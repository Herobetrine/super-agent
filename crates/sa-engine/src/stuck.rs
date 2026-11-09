//! Bounded semantic loop detection. Only complete tool input/output pairs
//! count; absent fields, streaming deltas and wait/poll tools never prove a loop.
//! Identical successful or unknown results can still make external progress:
//! they may warn, but only explicitly failed native results can stop a turn.
use crate::protocol::digest;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, PartialEq, Eq)]
pub enum Signal {
    Warn,
    Stop,
}
#[derive(Default)]
pub struct Detector {
    pending: HashMap<String, (String, Value)>,
    seen: HashSet<String>,
    recent: VecDeque<(String, bool)>,
    warned: bool,
}
impl Detector {
    fn start(&mut self, id: &str, name: &str, input: &Value) {
        if id.is_empty() || input.is_null() || self.pending.len() >= 128 {
            return;
        }
        let lower = name.to_ascii_lowercase();
        if ["wait", "sleep", "poll", "status", "taskoutput"]
            .iter()
            .any(|p| lower.contains(p))
        {
            self.recent.clear();
            self.warned = false;
            return;
        }
        let input_text = input.to_string().to_ascii_lowercase();
        if [
            "sleep ",
            "start-sleep",
            "git status",
            "wait_for",
            "wait-for",
        ]
        .iter()
        .any(|p| input_text.contains(p))
        {
            self.recent.clear();
            self.warned = false;
            return;
        }
        self.pending.insert(id.into(), (name.into(), input.clone()));
    }
    fn finish(&mut self, id: &str, output: &Value, failed: bool) -> Option<Signal> {
        let (name, input) = self.pending.remove(id)?;
        if output.is_null() || !self.seen.insert(id.into()) {
            return None;
        }
        if self.seen.len() > 512 {
            self.seen.clear();
            self.seen.insert(id.into());
        }
        self.recent.push_back((
            digest(&json!({"tool":name,"input":input,"output":output}).to_string()),
            failed,
        ));
        if self.recent.len() > 16 {
            self.recent.pop_front();
        }
        let n = self.recent.len();
        let repeat = |size: usize| {
            n >= size && (0..size).all(|i| self.recent[n - 1 - i].0 == self.recent[n - 1 - i % 2].0)
        };
        if repeat(12) && self.recent.iter().rev().take(12).all(|(_, failed)| *failed) {
            return Some(Signal::Stop);
        }
        if repeat(6) && !self.warned {
            self.warned = true;
            return Some(Signal::Warn);
        }
        if !repeat(4) {
            self.warned = false;
        }
        None
    }
    pub fn observe(&mut self, agent: &str, e: &Value) -> Option<Signal> {
        match agent {
            "deepseek" => {
                let id = e["callId"].as_str()?;
                if e["type"] == "tool_call" {
                    self.start(id, e["tool"].as_str()?, &e["input"]);
                }
                if e["type"] == "tool_result" {
                    return self.finish(id, &e["result"], e["status"] == "error");
                }
            }
            "codex" => {
                let item = &e["params"]["item"];
                let id = item["id"].as_str()?;
                let name = item["type"].as_str()?;
                if name == "commandExecution"
                    && item["command"].is_null()
                    && !self.pending.contains_key(id)
                {
                    return None;
                }
                if matches!(name, "mcpToolCall" | "dynamicToolCall")
                    && item["arguments"].is_null()
                    && !self.pending.contains_key(id)
                {
                    return None;
                }
                let input = match name {
                    "commandExecution" => json!({"command":item["command"],"cwd":item["cwd"]}),
                    "mcpToolCall" | "dynamicToolCall" => {
                        json!({"server":item["server"],"tool":item["tool"],"arguments":item["arguments"]})
                    }
                    _ => return None,
                };
                let tool_name = if name == "commandExecution" {
                    name
                } else {
                    item["tool"].as_str().unwrap_or(name)
                };
                if e["method"] == "item/started" {
                    self.start(id, tool_name, &input);
                }
                if e["method"] == "item/completed" {
                    // Complete events can include all fields without a start.
                    if !self.pending.contains_key(id) {
                        self.start(id, tool_name, &input);
                    }
                    let result = if name == "commandExecution" {
                        item.get("aggregatedOutput")
                            .filter(|v| !v.is_null())
                            .map(|v| json!({"output":v,"exit":item["exitCode"]}))
                    } else {
                        item.get("result").or_else(|| item.get("error")).cloned()
                    };
                    let failed = if name == "commandExecution" {
                        item["exitCode"].as_i64().is_some_and(|n| n != 0)
                    } else {
                        item.get("error").is_some_and(|v| !v.is_null())
                            || item["result"]["isError"] == true
                            || item["success"] == false
                            || item["status"] == "failed"
                    };
                    return result.and_then(|v| self.finish(id, &v, failed));
                }
            }
            "claude" => {
                let mut signal = None;
                for b in e["message"]["content"].as_array().into_iter().flatten() {
                    if b["type"] == "tool_use"
                        && let (Some(id), Some(name)) = (b["id"].as_str(), b["name"].as_str())
                    {
                        self.start(id, name, &b["input"]);
                    }
                    if b["type"] == "tool_result"
                        && let Some(id) = b["tool_use_id"].as_str()
                    {
                        let s = self.finish(id, &b["content"], b["is_error"] == true);
                        if s.is_some() {
                            signal = s;
                        }
                    }
                }
                return signal;
            }
            "acp" => {
                let u = &e["params"]["update"];
                let id = u["toolCallId"].as_str()?;
                if u["sessionUpdate"] == "tool_call" {
                    self.start(id, u["title"].as_str()?, &u["rawInput"]);
                }
                if matches!(u["status"].as_str(), Some("completed" | "failed")) {
                    return self.finish(id, &u["rawOutput"], u["status"] == "failed");
                }
            }
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_native_failure_is_required_across_all_supported_harnesses() {
        for agent in ["deepseek", "claude", "acp", "codex"] {
            for failed in [false, true] {
                let mut detector = Detector::default();
                let mut stopped = false;
                for i in 0..12 {
                    let id = i.to_string();
                    let (start, end) = match agent {
                        "deepseek" => (
                            json!({"type":"tool_call","callId":id,"tool":"build","input":{"target":"same"}}),
                            json!({"type":"tool_result","callId":id,"status":if failed{"error"}else{"completed"},"result":"error is just text"}),
                        ),
                        "claude" => (
                            json!({"message":{"content":[{"type":"tool_use","id":id,"name":"build","input":{"target":"same"}}]}}),
                            json!({"message":{"content":[{"type":"tool_result","tool_use_id":id,"is_error":failed,"content":"error is just text"}]}}),
                        ),
                        "acp" => (
                            json!({"params":{"update":{"sessionUpdate":"tool_call","toolCallId":id,"title":"build","rawInput":{"target":"same"}}}}),
                            json!({"params":{"update":{"sessionUpdate":"tool_call_update","toolCallId":id,"status":if failed{"failed"}else{"completed"},"rawOutput":"error is just text"}}}),
                        ),
                        _ => (
                            json!({"method":"item/started","params":{"item":{"id":id,"type":"mcpToolCall","tool":"build","server":"fixture","arguments":{"target":"same"}}}}),
                            json!({"method":"item/completed","params":{"item":{"id":id,"type":"mcpToolCall","result":{"isError":failed,"content":"error is just text"}}}}),
                        ),
                    };
                    detector.observe(agent, &start);
                    stopped |= detector.observe(agent, &end) == Some(Signal::Stop);
                }
                assert_eq!(stopped, failed, "{agent}: failed={failed}");
            }
        }
        let mut unknown = Detector::default();
        for i in 0..24 {
            unknown.observe(
                "deepseek",
                &json!({"type":"tool_call","callId":i.to_string(),"tool":"build","input":{}}),
            );
            assert_ne!(unknown.observe("deepseek", &json!({"type":"tool_result","callId":i.to_string(),"result":{"error":"looks like failure but native status is absent"}})),Some(Signal::Stop));
        }
    }

    #[test]
    fn successful_result_breaks_the_automatic_stop_failure_streak() {
        let mut detector = Detector::default();
        for i in 0..24 {
            detector.start(&i.to_string(), "same", &json!({}));
            assert_ne!(
                detector.finish(&i.to_string(), &json!("same"), i % 5 != 0),
                Some(Signal::Stop)
            );
        }
    }
    #[test]
    fn repeated_errors_warn_then_stop_but_new_results_and_polling_do_not() {
        let mut d = Detector::default();
        let mut signals = vec![];
        for i in 0..12 {
            assert!(d.observe("deepseek",&json!({"type":"tool_call","callId":i.to_string(),"tool":"pwsh","input":{"cmd":"compile"}})).is_none());
            if let Some(s)=d.observe("deepseek",&json!({"type":"tool_result","callId":i.to_string(),"status":"error","result":{"error":"unchanged"}})){signals.push(s);}
        }
        assert_eq!(signals, vec![Signal::Warn, Signal::Stop]);
        let mut d = Detector::default();
        for i in 0..24 {
            d.observe("deepseek",&json!({"type":"tool_call","callId":i.to_string(),"tool":"read","input":{"path":"x"}}));
            assert!(
                d.observe(
                    "deepseek",
                    &json!({"type":"tool_result","callId":i.to_string(),"result":i})
                )
                .is_none()
            );
            d.start(&format!("wait-{i}"), "wait_for_task", &json!({"task":1}));
            assert!(
                d.finish(&format!("wait-{i}"), &json!("pending"), false)
                    .is_none()
            );
        }
    }
    #[test]
    fn duplicate_terminal_events_are_not_additional_attempts() {
        let mut d = Detector::default();
        let event = json!({"method":"item/completed","params":{"item":{"id":"one","type":"commandExecution","command":"test","aggregatedOutput":"error","exitCode":1}}});
        for _ in 0..30 {
            assert!(d.observe("codex", &event).is_none());
        }
        assert_eq!(d.recent.len(), 1);
    }
}
