//! User-visible wording shared by the views. Ported from `desktop/ui/common.js`
//! and `desktop/ui/activity.js` so the native observer speaks the same language
//! as the existing workbench.

use chrono::{DateTime, Local, TimeZone};

/// Display name of an agent id.
pub fn agent_name(agent: &str) -> String {
    match agent {
        "codex" => "Codex".into(),
        "claude" => "Claude Code".into(),
        "deepseek" => "DeepSeek".into(),
        "antigravity" => "Antigravity".into(),
        "acp" => "ACP 执行端".into(),
        "" => "执行者".into(),
        other => other.into(),
    }
}

/// Failure class of the resilient runner (`ErrorClass`, snake_case).
pub fn error_class(class: &str) -> String {
    match class {
        "network" => "网络中断".into(),
        "rate_limited" => "触发限流".into(),
        "overloaded" => "上游过载".into(),
        "stalled" => "长时间无输出".into(),
        "process_crashed" => "执行端异常退出".into(),
        "route_unavailable" => "供应商或模型不可用".into(),
        "auth" => "认证失败".into(),
        "billing" => "额度或计费问题".into(),
        "quota_exhausted" => "额度已用尽".into(),
        "quota_window" => "等待额度重置".into(),
        "quota_reserve" => "为主控保留额度".into(),
        "permission" => "权限不足".into(),
        "protocol" => "协议错误".into(),
        "input_too_large" => "输入过长".into(),
        "session_mismatch" => "会话不匹配".into(),
        "cancelled" => "已取消".into(),
        "" => "未知原因".into(),
        other => other.into(),
    }
}

/// Why a quota-held turn is waiting, with its local continuation time when known.
pub fn quota_wait(reason: &str, until: Option<f64>) -> Option<String> {
    let (label, suffix) = match reason {
        "quota_window" => ("等待额度重置", "继续"),
        "quota_reserve" => ("为主控保留额度", "后继续"),
        _ => return None,
    };
    let time = until.map(stamp).unwrap_or_default();
    Some(if time.is_empty() {
        label.into()
    } else {
        format!("{label}，预计 {time} {suffix}")
    })
}

/// `provider / model` of a journaled route object.
pub fn route_label(route: &serde_json::Value) -> String {
    let get = |k: &str| route.get(k).and_then(serde_json::Value::as_str).unwrap_or_default();
    match (get("provider"), get("model")) {
        ("", "") => "未知路由".into(),
        (p, "") => p.into(),
        (p, m) => format!("{p} / {m}"),
    }
}

/// Two-letter monogram drawn inside the agent avatar.
pub fn agent_monogram(agent: &str) -> &'static str {
    match agent {
        "codex" => "CX",
        "claude" => "CC",
        "deepseek" => "DS",
        "antigravity" => "AG",
        "acp" => "ACP",
        _ => "·",
    }
}

/// Job / ticket states (`StudioUI.states`).
pub fn job_state(state: &str) -> String {
    match state {
        "queued" => "等待执行",
        "running" => "执行中",
        "waiting" => "等待协作",
        "draining" => "收集回报",
        "done" => "已完成",
        "failed" => "失败",
        "partial" => "部分完成",
        "cancelled" => "已停止",
        "interrupted" => "已中断",
        "" => "状态未知",
        other => return other.to_string(),
    }
    .into()
}

/// Progress row / native task states (`StudioActivity.states`).
pub fn activity_state(state: &str) -> String {
    match state {
        "running" => "执行中",
        "started" => "已开始",
        "done" | "completed" => "已完成",
        "success" => "成功",
        "failed" | "error" => "失败",
        "stopped" | "cancelled" => "已停止",
        "queued" => "等待执行",
        "waiting" => "等待中",
        "interrupted" => "已中断",
        "reported" => "已汇报",
        "unknown" | "" => "状态未知",
        other => return compact(other, 40),
    }
    .into()
}

/// Permission profiles (`StudioUI.profiles`).
pub fn profile(p: &str) -> String {
    match p {
        "readonly" => "只读",
        "ask" => "寻求审批",
        "partial" => "部分审批",
        "full" => "完全访问 · 高危",
        "auto" => "自动模式 · 高危",
        "" => "沿用工单",
        other => return other.to_string(),
    }
    .into()
}

/// States that mean the scheduler is still working on the job.
pub fn is_active(state: &str) -> bool {
    matches!(state, "queued" | "running" | "waiting" | "draining")
}

/// Labels for the latest-event line (`eventLabels` in studio.js).
pub fn event_label(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "delta" => "收到文本输出",
        "reply" => "收到回报",
        "send" => "分派任务",
        "role" => "记录分工",
        "approval.request" => "请求审批",
        "approval.decision" => "审批已处理",
        "mcp.result" => "收到 MCP 工具回执",
        "failed" => "任务失败",
        "interrupted" => "任务中断",
        "auto.retry" => "安排自动重试",
        "auto.backoff" => "等待重试",
        "auto.milestone" => "收到阶段回报",
        "ask" => "向你提问",
        "answer" => "你的回答已送达",
        "ask.withdrawn" => "问题已撤回",
        "report" => "已提交任务结论",
        "continued" => "续派任务",
        "skipped" => "前置任务未完成，未执行",
        "check.started" => "正在运行完成检查",
        "check.result" => "完成检查已结束",
        "check.bounced" => "检查未通过，已退回修复",
        "delivery.auto" => "已自动核对需求交付证据",
        "auto.stalled" => "连续给出相同的下一步，已暂停等待你决定",
        _ => return None,
    })
}

/// Agent-declared outcome wording, kept distinct from platform evidence.
pub fn outcome_status(status: &str) -> &'static str {
    match status {
        "done" => "已完成",
        "partial" => "部分完成",
        "blocked" => "受阻",
        _ => "结果",
    }
}

pub fn argv(payload: &serde_json::Value) -> String {
    payload.get("argv").and_then(serde_json::Value::as_array)
        .into_iter().flatten().filter_map(serde_json::Value::as_str)
        .collect::<Vec<_>>().join(" ")
}

/// One platform-owned check result; a cached pass has no new command run.
pub fn check_result(payload: &serde_json::Value) -> String {
    if payload.get("cached").and_then(serde_json::Value::as_bool) == Some(true) {
        return "平台检查通过（文件未变，沿用上次结果）".into();
    }
    let command = argv(payload);
    if payload.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
        let seconds = payload.get("seconds").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
        format!("平台检查通过 · {command} · {seconds:.1} 秒")
    } else {
        format!("平台检查未通过 · {command}")
    }
}

pub fn check_lines(payload: &serde_json::Value) -> Vec<(String, bool)> {
    let mut lines = if payload.get("cached").and_then(serde_json::Value::as_bool) == Some(true) {
        vec![(check_result(payload), true)]
    } else {
        payload.get("runs").and_then(serde_json::Value::as_array).into_iter().flatten()
            .map(|run| (check_result(run), run.get("ok").and_then(serde_json::Value::as_bool) == Some(true)))
            .collect()
    };
    if let Some(delivery) = payload.get("delivery") {
        lines.push((delivery_result(delivery), delivery.get("status").and_then(serde_json::Value::as_str) == Some("ready")));
    }
    lines
}

/// Automatic snapshot evidence for operator-adopted requirements. Isolated
/// work is described as not yet in the project, never as delivered.
pub fn delivery_result(payload: &serde_json::Value) -> String {
    let status = match payload.get("status").and_then(serde_json::Value::as_str) {
        Some("ready") => "全部必需项通过",
        Some("partial") => "部分必需项通过",
        Some("blocked") => "必需项未通过",
        _ => "结果未知",
    };
    let mut text = format!("需求交付证据：{status}");
    if let Some(snapshot) = payload.get("snapshot_id").and_then(serde_json::Value::as_i64) {
        text.push_str(&format!(" · 快照 {snapshot}"));
    }
    if payload.get("isolated").and_then(serde_json::Value::as_bool) == Some(true) {
        text.push_str(" · 隔离工作区，尚未合入");
    }
    if payload.get("gate").and_then(serde_json::Value::as_bool) != Some(true) {
        text.push_str(" · 仅记录");
    }
    if !matches!(payload.get("status").and_then(serde_json::Value::as_str), Some("ready" | "partial" | "blocked"))
        && let Some(reason) = payload.get("reason").and_then(serde_json::Value::as_str)
    {
        text.push_str(&format!(" · {}", compact(reason, 60)));
    }
    text
}

/// Payload-aware journal labels also supply the running check's activity.
pub fn collaboration_event(kind: &str, payload: &serde_json::Value) -> Option<String> {
    Some(match kind {
        "ask" => "向你提问".into(),
        "answer" => "你的回答已送达".into(),
        "ask.withdrawn" => "问题已撤回".into(),
        "continued" => format!("续派给 #{}", payload.get("job")?),
        "skipped" => "前置任务未完成，未执行".into(),
        "check.started" => format!("正在运行完成检查：{}", argv(payload)),
        "check.result" => check_result(payload),
        "check.bounced" => format!("检查未通过，已退回修复（第 {} 次）", payload.get("attempt")?),
        "delivery.auto" => delivery_result(payload),
        "auto.stalled" => "连续给出相同的下一步，已暂停等待你决定".into(),
        _ => return None,
    })
}

/// Collapse whitespace and cut to `n` characters with an ellipsis.
pub fn compact(s: &str, n: usize) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() > n {
        let mut out: String = joined.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    } else {
        joined
    }
}

/// Last path component of a project folder.
pub fn basename(path: &str) -> String {
    path.split(['\\', '/'])
        .rfind(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "无项目".into())
}

fn local(ts: f64) -> Option<DateTime<Local>> {
    if !ts.is_finite() || ts <= 0.0 {
        return None;
    }
    let secs = ts.floor() as i64;
    let nanos = ((ts - ts.floor()) * 1e9) as u32;
    Local.timestamp_opt(secs, nanos).single()
}

/// `HH:MM` (studio.js `stamp`).
pub fn stamp(ts: f64) -> String {
    local(ts)
        .map(|d| d.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// Local reset time including the date for windows lasting more than one day.
pub fn reset_time(ts: f64) -> String {
    local(ts)
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// `HH:MM:SS`, prefixed with the date when it is not today (`activityTime`).
pub fn activity_time(ts: f64) -> String {
    let Some(d) = local(ts) else {
        return String::new();
    };
    if d.date_naive() == Local::now().date_naive() {
        d.format("%H:%M:%S").to_string()
    } else {
        d.format("%Y/%-m/%-d %H:%M:%S").to_string()
    }
}

pub fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Relative time for the sidebar ("3 分钟前").
pub fn relative(ts: f64) -> String {
    let Some(d) = local(ts) else {
        return String::new();
    };
    let delta = now_secs() - ts;
    if delta < 60.0 {
        "刚刚".into()
    } else if delta < 3600.0 {
        format!("{} 分钟前", (delta / 60.0) as i64)
    } else if delta < 86400.0 {
        format!("{} 小时前", (delta / 3600.0) as i64)
    } else if delta < 7.0 * 86400.0 {
        format!("{} 天前", (delta / 86400.0) as i64)
    } else if d.date_naive().format("%Y").to_string() == Local::now().format("%Y").to_string() {
        d.format("%-m月%-d日").to_string()
    } else {
        d.format("%Y/%-m/%-d").to_string()
    }
}

/// Elapsed duration such as "1 小时 12 分" or "4 分 05 秒".
pub fn duration(secs: f64) -> String {
    let s = secs.max(0.0) as i64;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h} 小时 {m:02} 分")
    } else if m > 0 {
        format!("{m} 分 {sec:02} 秒")
    } else {
        format!("{sec} 秒")
    }
}

/// "12 秒前" style age for live activity.
pub fn age(ts: f64) -> String {
    if local(ts).is_none() {
        return String::new();
    }
    let d = (now_secs() - ts).max(0.0);
    if d < 5.0 {
        "刚刚".into()
    } else {
        format!("{}前", duration(d))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collaboration_events_and_platform_evidence_have_operator_labels() {
        use serde_json::json;
        let cases = [
            ("ask", json!({}), "向你提问"),
            ("answer", json!({}), "你的回答已送达"),
            ("ask.withdrawn", json!({}), "问题已撤回"),
            ("continued", json!({"job":12}), "续派给 #12"),
            ("skipped", json!({}), "前置任务未完成，未执行"),
            ("check.started", json!({"argv":["cargo","test"]}), "正在运行完成检查：cargo test"),
            ("check.result", json!({"argv":["cargo","test"],"ok":true,"seconds":1.5}), "平台检查通过 · cargo test · 1.5 秒"),
            ("check.bounced", json!({"attempt":2}), "检查未通过，已退回修复（第 2 次）"),
            ("delivery.auto", json!({"status":"ready","snapshot_id":4,"gate":true}), "需求交付证据：全部必需项通过 · 快照 4"),
            ("auto.stalled", json!({}), "连续给出相同的下一步，已暂停等待你决定"),
        ];
        for (kind, payload, expected) in cases {
            assert!(event_label(kind).is_some(), "{kind}");
            assert_eq!(collaboration_event(kind, &payload).as_deref(), Some(expected));
        }
        assert_eq!(check_result(&json!({"argv":["cargo","test"],"ok":false})), "平台检查未通过 · cargo test");
        assert_eq!(check_lines(&json!({"ok":true,"cached":true,"runs":[]})), vec![("平台检查通过（文件未变，沿用上次结果）".into(), true)]);
        assert_eq!(outcome_status("blocked"), "受阻");
        let receipt = json!({"ok":false,"runs":[{"argv":["cargo","test"],"ok":true,"seconds":1.0}],
            "delivery":{"status":"blocked","snapshot_id":9,"isolated":true,"gate":false}});
        assert_eq!(check_lines(&receipt)[1], ("需求交付证据：必需项未通过 · 快照 9 · 隔离工作区，尚未合入 · 仅记录".into(), false));
        assert_eq!(delivery_result(&json!({"status":"unknown","reason":"任务目录不在工单项目内"})),
            "需求交付证据：结果未知 · 仅记录 · 任务目录不在工单项目内");
    }

    #[test]
    fn compact_cuts_and_collapses() {
        assert_eq!(compact("a  b\n c", 10), "a b c");
        assert_eq!(compact("一二三四五六", 4), "一二三…");
    }

    #[test]
    fn quota_wait_uses_local_stamp_and_preserves_unknown_time() {
        let until = 1_800_000_000.0;
        assert_eq!(
            quota_wait("quota_window", Some(until)).unwrap(),
            format!("等待额度重置，预计 {} 继续", stamp(until))
        );
        assert_eq!(
            quota_wait("quota_reserve", Some(until)).unwrap(),
            format!("为主控保留额度，预计 {} 后继续", stamp(until))
        );
        assert_eq!(quota_wait("quota_window", None).as_deref(), Some("等待额度重置"));
        assert_eq!(quota_wait("quota_reserve", Some(f64::NAN)).as_deref(), Some("为主控保留额度"));
        assert_eq!(quota_wait("circuit_open", Some(until)), None);
    }

    #[test]
    fn basename_handles_both_separators() {
        assert_eq!(basename("E:\\Super Agent\\"), "Super Agent");
        assert_eq!(basename("/home/x/proj"), "proj");
        assert_eq!(basename(""), "无项目");
    }

    #[test]
    fn duration_formats() {
        assert_eq!(duration(5.0), "5 秒");
        assert_eq!(duration(65.0), "1 分 05 秒");
        assert_eq!(duration(3720.0), "1 小时 02 分");
    }
}
