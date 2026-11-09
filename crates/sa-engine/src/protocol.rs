//! Pure protocol helpers (port of `runtime/engine-protocol.cjs`).
//!
//! JavaScript semantics that matter for parity:
//! - string lengths are UTF-16 code units (`String.prototype.length`);
//! - `trim()` uses the ECMAScript WhiteSpace + LineTerminator set;
//! - `Number.isSafeInteger` accepts integral floats such as `5.0`.

use crate::adapter::{AGENTS, Route};
use regex::Regex;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::LazyLock};

pub const PROFILES: [&str; 5] = ["readonly", "ask", "partial", "full", "auto"];
pub const TERMINAL: [&str; 4] = ["done", "partial", "failed", "cancelled"];
/// SQL list of terminal states (`endSQL`, S:7).
pub const END_SQL: &str = "('done','partial','failed','cancelled')";

pub fn is_full(profile: &str) -> bool {
    profile == "full" || profile == "auto"
}

pub fn can_write(agent: &str, profile: &str) -> bool {
    // ACP performs capability/mode and trusted-sandbox checks before execution.
    is_full(profile) || matches!(agent,"codex"|"deepseek"|"acp")
}

pub fn is_terminal(state: &str) -> bool {
    TERMINAL.contains(&state)
}

/// `String.prototype.length`.
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// ECMAScript WhiteSpace and LineTerminator code points (what `trim()` strips).
pub fn js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{9}'
            | '\u{a}'
            | '\u{b}'
            | '\u{c}'
            | '\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// `validText` on a known string.
pub fn valid_str(s: &str, max: usize) -> bool {
    !s.chars().all(js_whitespace) && utf16_len(s) <= max
}

/// `validText(x, max)`: a string with non-blank `trim()` and length ≤ max.
pub fn valid_text(v: Option<&Value>, max: usize) -> bool {
    matches!(v, Some(Value::String(s)) if valid_str(s, max))
}

static IDENT_BAD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\p{C}\p{Zl}\p{Zp}]").expect("ident regex"));

pub fn ident_str(s: &str) -> bool {
    valid_str(s, 256) && !IDENT_BAD.is_match(s)
}

/// `ident(x)`.
pub fn ident(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::String(s)) if ident_str(s))
}

/// Lowercase hex SHA-256 of the UTF-8 bytes.
pub fn digest(text: &str) -> String {
    let mut out = String::with_capacity(64);
    for b in Sha256::digest(text.as_bytes()) {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// `childProfiles(p)`.
pub fn child_profiles(p: &str) -> Vec<&'static str> {
    match PROFILES.iter().position(|x| *x == p) {
        Some(i) => PROFILES[..=i].to_vec(),
        None => vec!["readonly"],
    }
}

/// `isObj(x)`.
pub fn is_obj(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Object(_)))
}

/// JavaScript truthiness of an optional JSON value.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// JavaScript truthiness of a JSON value.
pub fn truthy_value(v: &Value) -> bool {
    truthy(Some(v))
}

const MAX_SAFE: f64 = 9_007_199_254_740_991.0;

/// `Number.isSafeInteger(x)` returning the integer.
pub fn safe_int(v: Option<&Value>) -> Option<i64> {
    let n = match v {
        Some(Value::Number(n)) => n,
        _ => return None,
    };
    if let Some(i) = n.as_i64() {
        return (i.unsigned_abs() as f64 <= MAX_SAFE).then_some(i);
    }
    let f = n.as_f64()?;
    (f.fract() == 0.0 && f.abs() <= MAX_SAFE).then_some(f as i64)
}

/// JSON value of a route in JS key order `{provider, model, mode}`.
pub fn route_value(r: &Route) -> Value {
    let mut value = json!({"provider": r.provider, "model": r.model, "mode": r.mode});
    if let Some(effort) = &r.reasoning_effort {
        value["reasoning_effort"] = json!(effort);
    }
    value
}

/// `route(x)` (PR:16-24).
pub fn route(v: Option<&Value>) -> Result<Route, String> {
    let bad = || "无效的模型、供应商或执行端".to_string();
    let Some(Value::Object(o)) = v else {
        return Err(bad());
    };
    let mode = o.get("mode").and_then(Value::as_str);
    if o.keys().any(|key| !matches!(key.as_str(), "provider" | "model" | "mode" | "reasoning_effort"))
        || !ident(o.get("provider"))
        || !ident(o.get("model"))
        || !matches!(mode, Some("cli" | "desktop"))
        || (o.contains_key("reasoning_effort") && !ident(o.get("reasoning_effort")))
    {
        return Err(bad());
    }
    Ok(Route {
        provider: o["provider"].as_str().unwrap_or_default().to_string(),
        model: o["model"].as_str().unwrap_or_default().to_string(),
        mode: mode.unwrap_or_default().to_string(),
        reasoning_effort: o.get("reasoning_effort").and_then(Value::as_str).map(str::to_string),
    })
}

/// `routes(obj)` (PR:26-31): validated per-agent routes, input key order.
pub fn routes(v: &Value) -> Result<Map<String, Value>, String> {
    let Value::Object(o) = v else {
        return Err("无效的工单模型配置".into());
    };
    if o.keys().any(|k| !AGENTS.contains(&k.as_str())) {
        return Err("无效的工单模型配置".into());
    }
    let mut out = Map::new();
    for (k, r) in o {
        out.insert(k.clone(), route_value(&route(Some(r))?));
    }
    Ok(out)
}

fn match_row<'a>(rows: Option<&'a Value>, id: &str) -> Option<&'a Map<String, Value>> {
    let rows = rows.and_then(Value::as_array)?;
    let mut found = rows
        .iter()
        .filter_map(Value::as_object)
        .filter(|r| r.get("id").and_then(Value::as_str) == Some(id));
    let first = found.next()?;
    found.next().is_none().then_some(first)
}

/// `available === undefined || available === true`.
fn not_unavailable(o: &Map<String, Value>) -> bool {
    matches!(o.get("available"), None | Some(Value::Bool(true)))
}

/// `options(cat, picked)` (PR:33-66). `Err` mirrors `route()` throwing.
pub fn options(cat: Option<&Value>, picked: Option<&Value>) -> Result<Option<Value>, String> {
    let Some(Value::Object(cat)) = cat else {
        return Ok(None);
    };
    if !not_unavailable(cat) {
        return Ok(None);
    }
    let selected = route(picked)?;
    let mode = match_row(cat.get("modes"), &selected.mode);
    let source = match_row(cat.get("providers"), &selected.provider);
    let Some(mode) = mode else { return Ok(None) };
    if mode.get("available") != Some(&Value::Bool(true)) {
        return Ok(None);
    }
    if truthy(mode.get("providers")) {
        let allowed = mode
            .get("providers")
            .and_then(Value::as_array)
            .is_some_and(|a| a.iter().any(|p| p.as_str() == Some(&selected.provider)));
        if !allowed {
            return Ok(None);
        }
    }
    let Some(source) = source else { return Ok(None) };
    if !not_unavailable(source) {
        return Ok(None);
    }
    let mut ids: Vec<(String, bool)> = vec![];
    let mut positions = HashMap::new();
    let mut efforts = Map::new();
    let mut defaults = Map::new();
    for m in source.get("models").and_then(Value::as_array).into_iter().flatten() {
        let Some(m) = m.as_object() else { continue };
        if !ident(m.get("id")) {
            continue;
        }
        let id = m["id"].as_str().unwrap_or_default().to_string();
        let ok = not_unavailable(m);
        match positions.get(&id).copied() {
            Some(index) => {
                // Conflicting duplicate entries cannot grant an effort.
                let entry: &mut (String, bool) = &mut ids[index];
                entry.1 = entry.1 && ok;
                efforts.remove(&id);
                defaults.remove(&id);
            }
            None => {
                positions.insert(id.clone(), ids.len());
                ids.push((id.clone(), ok));
                if let Some(values) = m.get("reasoningEfforts").and_then(Value::as_array)
                    && values.iter().all(|value| ident(Some(value)))
                {
                    efforts.insert(id.clone(), json!(values));
                    if let Some(default) = m.get("defaultReasoningEffort")
                        && values.contains(default)
                    {
                        defaults.insert(id, default.clone());
                    }
                }
            }
        }
    }
    if !ids.iter().any(|(k, ok)| *k == selected.model && *ok) {
        return Ok(None);
    }
    if let Some(effort) = &selected.reasoning_effort
        && !efforts.get(&selected.model).and_then(Value::as_array)
            .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(effort)))
    {
        return Err("所选模型未声明支持该思考强度，请刷新模型目录或重新选择".into());
    }
    efforts.retain(|id, _| ids[positions[id]].1);
    defaults.retain(|id, _| ids[positions[id]].1);
    let mut models = vec![Value::String(selected.model.clone())];
    models.extend(
        ids.into_iter()
            .filter(|(id, ok)| *ok && *id != selected.model)
            .map(|(id, _)| Value::String(id)),
    );
    let mut menu = json!({
        "provider": selected.provider,
        "mode": selected.mode,
        "default_model": selected.model,
        "models": models,
    });
    if !efforts.is_empty() {
        menu["reasoning_efforts"] = json!(efforts);
    }
    if !defaults.is_empty() {
        menu["default_reasoning_efforts"] = json!(defaults);
    }
    Ok(Some(menu))
}

/// `toolsProof(e)` (PR:68-89): `Some("ok" | "attempt")` for tool evidence.
pub fn tools_proof(e: &Value) -> Option<&'static str> {
    let o = e.as_object()?;
    let s = |k: &str| o.get(k).and_then(Value::as_str);
    if s("type") == Some("tool_result") && truthy(o.get("callId")) {
        return Some(if matches!(s("status"), Some("ok" | "success" | "completed")) {
            "ok"
        } else {
            "attempt"
        });
    }
    if s("type") == Some("user")
        && let Some(content) = o.get("message").and_then(|m| m.get("content")).and_then(Value::as_array)
    {
        let blocks: Vec<&Value> = content
            .iter()
            .filter(|b| {
                b.get("type").and_then(Value::as_str) == Some("tool_result") && truthy(b.get("tool_use_id"))
            })
            .collect();
        if !blocks.is_empty() {
            return Some(if blocks.iter().any(|b| !truthy(b.get("is_error"))) {
                "ok"
            } else {
                "attempt"
            });
        }
    }
    if s("method") == Some("item/completed") {
        let item = o.get("params").and_then(|p| p.get("item"));
        let kind = item.and_then(|i| i.get("type")).and_then(Value::as_str);
        if matches!(
            kind,
            Some("commandExecution" | "mcpToolCall" | "webSearch" | "dynamicToolCall")
        ) {
            let item = item.expect("item present");
            let exit = item.get("exitCode");
            let exit_ok = matches!(exit, None | Some(Value::Null))
                || exit.and_then(Value::as_f64) == Some(0.0);
            let done = item.get("status").and_then(Value::as_str) == Some("completed");
            return Some(if done && exit_ok { "ok" } else { "attempt" });
        }
    }
    let step = o.get("step_update");
    if s("event") == Some("step_update")
        && step.and_then(|x| x.get("step_type")).and_then(Value::as_str) == Some("tool")
    {
        let state = step.and_then(|x| x.get("state")).and_then(Value::as_str);
        if matches!(state, Some("DONE" | "ERROR" | "FAILED")) {
            return Some(if state == Some("DONE") { "ok" } else { "attempt" });
        }
    }
    None
}

fn parse_err(e: serde_json::Error) -> String {
    format!("JSON 解析失败：{e}")
}

/// `controls(text, continuous)` (PR:91-125). The map is keyed by tag.
pub fn controls(text: &str, continuous: bool) -> Result<Map<String, Value>, String> {
    let field = |tag: &str| -> Option<&'static str> {
        match tag {
            "SA_GOAL" => Some("objective"),
            "SA_NEXT" => Some("body"),
            "SA_FINISH" => Some("summary"),
            "SA_LEARN" if continuous => Some("lesson"),
            "SA_EXHAUSTED" if continuous => Some("summary"),
            _ => None,
        }
    };
    let mut out = Map::new();
    for line in text.split('\n') {
        let Some(space) = line.find(' ') else { continue };
        let tag = &line[..space];
        let Some(name) = field(tag) else { continue };
        let cmd: Value = serde_json::from_str(&line[space + 1..]).map_err(parse_err)?;
        if out.contains_key(tag) || !cmd.is_object() || !valid_text(cmd.get(name), 32000) {
            return Err(format!("{tag} 缺少有效且唯一的字段"));
        }
        if tag == "SA_EXHAUSTED" || tag == "SA_FINISH" {
            if !matches!(cmd.get("status").and_then(Value::as_str), Some("done" | "blocked")) {
                return Err("阶段状态无效".into());
            }
            let resolved_ok = match cmd.get("resolved") {
                None | Some(Value::Null) => true,
                Some(Value::Array(rows)) => rows
                    .iter()
                    .all(|r| safe_int(r.get("job")).is_some() && valid_text(r.get("evidence"), usize::MAX)),
                Some(_) => false,
            };
            if !resolved_ok {
                return Err("resolved 必须引用工单中的任务并提供核验依据".into());
            }
        }
        if tag == "SA_EXHAUSTED" {
            for k in ["checks", "alternatives"] {
                let ok = cmd.get(k).and_then(Value::as_array).is_some_and(|a| {
                    !a.is_empty() && a.len() <= 32 && a.iter().all(|v| valid_text(Some(v), 4000))
                });
                if !ok {
                    return Err("耗尽审查需提供 checks 和 alternatives".into());
                }
            }
        }
        out.insert(tag.to_string(), cmd);
    }
    let has = |k: &str| out.contains_key(k);
    if (has("SA_FINISH") && has("SA_NEXT"))
        || (has("SA_EXHAUSTED") && (has("SA_FINISH") || has("SA_GOAL") || has("SA_NEXT")))
    {
        return Err("同一轮的状态指令相互冲突".into());
    }
    Ok(out)
}

/// `unresolved(reports, cmd)` (PR:127-134). Rust port: a child replaced by
/// automatic failover (`replaced_by`) is resolved by its sibling's report.
pub fn unresolved(reports: &[Value], cmd: &Value) -> Vec<i64> {
    let resolved: Vec<i64> = cmd
        .get("resolved")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(|r| safe_int(r.get("job"))).collect())
        .unwrap_or_default();
    reports
        .iter()
        .filter(|r| {
            matches!(
                r.get("state").and_then(Value::as_str),
                Some("failed" | "partial" | "cancelled")
            ) && r.get("replaced_by").is_none()
        })
        .filter_map(|r| r.get("id").and_then(Value::as_i64))
        .filter(|id| !resolved.contains(id))
        .collect()
}

/// `JSON.stringify` of a value (insertion-ordered objects).
pub fn stringify(v: &Value) -> String {
    serde_json::to_string(v).expect("JSON values always serialize")
}

/// Seconds as a JSON number; whole seconds render as integers like JS.
pub fn seconds_json(secs: f64) -> Value {
    if secs.fract() == 0.0 && secs.abs() < MAX_SAFE {
        json!(secs as i64)
    } else {
        json!(secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_are_utf16() {
        assert_eq!(utf16_len("😀"), 2);
        assert!(valid_str(&"字".repeat(320), 320));
        assert!(!valid_str(&"字".repeat(321), 320));
        assert!(!valid_str(&"😀".repeat(161), 320));
        assert!(!valid_str("\u{feff} \u{3000}", 10));
        // NEL is not ECMAScript whitespace.
        assert!(valid_str("\u{85}", 10));
    }

    #[test]
    fn ident_rejects_controls() {
        assert!(ident_str("measure"));
        assert!(!ident_str("a\u{200b}b"));
        assert!(!ident_str("a\u{2028}"));
        assert!(!ident_str(""));
    }

    #[test]
    fn safe_integers() {
        assert_eq!(safe_int(Some(&json!(5.0))), Some(5));
        assert_eq!(safe_int(Some(&json!(5.5))), None);
        assert_eq!(safe_int(Some(&json!("5"))), None);
        assert_eq!(safe_int(Some(&json!(9_007_199_254_740_992_i64))), None);
    }

    #[test]
    fn options_menu() {
        let cat = json!({
            "modes": [{"id": "cli", "available": true}],
            "providers": [{"id": "p", "models": [{"id": "a"}, {"id": "b"}, {"id": "c", "available": false}]}]
        });
        let pick = json!({"provider": "p", "model": "b", "mode": "cli"});
        let menu = options(Some(&cat), Some(&pick)).unwrap().unwrap();
        assert_eq!(
            stringify(&menu),
            r#"{"provider":"p","mode":"cli","default_model":"b","models":["b","a"]}"#
        );
        let gone = json!({"provider": "p", "model": "c", "mode": "cli"});
        assert!(options(Some(&cat), Some(&gone)).unwrap().is_none());
        assert!(options(Some(&cat), None).is_err());
    }

    #[test]
    fn route_effort_roundtrip_and_model_specific_capabilities() {
        let legacy = json!({"provider":"p", "model":"a", "mode":"cli"});
        let old: Route = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(old.reasoning_effort, None);
        assert_eq!(serde_json::to_value(&old).unwrap(), legacy);
        let selected = json!({"provider":"p", "model":"b", "mode":"cli", "reasoning_effort":"high"});
        assert_eq!(route_value(&route(Some(&selected)).unwrap()), selected);
        let cat = json!({"modes":[{"id":"cli", "available":true}], "providers":[{"id":"p", "models":[
            {"id":"a"}, {"id":"b", "reasoningEfforts":["low","high"], "defaultReasoningEffort":"low"}
        ]}]});
        let menu = options(Some(&cat), Some(&selected)).unwrap().unwrap();
        assert_eq!(menu["reasoning_efforts"]["b"], json!(["low", "high"]));
        assert_eq!(menu["default_reasoning_efforts"]["b"], json!("low"));
        for (model, effort) in [("a", "high"), ("b", "max")] {
            let bad = json!({"provider":"p", "model":model, "mode":"cli", "reasoning_effort":effort});
            assert!(options(Some(&cat), Some(&bad)).unwrap_err().contains("思考强度"));
        }
        for value in [Value::Null, json!(42), json!(""), json!("bad\nvalue")] {
            let mut bad = selected.clone();
            bad["reasoning_effort"] = value;
            assert!(route(Some(&bad)).is_err());
        }
    }

    #[test]
    fn full_catalog_menu_keeps_late_models_and_efforts() {
        let models: Vec<Value> = (0..16_384).map(|n| json!({"id":format!("m-{n}"), "reasoningEfforts":["low","high"]})).collect();
        let cat = json!({"modes":[{"id":"cli", "available":true}], "providers":[{"id":"p", "models":models}]});
        let picked = json!({"provider":"p", "model":"m-16383", "mode":"cli", "reasoning_effort":"high"});
        let menu = options(Some(&cat), Some(&picked)).unwrap().unwrap();
        assert_eq!(menu["models"].as_array().unwrap().len(), 16_384);
        assert_eq!(menu["models"][0], json!("m-16383"));
        assert_eq!(menu["reasoning_efforts"]["m-16383"], json!(["low", "high"]));
    }

    #[test]
    fn control_conflicts() {
        let t = "SA_FINISH {\"status\":\"done\",\"summary\":\"x\"}\nSA_NEXT {\"body\":\"y\"}";
        assert_eq!(controls(t, false).unwrap_err(), "同一轮的状态指令相互冲突");
        assert!(controls("SA_LEARN {\"lesson\":\"x\"}", false).unwrap().is_empty());
        assert!(controls("SA_LEARN {\"lesson\":\"x\"}", true).unwrap().contains_key("SA_LEARN"));
    }
}
