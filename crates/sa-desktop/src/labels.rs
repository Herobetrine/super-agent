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
        "permission" => "权限不足".into(),
        "protocol" => "协议错误".into(),
        "input_too_large" => "输入过长".into(),
        "session_mismatch" => "会话不匹配".into(),
        "cancelled" => "已取消".into(),
        "" => "未知原因".into(),
        other => other.into(),
    }
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
    fn compact_cuts_and_collapses() {
        assert_eq!(compact("a  b\n c", 10), "a b c");
        assert_eq!(compact("一二三四五六", 4), "一二三…");
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
