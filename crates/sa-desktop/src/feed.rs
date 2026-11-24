//! Turns journal events into conversation items. Port of `ingest` in
//! `desktop/ui/studio.js` plus `desktop/ui/activity.js`, with one addition:
//! SA_* control lines are folded into compact chips instead of being dropped.

use crate::labels::{activity_state, activity_time, agent_name, compact, event_label, job_state};
use crate::markdown::{self, Block};
use sa_store::{EventView, JobView, TicketDetail};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

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
    ReportDone,
    ReportPartial,
    ReportBlocked,
    Ask,
    CheckPass,
    CheckFail,
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
        /// Parsed body, shared with [`ParseCache`].
        blocks: Arc<Vec<Block>>,
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

/// Apply [`sa_store::folded_from_feed`] to events accumulated across
/// incremental loads, and drop the lead's text deltas once its turn is no
/// longer streaming (the reply replaces them).
pub fn fold_settled_rounds(events: &mut Vec<EventView>, jobs: &[JobView], lead_resumed: i64) {
    let root = jobs.iter().find(|j| j.parent.is_none());
    let root_id = root.map(|j| j.id);
    let streaming = root.filter(|j| j.state == "running").map(|j| (j.id, j.turn));
    let rounds: HashMap<i64, (&str, i64)> = jobs.iter().map(|j| (j.id, (j.state.as_str(), j.turn))).collect();
    events.retain(|e| {
        if e.kind == "delta" && e.job.is_some() && e.job == root_id {
            return streaming == e.job.zip(e.turn);
        }
        let round = e.job.and_then(|j| rounds.get(&j).copied());
        !sa_store::folded_from_feed(&e.kind, e.seq, e.job, e.turn, root_id, lead_resumed, round)
    });
}

/// Resolve stable dispatch keys only when the snapshot identifies one child.
fn dispatch_keys(jobs: &[JobView], events: &[EventView]) -> HashMap<(i64, String), i64> {
    let mut keys = HashMap::new();
    let sources = events.iter().filter(|e| e.kind == "reply").filter_map(|e| e.job.map(|id| (id, s(&e.payload, "text"))))
        .chain(jobs.iter().filter_map(|j| j.result.as_ref().map(|text| (j.id, text.clone()))));
    for (owner, text) in sources {
        for line in text.lines() {
            let Some(("SEND", raw)) = control_tag(line) else { continue };
            let Ok(value) = serde_json::from_str::<Value>(raw) else { continue };
            let Some(key) = value.get("id").and_then(Value::as_str) else { continue };
            if value.get("continue").is_some() { continue; }
            let candidates: Vec<_> = jobs.iter().filter(|j| j.parent == Some(owner) && j.agent == s(&value, "to") && j.body == s(&value, "body")).collect();
            if let [child] = candidates.as_slice() {
                keys.insert((owner, key.into()), child.id);
            }
        }
    }
    keys
}

fn resolved_reply(text: &str, owner: Option<i64>, keys: &HashMap<(i64, String), i64>) -> (String, Vec<Control>) {
    let text = text.lines().map(|line| {
        let Some(("SEND", raw)) = control_tag(line) else { return line.to_string() };
        let Ok(mut value) = serde_json::from_str::<Value>(raw) else { return line.to_string() };
        if let Some(after) = value.get_mut("after").and_then(Value::as_array_mut) {
            for id in after {
                if let Some(key) = id.as_str()
                    && let Some(number) = owner.and_then(|owner| keys.get(&(owner, key.to_string())))
                {
                    *id = Value::from(*number);
                }
            }
        }
        format!("SA_SEND {value}")
    }).collect::<Vec<_>>().join("\n");
    split_reply(&text)
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

const TAGS: [&str; 12] = [
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
    "REPORT",
    "ASK",
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
            let mut title = if to.is_empty() {
                "派发".to_string()
            } else {
                format!("派发 → {}", agent_name(&to))
            };
            if let Some(target) = v.get("continue") {
                title = format!("续派给 #{target}");
            } else if let Some(after) = v.get("after").and_then(Value::as_array).filter(|a| !a.is_empty()) {
                let ids = after.iter().map(|id| format!("#{}", id.as_str().map(str::to_string).unwrap_or_else(|| id.to_string()))).collect::<Vec<_>>().join("、");
                title.push_str(&format!("，等 {ids} 完成后开始"));
            }
            (
                ControlKind::Send,
                title,
                pick(&["role", "body"]),
                (!to.is_empty()).then_some(to),
            )
        }
        "REPORT" => {
            let kind = match s(&v, "status").as_str() {
                "done" => ControlKind::ReportDone,
                "partial" => ControlKind::ReportPartial,
                _ => ControlKind::ReportBlocked,
            };
            let mut text = s(&v, "summary");
            for (key, label) in [("verified", "已核对"), ("unverified", "未核对"), ("artifacts", "产物")] {
                let entries = v.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>();
                if !entries.is_empty() {
                    text.push_str(&format!("\n{label}\n{}", entries.into_iter().map(|item| format!("· {item}")).collect::<Vec<_>>().join("\n")));
                }
            }
            (kind, crate::labels::outcome_status(&s(&v, "status")).into(), text, None)
        }
        "ASK" => (ControlKind::Ask, String::new(), format!("向你提问：{}", pick(&["question"])), None),
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
        // Outcome sections are already bounded by the protocol and need their
        // line breaks and full lists for the result block.
        text: if tag == "REPORT" { text } else { compact(&text, 600) },
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
    // The result is the last block of the reply, after other control lines.
    controls.sort_by_key(|c| matches!(c.kind, ControlKind::ReportDone | ControlKind::ReportPartial | ControlKind::ReportBlocked));
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

/// Parsed markdown by reply text, kept across rebuilds. The feed is rebuilt
/// on every journal change (many times a second while a reply streams);
/// without this each rebuild would parse every reply of the conversation again.
#[derive(Default)]
pub struct ParseCache {
    /// Text hash -> (text length, blocks). The length guards against a
    /// colliding hash handing out another reply's blocks.
    map: HashMap<u64, (usize, Arc<Vec<Block>>)>,
    used: std::collections::HashSet<u64>,
}

impl ParseCache {
    fn parse(&mut self, text: &str) -> Arc<Vec<Block>> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        let key = h.finish();
        self.used.insert(key);
        match self.map.get(&key) {
            Some((len, blocks)) if *len == text.len() => blocks.clone(),
            _ => {
                let blocks = Arc::new(markdown::parse(text));
                self.map.insert(key, (text.len(), blocks.clone()));
                blocks
            }
        }
    }

    /// Forget texts the last build did not use (a streaming reply leaves one
    /// stale entry per rebuild).
    fn sweep(&mut self) {
        let used = std::mem::take(&mut self.used);
        self.map.retain(|k, _| used.contains(k));
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

/// A build with no earlier parse to reuse; the app always carries a cache.
#[cfg(test)]
pub fn build(ticket: &TicketDetail, jobs: &[JobView], events: &[EventView]) -> Feed {
    build_with(ticket, jobs, events, &mut ParseCache::default())
}

/// [`build`], reusing markdown parsed by earlier builds of the same ticket.
pub fn build_with(
    ticket: &TicketDetail,
    jobs: &[JobView],
    events: &[EventView],
    cache: &mut ParseCache,
) -> Feed {
    let by_id: HashMap<i64, &JobView> = jobs.iter().map(|j| (j.id, j)).collect();
    let dispatches = dispatch_keys(jobs, events);
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
                        blocks: Arc::default(),
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
                let (body, controls) = resolved_reply(&s(p, "text"), e.job, &dispatches);
                let peer = job.is_some_and(|j| j.parent.is_some());
                let reply = Item::Reply {
                    key,
                    agent: agent.clone(),
                    job: e.job,
                    peer,
                    role: job.map(|j| j.role.clone()).unwrap_or_default(),
                    blocks: cache.parse(&body),
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
            k @ ("ask" | "answer" | "ask.withdrawn" | "continued" | "skipped" | "check.started" | "check.result" | "check.bounced" | "auto.stalled") => {
                if let Some(text) = crate::labels::collaboration_event(k, p) {
                    latest = Some(Latest { job: e.job, turn: e.turn, label: text.clone(), ts: e.ts, agent: Some(agent.clone()) });
                    items.push(Item::Line { key, kind: LineKind::Auto, text, agent: None, to: None, ts: e.ts,
                        tone: if k == "check.bounced" || (k == "check.result" && p.get("ok") == Some(&Value::Bool(false))) { Tone::Error } else { Tone::Normal } });
                }
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
            "turn.stalled" | "turn.retry_scheduled" | "turn.route_wait" | "turn.quota_wait" | "turn.deferred" | "turn.gave_up" | "route.rotated"
            | "harness.health" | "turn.loop_warning" | "turn.loop_stopped" | "workspace.result" | "workspace.error" => {
                if let Some((text, tone)) = resilience_line(&e.kind, p, &agent) {
                    if let Some(label) = quota_wait_line(&e.kind, p)
                        && e.job.is_some() && e.turn.is_some() && e.ts.is_finite()
                    {
                        latest = Some(Latest {
                            job: e.job,
                            turn: e.turn,
                            label,
                            ts: e.ts,
                            agent: None,
                        });
                    }
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
            *blocks = cache.parse(&clean);
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
        let (body, controls) = resolved_reply(result, Some(root.id), &dispatches);
        if !body.is_empty() || !controls.is_empty() {
            items.push(Item::Reply {
                key: i64::MAX,
                agent: root.agent.clone(),
                job: Some(root.id),
                peer: false,
                role: String::new(),
                blocks: cache.parse(&body),
                text: body.clone(),
                controls,
                ts: 0.0,
                streaming: false,
                latest_result: true,
            });
        }
    }
    // Attach platform evidence only to the latest committed reply for each job.
    let mut seen = std::collections::HashSet::new();
    for item in items.iter_mut().rev() {
        if let Item::Reply { job: Some(id), controls, streaming: false, .. } = item
            && seen.insert(*id)
            && let Some(job) = by_id.get(id)
        {
            if let Some(outcome) = &job.outcome {
                controls.retain(|c| !matches!(c.kind, ControlKind::ReportDone | ControlKind::ReportPartial | ControlKind::ReportBlocked));
                controls.push(control("REPORT", &outcome.to_string()));
            }
            if let Some(checks) = &job.checks {
                controls.extend(crate::labels::check_lines(checks).into_iter().map(|(text, ok)| Control {
                    kind: if ok { ControlKind::CheckPass } else { ControlKind::CheckFail },
                    title: String::new(), text, agent: None,
                }));
            }
        }
    }
    cache.sweep();
    Feed { items, latest }
}

/// Alert title such as "Codex · 失败".
pub fn alert_title(kind: &str, agent: &str) -> String {
    format!("{} · {}", agent_name(agent), job_state(kind))
}

/// Quota waits are normal scheduling holds, separate from route recovery.
fn quota_wait_line(kind: &str, p: &Value) -> Option<String> {
    if !matches!(kind, "turn.quota_wait" | "turn.deferred") {
        return None;
    }
    let reason = p.get("reason").and_then(Value::as_str).unwrap_or(if kind == "turn.quota_wait" {
        "quota_window"
    } else {
        ""
    });
    crate::labels::quota_wait(reason, p.get("until").and_then(Value::as_f64))
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
        "turn.quota_wait" | "turn.deferred" => (
            format!("{who} · {}", quota_wait_line(kind, p)?),
            Tone::Normal,
        ),
        "turn.route_wait" => (
            format!(
                "{who} · 等待上游恢复 · {} · {class}",
                crate::labels::route_label(p.get("route").unwrap_or(&Value::Null))
            ),
            Tone::Normal,
        ),
        "turn.gave_up" => {
            let reset = num("quota_until").map(crate::labels::reset_time).filter(|s| !s.is_empty());
            let detail = if let Some(reset) = reset {
                format!("订阅额度窗口已用尽，预计 {reset} 重置")
            } else {
                format!("共尝试 {} 次", num("attempts").unwrap_or(0.0) as i64)
            };
            (format!("{who} · 已放弃（{class}）· {detail}"), Tone::Error)
        },
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
            pause_status: None,
            attention: None,
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
            outcome: None,
            checks: None,
            needs: Vec::new(),
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
    fn a_resumed_lead_keeps_only_its_own_overview_from_the_settled_round() {
        let mut lead = job(1, "claude", None);
        lead.turn = 2;
        let mut codex = job(2, "codex", Some(1));
        codex.state = "done".into();
        let jobs = [lead, codex];
        let at = |mut e: EventView, turn: i64| {
            e.turn = Some(turn);
            e
        };
        let mut events = vec![
            ev(1, "user", 1, json!("目标")),
            ev(2, "delta", 1, json!("第 1 轮")),
            ev(3, "reply", 1, json!({"text": "第 1 轮概览"})),
            ev(4, "activity", 2, json!({"label": "进展汇报 · 已录到点按 E", "category": "report", "state": "reported"})),
            ev(5, "reply", 2, json!({"text": "录制完成"})),
            ev(6, "note", 2, json!({"job": 1, "body": "实例路径不存在"})),
            ev(7, "ask", 2, json!({"id": 1, "question": "有备份吗？"})),
            at(ev(9, "delta", 1, json!("第 2 轮")), 2),
        ];
        fold_settled_rounds(&mut events, &jobs, 8);
        let kept: Vec<_> = events.iter().map(|e| (e.seq, e.kind.as_str())).collect();
        assert_eq!(kept, [(1, "user"), (3, "reply"), (7, "ask"), (9, "delta")]);

        let feed = build(&ticket(), &jobs, &events);
        assert!(!feed.items.iter().any(|i| matches!(i, Item::Report { .. })));
        assert!(!feed.items.iter().any(|i| matches!(i, Item::Reply { peer: true, .. })));
        assert!(feed.items.iter().any(|i| matches!(i, Item::Reply { peer: false, streaming: false, .. })));

        // Once the lead's turn 2 replies, its stream text is dropped too.
        let mut jobs = jobs;
        jobs[0].state = "waiting".into();
        fold_settled_rounds(&mut events, &jobs, 8);
        assert!(!events.iter().any(|e| e.kind == "delta"));
    }

    #[test]
    fn rebuilds_reuse_parsed_replies_and_drop_stale_stream_text() {
        let jobs = [job(1, "claude", None)];
        let mut events = vec![
            ev(1, "user", 1, json!("目标")),
            ev(2, "reply", 1, json!({"text": "**第一轮** 结论"})),
            ev(3, "delta", 1, json!("正在")),
        ];
        let mut cache = ParseCache::default();
        let first = build_with(&ticket(), &jobs, &events, &mut cache);
        let blocks = |feed: &Feed, i: usize| match &feed.items[i] {
            Item::Reply { blocks, .. } => blocks.clone(),
            other => panic!("expected a reply, got {other:?}"),
        };
        // The finished reply and the streaming one.
        assert_eq!(cache.len(), 2);
        events.push(ev(4, "delta", 1, json!("输出")));
        let second = build_with(&ticket(), &jobs, &events, &mut cache);
        // Same allocation: the finished reply was not parsed again.
        assert!(Arc::ptr_eq(&blocks(&first, 1), &blocks(&second, 1)));
        assert!(matches!(&second.items[2], Item::Reply { text, streaming: true, .. } if text == "正在输出"));
        // The shorter stream text of the first build is forgotten.
        assert_eq!(cache.len(), 2);
        // The cached build equals an uncached one.
        assert_eq!(second, build(&ticket(), &jobs, &events));
    }

    #[test]
    fn folds_outcomes_questions_continuations_and_dependencies() {
        let (body, controls) = split_reply("正文\nSA_REPORT {\"status\":\"partial\",\"summary\":\"实现完成\",\"verified\":[\"单元检查\"],\"unverified\":[\"窗口\"],\"artifacts\":[\"out.txt\"]}\nSA_ASK {\"question\":\"是否继续\"}\nSA_SEND {\"continue\":12,\"body\":\"修复\"}\nSA_SEND {\"to\":\"codex\",\"after\":[\"impl\",12],\"body\":\"审阅\"}");
        assert_eq!(body, "正文");
        assert_eq!(controls[0].kind, ControlKind::Ask);
        assert_eq!(controls[0].text, "向你提问：是否继续");
        assert_eq!(controls[1].title, "续派给 #12");
        assert_eq!(controls[2].title, "派发 → Codex，等 #impl、#12 完成后开始");
        assert_eq!(controls[3].kind, ControlKind::ReportPartial);
        assert_eq!(controls[3].title, "部分完成");
        assert_eq!(controls[3].text, "实现完成\n已核对\n· 单元检查\n未核对\n· 窗口\n产物\n· out.txt");
    }

    #[test]
    fn platform_checks_attach_only_to_latest_reply_and_running_checks_are_activity() {
        let mut root = job(1, "claude", None);
        root.checks = Some(json!({"ok":true,"cached":false,"runs":[{"argv":["cargo","test"],"ok":true,"seconds":2.0}]}));
        let events = [
            ev(1, "reply", 1, json!({"text":"第一轮"})),
            ev(2, "reply", 1, json!({"text":"第二轮"})),
            ev(3, "check.started", 1, json!({"argv":["cargo","test"]})),
        ];
        let feed = build(&ticket(), &[root], &events);
        let replies: Vec<_> = feed.items.iter().filter_map(|item| match item { Item::Reply { controls, .. } => Some(controls), _ => None }).collect();
        assert!(replies[0].is_empty());
        assert_eq!(replies[1][0].kind, ControlKind::CheckPass);
        assert_eq!(replies[1][0].text, "平台检查通过 · cargo test · 2.0 秒");
        assert_eq!(feed.latest.unwrap().label, "正在运行完成检查：cargo test");
    }

    #[test]
    fn dependency_keys_resolve_only_with_a_unique_snapshot_child() {
        let mut child = job(12, "codex", Some(1));
        child.body = "实现".into();
        let text = "SA_SEND {\"id\":\"impl\",\"to\":\"codex\",\"body\":\"实现\"}\nSA_SEND {\"id\":\"review\",\"to\":\"claude\",\"after\":[\"impl\"],\"body\":\"审阅\"}";
        let events = [ev(1, "reply", 1, json!({"text":text}))];
        let keys = dispatch_keys(&[child], &events);
        let (_, controls) = resolved_reply(text, Some(1), &keys);
        assert_eq!(controls[1].title, "派发 → Claude Code，等 #12 完成后开始");
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
    fn quota_waits_explain_reset_and_reserve_without_retry_claims() {
        let jobs = vec![job(1, "codex", None)];
        let until = 1_800_000_000.0;
        for kind in ["turn.quota_wait", "turn.deferred"] {
            for (reason, expected) in [
                ("quota_window", format!("等待额度重置，预计 {} 继续", crate::labels::stamp(until))),
                ("quota_reserve", format!("为主控保留额度，预计 {} 后继续", crate::labels::stamp(until))),
            ] {
                let feed = build(&ticket(), &jobs, &[ev(1, kind, 1, json!({"reason":reason,"until":until}))]);
                assert!(matches!(&feed.items[1], Item::Line { text, tone: Tone::Normal, .. } if text == &format!("Codex · {expected}")));
                assert_eq!(feed.latest.unwrap().label, expected);
            }
        }
        let feed = build(&ticket(), &jobs, &[
            ev(1, "turn.gave_up", 1, json!({"class":"billing","quota_until":until})),
        ]);
        assert!(matches!(&feed.items[1], Item::Line { text, tone: Tone::Error, .. }
            if text.contains(&format!("预计 {} 重置", crate::labels::reset_time(until))) && !text.contains("0 次")));
        assert_eq!(resilience_line("turn.deferred", &json!({"reason":"circuit_open","until":until}), "codex"), None);
    }

    #[test]
    fn quota_bookkeeping_does_not_replace_latest_or_create_feed_items() {
        let jobs = vec![job(1, "codex", None)];
        let feed = build(&ticket(), &jobs, &[
            ev(1, "reply", 1, json!({"text":"答复"})),
            ev(2, "quota.window", 1, json!({"provider":"private-provider","used":0.92})),
        ]);
        assert_eq!(feed.items.len(), 2);
        assert_eq!(feed.latest.unwrap().label, "收到回报");
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
