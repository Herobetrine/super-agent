//! Turns journal events into conversation items. Port of `ingest` in
//! `desktop/ui/studio.js` plus `desktop/ui/activity.js`, with one addition:
//! SA_* control lines are folded into compact chips instead of being dropped.

use crate::labels::{activity_state, activity_time, agent_name, compact, event_label, job_state};
use crate::markdown::{self, Block};
use sa_store::{EventView, JobView, TicketDetail};
use serde_json::Value;
use std::collections::HashMap;

/// Progress rows per collapsed group (activity.js uses 24).
const GROUP_MAX: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlKind {
    Progress,
    Send,
    Note,
    Role,
    Tool,
    Goal,
    Next,
    Finish,
    Blocked,
    Learn,
    Exhausted,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Control {
    pub kind: ControlKind,
    pub title: String,
    pub text: String,
    /// Target agent for SA_SEND.
    pub agent: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProgressRow {
    pub seq: i64,
    pub ts: f64,
    pub label: String,
    pub agent: String,
    pub job: Option<i64>,
    pub turn: Option<i64>,
    pub state: String,
    pub tool: String,
    pub task_id: String,
    pub title: String,
    pub steps: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LineKind {
    Send,
    Note,
    Tool,
    Decision,
    Auto,
    Retry,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    User {
        key: i64,
        text: String,
        ts: f64,
        follow: bool,
    },
    Reply {
        key: i64,
        agent: String,
        job: Option<i64>,
        peer: bool,
        role: String,
        blocks: Vec<Block>,
        /// Markdown body without control lines (for copying).
        text: String,
        controls: Vec<Control>,
        ts: f64,
        streaming: bool,
        latest_result: bool,
    },
    Line {
        key: i64,
        kind: LineKind,
        text: String,
        agent: Option<String>,
        to: Option<String>,
        ts: f64,
        tone: Tone,
    },
    Alert {
        key: i64,
        kind: String,
        agent: String,
        job: Option<i64>,
        text: String,
        ts: f64,
    },
    Approval {
        key: i64,
        job: Option<i64>,
        approval: Option<i64>,
        request: String,
        ts: f64,
    },
    Report {
        key: i64,
        owner: String,
        text: String,
        ts: f64,
    },
    Progress {
        key: i64,
        rows: Vec<ProgressRow>,
    },
}

/// Latest operator-facing activity for the live line.
#[derive(Clone, Debug, PartialEq)]
pub struct Latest {
    pub job: Option<i64>,
    pub turn: Option<i64>,
    pub label: String,
    pub ts: f64,
    pub agent: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Feed {
    pub items: Vec<Item>,
    pub latest: Option<Latest>,
}

fn s(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// Payload as text: plain JSON strings, or `{text}` objects.
fn text_payload(p: &Value) -> String {
    match p {
        Value::String(x) => x.clone(),
        Value::Null => String::new(),
        other => {
            let t = s(other, "text");
            if t.is_empty() { s(other, "body") } else { t }
        }
    }
}

const TAGS: [&str; 10] = [
    "SEND",
    "ROLE",
    "NOTE",
    "TOOL",
    "PROGRESS",
    "GOAL",
    "NEXT",
    "FINISH",
    "LEARN",
    "EXHAUSTED",
];

/// Recognise an `SA_<TAG> {json}` control line (same rule as `cleanReply`).
fn control_tag(line: &str) -> Option<(&'static str, &str)> {
    let t = line.trim_start();
    let rest = t.strip_prefix("SA_")?;
    for tag in TAGS {
        if let Some(after) = rest.strip_prefix(tag)
            && (after.is_empty() || after.starts_with(char::is_whitespace))
        {
            return Some((tag, after.trim()));
        }
    }
    None
}

fn control(tag: &str, raw: &str) -> Control {
    let v: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    let pick = |keys: &[&str]| {
        keys.iter()
            .map(|k| s(&v, k))
            .find(|x| !x.trim().is_empty())
            .unwrap_or_default()
    };
    let fallback = || {
        if v.is_null() {
            raw.to_string()
        } else {
            String::new()
        }
    };
    let (kind, title, text, agent) = match tag {
        "PROGRESS" => (
            ControlKind::Progress,
            "阶段汇报".to_string(),
            pick(&["summary"]),
            None,
        ),
        "SEND" => {
            let to = s(&v, "to");
            let title = if to.is_empty() {
                "派发".to_string()
            } else {
                format!("派发 → {}", agent_name(&to))
            };
            (
                ControlKind::Send,
                title,
                pick(&["role", "body"]),
                (!to.is_empty()).then_some(to),
            )
        }
        "NOTE" => {
            let job = s(&v, "job");
            let title = if job.is_empty() {
                "留言".to_string()
            } else {
                format!("留言 → 任务 #{job}")
            };
            (ControlKind::Note, title, pick(&["body"]), None)
        }
        "ROLE" => (ControlKind::Role, "分工".into(), pick(&["role"]), None),
        "TOOL" => {
            let t = [s(&v, "server"), s(&v, "tool")]
                .into_iter()
                .filter(|x| !x.is_empty())
                .collect::<Vec<_>>();
            (ControlKind::Tool, "工具调用".into(), t.join(" / "), None)
        }
        "GOAL" => (ControlKind::Goal, "目标".into(), pick(&["objective"]), None),
        "NEXT" => (ControlKind::Next, "下一步".into(), pick(&["body"]), None),
        "FINISH" => {
            if s(&v, "status") == "blocked" {
                (
                    ControlKind::Blocked,
                    "阶段受阻".into(),
                    pick(&["summary"]),
                    None,
                )
            } else {
                (
                    ControlKind::Finish,
                    "阶段结论".into(),
                    pick(&["summary"]),
                    None,
                )
            }
        }
        "LEARN" => (ControlKind::Learn, "经验".into(), pick(&["lesson"]), None),
        _ => (
            ControlKind::Exhausted,
            "穷尽说明".into(),
            pick(&["summary"]),
            None,
        ),
    };
    let text = if text.is_empty() { fallback() } else { text };
    Control {
        kind,
        title,
        text: compact(&text, 600),
        agent,
    }
}

/// Split a reply into its visible markdown and folded control lines.
pub fn split_reply(text: &str) -> (String, Vec<Control>) {
    let mut body = Vec::new();
    let mut controls = Vec::new();
    let normalized = text.replace("\r\n", "\n");
    for line in normalized.split('\n') {
        match control_tag(line) {
            Some((tag, raw)) => controls.push(control(tag, raw)),
            None => body.push(line),
        }
    }
    (body.join("\n").trim().to_string(), controls)
}

fn progress_row(e: &EventView, jobs: &HashMap<i64, &JobView>) -> ProgressRow {
    let p = &e.payload;
    let agent = e
        .agent
        .clone()
        .filter(|a| !a.is_empty())
        .or_else(|| Some(s(p, "agent")).filter(|a| !a.is_empty()))
        .or_else(|| e.job.and_then(|j| jobs.get(&j)).map(|j| j.agent.clone()))
        .unwrap_or_default();
    let steps = p
        .get("steps")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|st| (s(st, "tool"), s(st, "state"))).collect())
        .unwrap_or_default();
    let label = s(p, "label");
    ProgressRow {
        seq: e.seq,
        ts: e.ts,
        label: if label.is_empty() {
            "执行事件".into()
        } else {
            compact(&label, 200)
        },
        agent,
        job: e.job,
        turn: e.turn,
        state: s(p, "state"),
        tool: s(p, "tool"),
        task_id: s(p, "taskId"),
        title: s(p, "title"),
        steps,
    }
}

/// Summary line of a progress group: "12 条进展 · 13:20:05 Claude Code · 调用 Bash · 已完成".
pub fn group_summary(rows: &[ProgressRow]) -> String {
    let Some(last) = rows.last() else {
        return String::new();
    };
    let mut out = format!("{} 条进展 · {}", rows.len(), activity_time(last.ts));
    if !last.agent.is_empty() {
        out.push(' ');
        out.push_str(&agent_name(&last.agent));
    }
    out.push_str(" · ");
    out.push_str(&last.label);
    if !last.state.is_empty() && last.state != "running" {
        out.push_str(" · ");
        out.push_str(&activity_state(&last.state));
    }
    out
}

pub fn build(ticket: &TicketDetail, jobs: &[JobView], events: &[EventView]) -> Feed {
    let by_id: HashMap<i64, &JobView> = jobs.iter().map(|j| (j.id, j)).collect();
    let mut items: Vec<Item> = Vec::new();
    let mut latest: Option<Latest> = None;
    let mut goal = false;
    let mut root_reply = false;
    // job -> (index into items, accumulated delta text)
    let mut streams: HashMap<i64, (usize, String)> = HashMap::new();
    let mut removed: Vec<usize> = Vec::new();

    for e in events {
        let p = &e.payload;
        let job = e.job.and_then(|j| by_id.get(&j)).copied();
        let agent = job.map(|j| j.agent.clone()).unwrap_or_default();
        let key = e.seq;
        if let (Some(label), Some(_), Some(_)) = (event_label(&e.kind), e.job, e.turn)
            && e.ts.is_finite()
        {
            latest = Some(Latest {
                job: e.job,
                turn: e.turn,
                label: label.into(),
                ts: e.ts,
                agent: None,
            });
        }
        match e.kind.as_str() {
            "activity" => {
                let row = progress_row(e, &by_id);
                latest = Some(Latest {
                    job: e.job,
                    turn: e.turn,
                    label: row.label.clone(),
                    ts: e.ts,
                    agent: Some(row.agent.clone()).filter(|a| !a.is_empty()),
                });
                if s(p, "category") == "report" {
                    let owner = [
                        agent_name(&row.agent),
                        e.job.map(|j| format!("任务 #{j}")).unwrap_or_default(),
                        e.turn.map(|t| format!("第 {t} 轮")).unwrap_or_default(),
                        activity_time(e.ts),
                    ]
                    .into_iter()
                    .filter(|x| !x.is_empty() && x != "执行者")
                    .collect::<Vec<_>>()
                    .join(" · ");
                    let text = row
                        .label
                        .trim_start_matches("进展汇报")
                        .trim_start_matches([' ', '·'])
                        .trim();
                    items.push(Item::Report {
                        key,
                        owner,
                        text: text.to_string(),
                        ts: e.ts,
                    });
                    continue;
                }
                if let Some(Item::Progress { rows, .. }) = items.last_mut()
                    && rows.len() < GROUP_MAX
                {
                    rows.push(row);
                } else {
                    items.push(Item::Progress {
                        key,
                        rows: vec![row],
                    });
                }
            }
            "user" | "user.follow" => {
                if e.kind == "user" && goal {
                    continue;
                }
                items.push(Item::User {
                    key,
                    text: text_payload(p),
                    ts: e.ts,
                    follow: e.kind == "user.follow",
                });
                if e.kind == "user" {
                    goal = true;
                }
            }
            "delta" => {
                let Value::String(chunk) = p else { continue };
                if job.is_some_and(|j| j.parent.is_some()) {
                    continue;
                }
                let Some(j) = e.job else { continue };
                let entry = streams.entry(j).or_insert_with(|| {
                    items.push(Item::Reply {
                        key,
                        agent: agent.clone(),
                        job: Some(j),
                        peer: false,
                        role: String::new(),
                        blocks: Vec::new(),
                        text: String::new(),
                        controls: Vec::new(),
                        ts: e.ts,
                        streaming: true,
                        latest_result: false,
                    });
                    (items.len() - 1, String::new())
                });
                entry.1.push_str(chunk);
                if entry.1.len() > 60000 {
                    let cut = entry.1.len() - 60000;
                    let cut = (cut..entry.1.len())
                        .find(|i| entry.1.is_char_boundary(*i))
                        .unwrap_or(0);
                    entry.1.drain(..cut);
                }
            }
            "reply" => {
                let (body, controls) = split_reply(&s(p, "text"));
                let peer = job.is_some_and(|j| j.parent.is_some());
                let reply = Item::Reply {
                    key,
                    agent: agent.clone(),
                    job: e.job,
                    peer,
                    role: job.map(|j| j.role.clone()).unwrap_or_default(),
                    blocks: markdown::parse(&body),
                    text: body.clone(),
                    controls,
                    ts: e.ts,
                    streaming: false,
                    latest_result: false,
                };
                let empty = body.is_empty()
                    && matches!(&reply, Item::Reply { controls, .. } if controls.is_empty());
                match e.job.and_then(|j| streams.remove(&j)) {
                    Some((idx, _)) if empty => removed.push(idx),
                    Some((idx, _)) => {
                        if let Item::Reply { key: k, .. } = &items[idx] {
                            let k = *k;
                            items[idx] = reply;
                            if let Item::Reply { key, .. } = &mut items[idx] {
                                *key = k;
                            }
                        }
                    }
                    None if empty => {}
                    None => items.push(reply),
                }
                if !peer {
                    root_reply = true;
                }
            }
            "send" => {
                let to = s(p, "to");
                let what = {
                    let r = s(p, "role");
                    if r.is_empty() { s(p, "body") } else { r }
                };
                items.push(Item::Line {
                    key,
                    kind: LineKind::Send,
                    text: compact(&what, 140),
                    agent: Some(agent.clone()),
                    to: Some(to),
                    ts: e.ts,
                    tone: Tone::Normal,
                });
            }
            "note" | "note.rejected" => items.push(Item::Line {
                key,
                kind: LineKind::Note,
                text: format!(
                    "留言 → 任务 #{}　{}",
                    s(p, "job"),
                    compact(&s(p, "body"), 200)
                ),
                agent: None,
                to: None,
                ts: e.ts,
                tone: if e.kind == "note.rejected" {
                    Tone::Error
                } else {
                    Tone::Normal
                },
            }),
            "role" => items.push(Item::Line {
                key,
                kind: LineKind::Auto,
                text: format!(
                    "{} · 分工 · {}",
                    agent_name(&agent),
                    compact(&s(p, "role"), 140)
                ),
                agent: Some(agent.clone()),
                to: None,
                ts: e.ts,
                tone: Tone::Normal,
            }),
            "failed" | "interrupted" | "draining" | "cancelled" => {
                items.push(Item::Alert {
                    key,
                    kind: e.kind.clone(),
                    agent: agent.clone(),
                    job: e.job,
                    text: if let Value::String(x) = p {
                        x.chars().take(2200).collect()
                    } else {
                        String::new()
                    },
                    ts: e.ts,
                });
                if let Some(j) = e.job
                    && let Some((idx, _)) = streams.remove(&j)
                {
                    removed.push(idx);
                }
            }
            "approval.request" => {
                let action = p.get("action").cloned().unwrap_or(Value::Null);
                let tool = s(&action, "tool");
                items.push(Item::Approval {
                    key,
                    job: e.job,
                    approval: p.get("approval").and_then(Value::as_i64),
                    request: compact(
                        if tool.is_empty() {
                            "请求授权"
                        } else {
                            &tool
                        },
                        140,
                    ),
                    ts: e.ts,
                });
            }
            "mcp.result" => {
                let done = s(p, "state") == "done";
                let what = [s(p, "server"), s(p, "tool")]
                    .into_iter()
                    .filter(|x| !x.is_empty())
                    .collect::<Vec<_>>();
                items.push(Item::Line {
                    key,
                    kind: LineKind::Tool,
                    text: format!(
                        "工具调用{} · {}",
                        if done { "完成" } else { "失败" },
                        what.join(" / ")
                    ),
                    agent: None,
                    to: None,
                    ts: e.ts,
                    tone: if done { Tone::Normal } else { Tone::Error },
                });
            }
            "approval.decision" => items.push(Item::Line {
                key,
                kind: LineKind::Decision,
                text: format!(
                    "审批 #{} {}",
                    s(p, "approval"),
                    if s(p, "decision") == "allow" {
                        "已允许"
                    } else {
                        "已拒绝"
                    }
                ),
                agent: None,
                to: None,
                ts: e.ts,
                tone: Tone::Normal,
            }),
            k @ ("auto.learn" | "auto.goal" | "auto.next" | "auto.discovered"
            | "auto.milestone" | "auto.exhausted" | "auto.finish" | "auto.recover") => {
                let label = match k {
                    "auto.learn" => "经验",
                    "auto.goal" => "目标",
                    "auto.next" => "下一步",
                    "auto.discovered" => "发现后续工作",
                    "auto.milestone" => "阶段回报",
                    "auto.exhausted" => "穷尽说明",
                    "auto.finish" => "阶段结论",
                    _ => "复核中断状态",
                };
                let body = ["lesson", "objective", "body", "summary"]
                    .iter()
                    .map(|f| s(p, f))
                    .find(|x| !x.is_empty())
                    .unwrap_or_else(|| text_payload(p));
                let blocked = s(p, "status") == "blocked";
                items.push(Item::Line {
                    key,
                    kind: LineKind::Auto,
                    text: format!(
                        "{}{} · {}",
                        label,
                        if blocked { "（受阻）" } else { "" },
                        compact(&body, 300)
                    ),
                    agent: None,
                    to: None,
                    ts: e.ts,
                    tone: if blocked { Tone::Error } else { Tone::Normal },
                });
            }
            "turn.stalled" | "turn.retry_scheduled" | "turn.route_wait" | "turn.gave_up" | "route.rotated"
            | "harness.health" | "turn.loop_warning" | "turn.loop_stopped" | "workspace.result" | "workspace.error" => {
                if let Some((text, tone)) = resilience_line(&e.kind, p, &agent) {
                    items.push(Item::Line {
                        key,
                        kind: LineKind::Retry,
                        text,
                        agent: None,
                        to: None,
                        ts: e.ts,
                        tone,
                    });
                }
            }
            // `turn.retry_scheduled` right before carries the same retry with more detail.
            "auto.retry"
                if matches!(
                    items.last(),
                    Some(Item::Line { kind: LineKind::Retry, ts, .. }) if (*ts - e.ts).abs() < 0.001
                ) => {}
            "auto.backoff" | "auto.retry" => items.push(Item::Line {
                key,
                kind: LineKind::Retry,
                text: format!("自动重试 · {}", compact(&s(p, "reason"), 200)),
                agent: None,
                to: None,
                ts: e.ts,
                tone: Tone::Normal,
            }),
            _ => {}
        }
    }

    // Fill streaming replies with their accumulated text.
    for (_, (idx, body)) in streams {
        let (clean, controls) = split_reply(&body);
        if clean.is_empty() && controls.is_empty() {
            removed.push(idx);
        } else if let Some(Item::Reply {
            blocks,
            text,
            controls: c,
            ..
        }) = items.get_mut(idx)
        {
            *blocks = markdown::parse(&clean);
            *text = clean;
            *c = controls;
        }
    }
    removed.sort_unstable();
    removed.dedup();
    for idx in removed.into_iter().rev() {
        if idx < items.len() {
            items.remove(idx);
        }
    }

    if !goal {
        items.insert(
            0,
            Item::User {
                key: i64::MIN,
                text: ticket.goal.clone(),
                ts: ticket.created,
                follow: false,
            },
        );
    }
    let root = jobs.iter().find(|j| j.parent.is_none());
    if !root_reply
        && let Some(root) = root
        && matches!(root.state.as_str(), "done" | "partial")
        && let Some(result) = &root.result
    {
        let (body, controls) = split_reply(result);
        if !body.is_empty() || !controls.is_empty() {
            items.push(Item::Reply {
                key: i64::MAX,
                agent: root.agent.clone(),
                job: Some(root.id),
                peer: false,
                role: String::new(),
                blocks: markdown::parse(&body),
                text: body.clone(),
                controls,
                ts: 0.0,
                streaming: false,
                latest_result: true,
            });
        }
    }
    Feed { items, latest }
}

/// Alert title such as "Codex · 失败".
pub fn alert_title(kind: &str, agent: &str) -> String {
    format!("{} · {}", agent_name(agent), job_state(kind))
}

/// Status line of a resilience event (spec §9.2 G); `None` hides it.
fn resilience_line(kind: &str, p: &Value, agent: &str) -> Option<(String, Tone)> {
    let who = agent_name(agent);
    let num = |k: &str| p.get(k).and_then(Value::as_f64);
    let after = |ms: Option<f64>| {
        ms.map(|ms| format!(" · {} 后", crate::labels::duration((ms / 1000.0).round())))
            .unwrap_or_default()
    };
    let class = crate::labels::error_class(&s(p, "class"));
    Some(match kind {
        "turn.loop_warning" | "turn.loop_stopped" | "workspace.error" => (format!("{who} · {}",compact(&s(p,"reason"),240)),Tone::Error),
        "workspace.result" => (format!("任务 #{} · {}",p["job"],compact(&s(p,"detail"),240)),if p["state"]=="failed"||p["state"]=="conflict"{Tone::Error}else{Tone::Normal}),
        "turn.stalled" => {
            let phase = match s(p, "phase").as_str() {
                "first_frame" => "启动后迟迟没有输出",
                "model_idle" => "等待模型输出时无响应",
                "tool_idle" => "工具执行期间无响应",
                "hard_cap" => "超过单轮时长上限",
                _ => "长时间无响应",
            };
            (
                format!(
                    "{who} · 疑似卡住 · {phase} {}，已中断并准备续接",
                    crate::labels::duration((num("idle_ms").unwrap_or(0.0) / 1000.0).round())
                ),
                Tone::Error,
            )
        }
        "turn.retry_scheduled" => {
            let attempt = num("attempt").unwrap_or(0.0) as i64;
            let of = num("max").map(|m| format!("/{}", m as i64)).unwrap_or_default();
            let resume = if p.get("resume_session") == Some(&Value::Bool(true)) { " · 续接原会话" } else { "" };
            (
                format!("{who} · 已中断，重试中 {attempt}{of} · {class}{}{resume}", after(num("after_ms"))),
                Tone::Normal,
            )
        }
        "turn.route_wait" => (
            format!(
                "{who} · 等待上游恢复 · {} · {class}",
                crate::labels::route_label(p.get("route").unwrap_or(&Value::Null))
            ),
            Tone::Normal,
        ),
        "turn.gave_up" => (
            format!("{who} · 已放弃（{class}）· 共尝试 {} 次", num("attempts").unwrap_or(0.0) as i64),
            Tone::Error,
        ),
        "route.rotated" => (
            format!(
                "{who} · 供应商轮换 · {} → {} · {}{}",
                crate::labels::route_label(p.get("from").unwrap_or(&Value::Null)),
                crate::labels::route_label(p.get("to").unwrap_or(&Value::Null)),
                crate::labels::error_class(&s(p, "reason")),
                after(num("after_ms"))
            ),
            Tone::Normal,
        ),
        "harness.health" => {
            let state = s(p, "state");
            let text = match state.as_str() {
                "open" => "执行端熔断",
                "half_open" => "执行端试探恢复",
                "degraded" => "执行端降级",
                "healthy" => "执行端已恢复",
                _ => return None,
            };
            let reason = p
                .get("reason_class")
                .and_then(Value::as_str)
                .map(|c| format!(" · {}", crate::labels::error_class(c)))
                .unwrap_or_default();
            (
                format!("{} · {text} · {}{reason}", agent_name(&s(p, "agent")), crate::labels::route_label(p)),
                if state == "healthy" { Tone::Normal } else { Tone::Error },
            )
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ticket() -> TicketDetail {
        TicketDetail {
            id: 1,
            goal: "目标".into(),
            objective: None,
            cwd: "E:\\p".into(),
            lead: "claude".into(),
            created: 1.0,
            permission_profile: "readonly".into(),
            agent_routes: json!({}),
            allow_lead_routing: true,
            state: "running".into(),
        }
    }

    fn job(id: i64, agent: &str, parent: Option<i64>) -> JobView {
        JobView {
            id,
            agent: agent.into(),
            parent,
            role: String::new(),
            state: "running".into(),
            turn: 1,
            writing: false,
            ready_after: 0.0,
            permission_override: None,
            body: String::new(),
            result: None,
            route: None,
        }
    }

    fn ev(seq: i64, kind: &str, job: i64, payload: Value) -> EventView {
        EventView {
            seq,
            kind: kind.into(),
            ts: 100.0 + seq as f64,
            job: Some(job),
            turn: Some(1),
            agent: None,
            payload,
        }
    }

    #[test]
    fn folds_control_lines() {
        let (body, controls) = split_reply(
            "你好\nSA_PROGRESS {\"summary\":\"完成一半\"}\n  SA_SEND {\"to\":\"codex\",\"role\":\"审查\"}\nSA_SENDER x",
        );
        assert_eq!(body, "你好\nSA_SENDER x");
        assert_eq!(controls.len(), 2);
        assert_eq!(controls[0].title, "阶段汇报");
        assert_eq!(controls[0].text, "完成一半");
        assert_eq!(controls[1].title, "派发 → Codex");
        assert_eq!(controls[1].text, "审查");
    }

    #[test]
    fn groups_progress_and_inserts_goal() {
        let jobs = vec![job(1, "claude", None)];
        let mut events: Vec<EventView> = (1..=30)
            .map(|i| {
                ev(
                    i,
                    "activity",
                    1,
                    json!({"label":"调用 Bash","state":"running"}),
                )
            })
            .collect();
        events.push(ev(31, "reply", 1, json!({"text":"完成"})));
        let feed = build(&ticket(), &jobs, &events);
        assert!(matches!(&feed.items[0], Item::User { text, .. } if text == "目标"));
        assert!(matches!(&feed.items[1], Item::Progress { rows, .. } if rows.len() == 24));
        assert!(matches!(&feed.items[2], Item::Progress { rows, .. } if rows.len() == 6));
        assert!(matches!(&feed.items[3], Item::Reply { peer: false, .. }));
        assert_eq!(feed.latest.unwrap().label, "收到回报");
    }

    #[test]
    fn stream_is_replaced_by_reply_and_peer_marked() {
        let jobs = vec![job(1, "claude", None), job(2, "codex", Some(1))];
        let events = vec![
            ev(1, "user", 1, json!("hi")),
            ev(2, "delta", 1, json!("部分")),
            ev(3, "reply", 2, json!({"text":"协作结果"})),
            ev(4, "reply", 1, json!({"text":"最终"})),
        ];
        let feed = build(&ticket(), &jobs, &events);
        assert_eq!(feed.items.len(), 3);
        assert!(matches!(
            &feed.items[1],
            Item::Reply {
                key: 2,
                streaming: false,
                peer: false,
                ..
            }
        ));
        assert!(matches!(&feed.items[2], Item::Reply { peer: true, .. }));
    }

    #[test]
    fn resilience_events_become_status_lines() {
        let jobs = vec![job(1, "codex", None)];
        let route = json!({"provider": "openai", "model": "gpt", "mode": "cli"});
        let mut retry = ev(2, "turn.retry_scheduled", 1, json!({"class": "overloaded", "attempt": 2, "max": 5, "after_ms": 4000, "resume_session": true, "route": route}));
        let mut legacy = ev(3, "auto.retry", 1, json!({"after_seconds": 4, "reason": "serverOverloaded"}));
        retry.ts = 50.0;
        legacy.ts = 50.0;
        let events = vec![
            ev(1, "turn.stalled", 1, json!({"phase": "model_idle", "idle_ms": 361000})),
            retry,
            legacy,
            ev(4, "route.rotated", 1, json!({"from": route, "to": {"provider": "azure", "model": "gpt", "mode": "cli"}, "reason": "auth", "after_ms": 0})),
            ev(5, "harness.health", 1, json!({"agent": "codex", "provider": "openai", "model": "gpt", "state": "open", "reason_class": "overloaded"})),
            ev(6, "turn.gave_up", 1, json!({"class": "auth", "attempts": 3})),
        ];
        let feed = build(&ticket(), &jobs, &events);
        let lines: Vec<&str> = feed
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Line { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            lines,
            vec![
                "Codex · 疑似卡住 · 等待模型输出时无响应 6 分 01 秒，已中断并准备续接",
                "Codex · 已中断，重试中 2/5 · 上游过载 · 4 秒 后 · 续接原会话",
                "Codex · 供应商轮换 · openai / gpt → azure / gpt · 认证失败 · 0 秒 后",
                "Codex · 执行端熔断 · openai / gpt · 上游过载",
                "Codex · 已放弃（认证失败）· 共尝试 3 次",
            ]
        );
    }

    #[test]
    fn bad_payloads_do_not_panic() {
        let jobs = vec![job(1, "claude", None)];
        let events = vec![
            ev(1, "reply", 9, json!(null)),
            ev(2, "send", 1, json!(42)),
            ev(3, "failed", 1, json!({"x":1})),
            ev(4, "delta", 1, json!({"not":"string"})),
            ev(5, "approval.request", 1, json!("weird")),
            ev(6, "activity", 1, json!([1, 2])),
        ];
        let feed = build(&ticket(), &jobs, &events);
        assert!(!feed.items.is_empty());
    }
}
