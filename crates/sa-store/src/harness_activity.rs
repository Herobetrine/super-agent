//! Port of `runtime/harness-activity.cjs`: Codex app-server, DeepSeek headless
//! and Antigravity event frames projected to constant labels.
//!
//! Only constants from these tables reach the UI. Tool arguments, results,
//! private/custom tool names, paths and model-generated text stay in the ledger.

use crate::activity::{Activity, get, get_str, is_true, truthy};
use serde_json::Value;

fn codex_tool(name: &str) -> Option<&'static str> {
    Some(match name {
        "commandExecution" => "命令",
        "fileChange" => "文件修改",
        "mcpToolCall" => "MCP 工具",
        "dynamicToolCall" => "工具",
        "collabAgentToolCall" => "协作工具",
        "webSearch" => "网络搜索",
        "imageView" => "查看图片",
        "imageGeneration" => "生成图片",
        _ => return None,
    })
}

fn deepseek_tool(name: &str) -> Option<&'static str> {
    Some(match name {
        "pwsh" => "PowerShell 命令",
        "bash" => "Bash 命令",
        "read" => "读取文件",
        "write" => "写入文件",
        "edit" => "编辑文件",
        "read_image" => "读取图片",
        "glob" => "查找文件",
        "grep" => "搜索内容",
        _ => return None,
    })
}

fn agy_tool(name: &str) -> Option<&'static str> {
    Some(match name {
        "read_file" => "读取文件",
        "write_file" => "写入文件",
        "command" => "命令",
        "read_url" => "读取网页",
        "mcp" => "MCP 工具",
        _ => return None,
    })
}

fn state_label(state: &str) -> &'static str {
    match state {
        "running" => "执行中",
        "completed" => "已完成",
        "failed" => "失败",
        "stopped" => "已停止",
        _ => "状态待确认",
    }
}

fn codex_end(status: Option<&str>) -> Option<&'static str> {
    match status? {
        "completed" => Some("completed"),
        "failed" => Some("failed"),
        "interrupted" | "declined" => Some("stopped"),
        _ => None,
    }
}

fn agy_end(status: &str) -> Option<&'static str> {
    match status {
        "SUCCESS" => Some("completed"),
        "ERROR" | "FAILED" => Some("failed"),
        _ => None,
    }
}

fn run(name: &str, state: &'static str) -> Activity {
    Activity::new(format!("{name} 轮次{}", state_label(state)), "run", state)
}

fn tool(name: &str, state: &'static str) -> Activity {
    Activity::new(format!("{name} · {}", state_label(state)), "tool", state).with_tool(name)
}

fn codex(e: &Value) -> Option<Activity> {
    // `const {turn, item} = e.params || {}`
    let params = get(Some(e), "params");
    let turn = get(params, "turn");
    let item = get(params, "item");
    let method = get_str(Some(e), "method");
    if method == Some("turn/started") && truthy(get(turn, "id")) {
        return Some(run("Codex", "running"));
    }
    if method == Some("turn/completed") && truthy(get(turn, "id")) {
        return Some(run(
            "Codex",
            codex_end(get_str(turn, "status")).unwrap_or("unknown"),
        ));
    }
    let name = codex_tool(get_str(item, "type")?)?;
    if method == Some("item/started") {
        return Some(tool(name, "running"));
    }
    if method != Some("item/completed") {
        return None;
    }
    let present = |k: &str| !matches!(get(item, k), None | Some(Value::Null));
    let exit_failed = get(item, "exitCode")
        .and_then(Value::as_f64)
        .is_some_and(|code| code != 0.0);
    if get_str(item, "status") == Some("failed")
        || matches!(get(item, "success"), Some(Value::Bool(false)))
        || present("error")
        || present("failure")
        || is_true(get(get(item, "result"), "isError"))
        || exit_failed
    {
        return Some(tool(name, "failed"));
    }
    // Search and image-view items have no native success field.
    Some(tool(
        name,
        codex_end(get_str(item, "status")).unwrap_or("unknown"),
    ))
}

fn deepseek(e: &Value) -> Option<Activity> {
    let ev = Some(e);
    let kind = get_str(ev, "type");
    if kind == Some("status") {
        let phase = get_str(ev, "phase");
        if phase == Some("turn_start") {
            return Some(run("DeepSeek", "running"));
        }
        if phase != Some("turn_end") {
            return None;
        }
        let state = match get_str(get(ev, "reason"), "kind") {
            Some("completed") => "completed",
            Some("error") => "failed",
            Some("blocked" | "max-tokens" | "aborted" | "interrupted") => "stopped",
            _ => "unknown",
        };
        return Some(run("DeepSeek", state));
    }
    if !get_str(ev, "callId").is_some_and(|id| !id.is_empty()) {
        return None;
    }
    if kind == Some("tool_call") {
        let name = get_str(ev, "tool")
            .and_then(deepseek_tool)
            .unwrap_or("工具");
        return Some(tool(name, "running"));
    }
    if kind != Some("tool_result") {
        return None;
    }
    // The pinned headless protocol omits the tool name on the result.
    Some(match get_str(ev, "status") {
        Some("completed") => tool("工具", "completed"),
        Some("error") => tool("工具", "failed"),
        _ => tool("工具", "unknown"),
    })
}

fn antigravity(e: &Value) -> Option<Activity> {
    let ev = Some(e);
    let event = get_str(ev, "event");
    if event == Some("init") && truthy(get(ev, "conversation_id")) {
        return Some(run("Antigravity", "running"));
    }
    if event == Some("result")
        && let Some(status) = get_str(get(ev, "result"), "status")
    {
        return Some(run("Antigravity", agy_end(status).unwrap_or("unknown")));
    }
    let step = get(ev, "step_update");
    if event != Some("step_update") || get_str(step, "step_type") != Some("tool") {
        return None;
    }
    let name = get_str(step, "tool_name")
        .and_then(agy_tool)
        .unwrap_or("工具");
    match get_str(step, "state")? {
        "ACTIVE" => Some(tool(name, "running")),
        "DONE" => Some(tool(name, "completed")),
        "ERROR" | "FAILED" => Some(tool(name, "failed")),
        _ => None,
    }
}

/// `harnessActivity(event)`
pub(crate) fn harness_activity(event: &Value) -> Option<Activity> {
    if !event.is_object() {
        return None;
    }
    if get_str(Some(event), "method").is_some() {
        return codex(event);
    }
    if get_str(Some(event), "event").is_some() {
        return antigravity(event);
    }
    deepseek(event)
}

#[cfg(test)]
mod tests {
    //! Port of `tests-node/harness-activity.test.cjs`.
    use super::*;
    use serde_json::json;

    const PRIVATE: &str = "PRIVATE-SENTINEL";

    fn check(event: Value, state: &str, category: &str, tool: Option<&str>) {
        let view = harness_activity(&event).unwrap_or_else(|| panic!("no activity for {event}"));
        assert_eq!(view.state, state, "{event}");
        assert_eq!(view.category, category);
        assert_eq!(view.tool.as_deref(), tool);
        assert!(!view.label.is_empty());
        let json = view.to_value();
        assert!(!json.to_string().contains(PRIVATE));
        let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        let expected: Vec<&str> = if tool.is_some() {
            vec!["category", "label", "state", "tool"]
        } else {
            vec!["category", "label", "state"]
        };
        assert_eq!(keys, expected);
    }

    #[test]
    fn deepseek_turn_reason_controls_outcome() {
        check(
            json!({"type":"status","phase":"turn_start","turn":1}),
            "running",
            "run",
            None,
        );
        for (kind, state) in [
            ("completed", "completed"),
            ("aborted", "stopped"),
            ("error", "failed"),
            ("future", "unknown"),
        ] {
            check(
                json!({"type":"status","phase":"turn_end","reason":{"kind":kind,"error":PRIVATE}}),
                state,
                "run",
                None,
            );
        }
        assert_eq!(
            harness_activity(&json!({"type":"final","text":PRIVATE})),
            None
        );
    }

    #[test]
    fn deepseek_tool_calls_expose_only_known_names() {
        for (tool, label) in [
            ("read", "读取文件"),
            ("pwsh", "PowerShell 命令"),
            (PRIVATE, "工具"),
        ] {
            check(
                json!({"type":"tool_call","callId":PRIVATE,"tool":tool,"input":{"path":PRIVATE}}),
                "running",
                "tool",
                Some(label),
            );
        }
        for (status, state) in [
            ("error", "failed"),
            ("completed", "completed"),
            ("future", "unknown"),
        ] {
            check(
                json!({"type":"tool_result","callId":PRIVATE,"status":status,"result":PRIVATE}),
                state,
                "tool",
                Some("工具"),
            );
        }
        assert_eq!(
            harness_activity(&json!({"type":"tool_call","tool":"read"})),
            None
        );
    }

    #[test]
    fn codex_turn_and_tool_outcomes() {
        let turn = |method: &str, status: &str| json!({"method":method,"params":{"turn":{"id":PRIVATE,"status":status,"error":PRIVATE}}});
        check(turn("turn/started", "inProgress"), "running", "run", None);
        for (status, state) in [
            ("interrupted", "stopped"),
            ("failed", "failed"),
            ("completed", "completed"),
        ] {
            check(turn("turn/completed", status), state, "run", None);
        }
        let item = |method: &str, item: Value| json!({"method":method,"params":{"item":item}});
        let cmd = json!({"type":"commandExecution","command":PRIVATE,"cwd":PRIVATE,"aggregatedOutput":PRIVATE});
        check(
            item("item/started", cmd.clone()),
            "running",
            "tool",
            Some("命令"),
        );
        for (exit_code, state) in [(1, "failed"), (0, "completed")] {
            let mut done = cmd.clone();
            done["status"] = json!("completed");
            done["exitCode"] = json!(exit_code);
            check(item("item/completed", done), state, "tool", Some("命令"));
        }
        check(
            item(
                "item/completed",
                json!({"type":"fileChange","status":"declined","changes":[PRIVATE]}),
            ),
            "stopped",
            "tool",
            Some("文件修改"),
        );
        check(
            item(
                "item/completed",
                json!({"type":"mcpToolCall","tool":PRIVATE,"status":"completed","result":{"isError":true}}),
            ),
            "failed",
            "tool",
            Some("MCP 工具"),
        );
        check(
            item(
                "item/completed",
                json!({"type":"dynamicToolCall","tool":PRIVATE,"status":"failed"}),
            ),
            "failed",
            "tool",
            Some("工具"),
        );
        check(
            item(
                "item/completed",
                json!({"type":"webSearch","query":PRIVATE}),
            ),
            "unknown",
            "tool",
            Some("网络搜索"),
        );
    }

    #[test]
    fn antigravity_tool_steps_and_results() {
        check(
            json!({"event":"init","conversation_id":PRIVATE}),
            "running",
            "run",
            None,
        );
        for (status, state) in [
            ("SUCCESS", "completed"),
            ("ERROR", "failed"),
            ("future", "unknown"),
        ] {
            check(
                json!({"event":"result","result":{"status":status,"response":PRIVATE}}),
                state,
                "run",
                None,
            );
        }
        for (mark, state) in [
            ("ACTIVE", "running"),
            ("DONE", "completed"),
            ("FAILED", "failed"),
        ] {
            let step = json!({"step_type":"tool","state":mark,"tool_name":"read_file","tool_info":{"output":PRIVATE}});
            check(
                json!({"event":"step_update","step_update":step}),
                state,
                "tool",
                Some("读取文件"),
            );
            let mut other = step.clone();
            other["tool_name"] = json!(PRIVATE);
            check(
                json!({"event":"step_update","step_update":other}),
                state,
                "tool",
                Some("工具"),
            );
        }
    }

    #[test]
    fn malformed_frames_never_become_activities() {
        for raw in [
            json!(null),
            json!([]),
            json!(3),
            json!({}),
            json!("ping"),
            json!({"type":"thinking"}),
            json!({"type":"text","text":PRIVATE}),
            json!({"event":"ping"}),
            json!({"type":"session","cwd":PRIVATE}),
            json!({"type":"status","phase":"step_end"}),
            json!({"type":"tool_result","callId":{}}),
            json!({"event":"result"}),
            json!({"method":"turn/completed","params":null}),
            json!({"method":"item/completed","params":{"item":{"type":{"toString":null}}}}),
            json!({"method":"item/agentMessage/delta","params":{"delta":PRIVATE}}),
            json!({"method":"item/completed","params":{"item":{"type":"reasoning","summary":PRIVATE}}}),
            json!({"event":"step_update","step_update":{"step_type":"agent_response","state":"DONE","text_delta":PRIVATE}}),
        ] {
            assert_eq!(harness_activity(&raw), None, "{raw}");
        }
    }
}
