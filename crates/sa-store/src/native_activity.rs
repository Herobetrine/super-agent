//! Port of `runtime/native-activity.cjs`: Claude stream-json events (and,
//! through `harness_activity`, the other harness formats) projected into
//! privacy-preserving progress items plus a per-ticket native task table.
//!
//! Only correlation keys and display labels survive. Tool arguments, prompts,
//! outputs, thinking text and summaries never reach an item or a task.

use crate::activity::{
    Activity, JS_SPACE_CLASS, Step, get, get_str, is_true, js_len, js_space, safe_integer, truthy,
};
use crate::harness_activity::harness_activity;
use crate::model::{LatestActivity, NativeActivity, NativeTask};
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

const TOOLS: &[&str] = &[
    "Agent",
    "Task",
    "Bash",
    "PowerShell",
    "Read",
    "Write",
    "Edit",
    "MultiEdit",
    "Glob",
    "Grep",
    "NotebookEdit",
    "WebFetch",
    "WebSearch",
    "TaskCreate",
    "TaskUpdate",
    "TaskList",
    "TaskGet",
    "TaskOutput",
    "TaskStop",
    "TodoWrite",
    "Skill",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
];
const ROLES: &[&str] = &[
    "Explore",
    "Plan",
    "general-purpose",
    "code-reviewer",
    "claude-code-guide",
];

fn status_label(state: &str) -> Option<&'static str> {
    Some(match state {
        "running" => "执行中",
        "completed" => "已完成",
        "failed" => "失败",
        "stopped" => "已停止",
        "unknown" => "状态待确认",
        _ => return None,
    })
}

fn terminal(state: &str) -> bool {
    matches!(state, "completed" | "failed" | "stopped")
}

/// `nativeId(value)`: `/^[A-Za-z0-9_-]{1,160}$/` strings only.
pub(crate) fn native_id(value: Option<&Value>) -> Option<String> {
    let s = value?.as_str()?;
    let ok = (1..=160).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    ok.then(|| s.to_owned())
}

fn role(v: Option<&Value>) -> Option<&str> {
    v?.as_str().filter(|s| ROLES.contains(s))
}

/// `toolLabel(name)`
fn tool_label(name: Option<&Value>) -> String {
    match name.and_then(Value::as_str) {
        Some(n) if TOOLS.contains(&n) => n.to_owned(),
        Some(n) if n.starts_with("mcp__") => "MCP 工具".to_owned(),
        _ => "工具".to_owned(),
    }
}

/// `taskLabel(e)`
fn task_label(e: &Value) -> String {
    let ev = Some(e);
    if let Some(r) = role(get(ev, "subagent_type")) {
        return format!("子代理 · {r}");
    }
    let task_type = get_str(ev, "task_type");
    if matches!(task_type, Some("local_agent" | "remote_agent")) || truthy(get(ev, "subagent_type"))
    {
        return "子代理".to_owned();
    }
    if task_type == Some("local_bash") {
        return "后台命令".to_owned();
    }
    "原生任务".to_owned()
}

static TITLE_HAS_HAN_OR_SPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"[\p{{Script=Han}}{JS_SPACE_CLASS}]")).unwrap());
static TITLE_CHARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\p{L}\p{N} ()（）、，。:：&+/\-]+$").unwrap());
static TITLE_PATHLIKE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?:^|[{JS_SPACE_CLASS}])/|//|[A-Za-z0-9_\-]{{32,}}"
    ))
    .unwrap()
});

/// Only Claude's short, operator-facing Agent title. Tool arguments, the
/// assignment prompt, progress payloads and terminal summaries are not titles.
fn task_title(e: &Value) -> Option<String> {
    let ev = Some(e);
    if get_str(ev, "subtype") != Some("task_started")
        || get_str(ev, "task_type") != Some("local_agent")
    {
        return None;
    }
    let s = get_str(ev, "description")?.trim_matches(js_space);
    let len = js_len(s);
    if !(2..=140).contains(&len) || !TITLE_HAS_HAN_OR_SPACE.is_match(s) {
        return None;
    }
    if !TITLE_CHARS.is_match(s) || TITLE_PATHLIKE.is_match(s) {
        return None;
    }
    Some(s.to_owned())
}

/// `status(value)`
fn status(value: Option<&Value>) -> &'static str {
    match value.and_then(Value::as_str) {
        Some("killed" | "cancelled" | "canceled") => "stopped",
        Some("running") => "running",
        Some("completed") => "completed",
        Some("failed") => "failed",
        Some("stopped") => "stopped",
        _ => "unknown",
    }
}

fn task_activity(e: &Value) -> Option<Activity> {
    let ev = Some(e);
    let task_id = native_id(get(ev, "task_id"))?;
    if is_true(get(ev, "ambient")) {
        return None;
    }
    let subtype = get_str(ev, "subtype");
    let state = match subtype {
        Some("task_started" | "task_progress") => "running",
        Some("task_notification") => status(get(ev, "status")),
        Some("task_updated") => {
            let s = get(get(ev, "patch"), "status").filter(|v| v.is_string())?;
            status(Some(s))
        }
        _ => return None,
    };
    let tool = get_str(ev, "last_tool_name")
        .filter(|s| !s.is_empty())
        .map(|_| tool_label(get(ev, "last_tool_name")));
    let detail = if subtype == Some("task_started") {
        "已启动"
    } else {
        status_label(state).unwrap_or_default()
    };
    let suffix = tool.as_ref().map(|t| format!(" · {t}")).unwrap_or_default();
    let mut item = Activity::new(format!("{}{detail}{suffix}", task_label(e)), "task", state);
    item.task_id = Some(task_id);
    item.tool = tool;
    Some(item)
}

fn tool_blocks(e: &Value) -> Vec<&Value> {
    let wanted = match get_str(Some(e), "type") {
        Some("assistant") => "tool_use",
        Some("user") => "tool_result",
        _ => return Vec::new(),
    };
    match get(get(Some(e), "message"), "content") {
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| get_str(Some(part), "type") == Some(wanted))
            .collect(),
        _ => Vec::new(),
    }
}

type Lookup<'a> = &'a dyn Fn(Option<&Value>) -> Option<String>;

fn tool_activity(e: &Value, blocks: &[&Value], lookup: Option<Lookup>) -> Activity {
    let started = get_str(Some(e), "type") == Some("assistant");
    let name_of = |b: &Value| {
        if started {
            tool_label(get(Some(b), "name"))
        } else {
            lookup
                .and_then(|f| f(get(Some(b), "tool_use_id")))
                .unwrap_or_else(|| "工具".to_owned())
        }
    };
    let mut names: Vec<String> = Vec::new();
    for b in blocks {
        let name = name_of(b);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let tool = names.join("、");
    let count = if blocks.len() > 1 {
        format!("（{} 项）", blocks.len())
    } else {
        String::new()
    };
    let errored = |b: &Value| is_true(get(Some(b), "is_error"));
    let failed = !started && blocks.iter().any(|b| errored(b));
    let label = if started {
        format!("调用 {tool}")
    } else {
        format!("{tool}{}", if failed { "返回错误" } else { "已返回" })
    };
    let state = if started {
        "running"
    } else if failed {
        "failed"
    } else {
        "completed"
    };
    let mut item = Activity::new(label + &count, "tool", state).with_tool(tool);
    if blocks.len() > 1 {
        item.steps = Some(
            blocks
                .iter()
                .map(|b| Step {
                    tool: name_of(b),
                    state: if started {
                        "running"
                    } else if errored(b) {
                        "failed"
                    } else {
                        "completed"
                    },
                })
                .collect(),
        );
    }
    item
}

/// A journal row as seen by the projection (`progress.cjs` row shape).
#[derive(Clone, Debug)]
pub(crate) struct NativeRow<'a> {
    pub seq: i64,
    pub job: Option<i64>,
    pub turn: Option<i64>,
    pub ts: f64,
    pub kind: &'a str,
    pub payload: Option<&'a str>,
}

/// `eventFor(row)`
fn event_for(row: &NativeRow) -> Option<Value> {
    if !row.kind.is_empty() && row.kind != "native" {
        return None;
    }
    let e: Value = serde_json::from_str(row.payload?).ok()?;
    e.is_object().then_some(e)
}

fn classify(e: &Value, blocks: &[&Value], lookup: Option<Lookup>) -> Option<Activity> {
    let ev = Some(e);
    let kind = get_str(ev, "type");
    if kind == Some("system") {
        let subtype = get_str(ev, "subtype");
        if subtype == Some("init") {
            return Some(Activity::new("Claude Code 已启动", "run", "running"));
        }
        if subtype == Some("api_retry") {
            return Some(Activity::new("等待模型请求重试", "run", "running"));
        }
        if subtype == Some("status") && get_str(ev, "status") == Some("compacting") {
            return Some(Activity::new("正在整理会话上下文", "run", "running"));
        }
        if let Some(task) = task_activity(e) {
            return Some(task);
        }
    }
    if !blocks.is_empty() {
        return Some(tool_activity(e, blocks, lookup));
    }
    if kind == Some("tool_progress")
        && !truthy(get(ev, "heartbeat"))
        && let Some(_) = get_str(ev, "tool_name")
    {
        let tool = tool_label(get(ev, "tool_name"));
        let label = if truthy(get(ev, "subagent_retry")) {
            "子代理等待模型请求重试".to_owned()
        } else {
            format!("{tool} 执行中")
        };
        return Some(Activity::new(label, "tool", "running").with_tool(tool));
    }
    if kind == Some("result")
        && (matches!(get(ev, "is_error"), Some(Value::Bool(_)))
            || get_str(ev, "subtype") == Some("success"))
    {
        let ok = !truthy(get(ev, "is_error")) && get_str(ev, "subtype") == Some("success");
        return Some(if ok {
            Activity::new("原生轮次已完成", "run", "completed")
        } else {
            Activity::new("原生轮次失败", "run", "failed")
        });
    }
    harness_activity(e)
}

/// `activityFor(row)`: stateless projection of a single row.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn activity_for(row: &NativeRow) -> Option<Activity> {
    let e = event_for(row)?;
    classify(&e, &tool_blocks(&e), None)
}

/// `[job, turn, id]` correlation key.
type Key = (Option<i64>, Option<i64>, Option<String>);

#[derive(Clone, Debug, PartialEq)]
struct Task {
    id: String,
    job: Option<i64>,
    turn: Option<i64>,
    label: String,
    state: &'static str,
    last_tool: String,
    ts: f64,
    title: Option<String>,
    tool_uses: Option<i64>,
}

struct Rec {
    task: Task,
    run: Option<String>,
}

struct Link {
    rec: u64,
    run: Option<String>,
}

/// Incremental native projection (`createActivity()` state plus its private index).
#[derive(Default)]
pub(crate) struct NativeState {
    recs: HashMap<u64, Rec>,
    /// Task order (`state.tasks`).
    order: Vec<u64>,
    next_rec: u64,
    tasks: HashMap<Key, u64>,
    /// `None` marks an ambiguous tool-use link (JS `null`).
    parents: HashMap<Key, Option<Link>>,
    calls: HashMap<Key, String>,
    backgrounds: HashMap<Key, HashSet<Key>>,
    ambient: HashSet<Key>,
    seq: i64,
    latest: Option<LatestActivity>,
}

struct Ctx {
    job: Option<i64>,
    turn: Option<i64>,
    ts: f64,
}

impl Ctx {
    fn key(&self, id: Option<&str>) -> Key {
        (self.job, self.turn, id.map(str::to_owned))
    }
}

impl NativeState {
    /// `createActivity()`
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot as the public contract type (`{tasks, latest}`).
    pub fn view(&self) -> NativeActivity {
        NativeActivity {
            tasks: self
                .order
                .iter()
                .filter_map(|id| self.recs.get(id))
                .map(|rec| {
                    let t = &rec.task;
                    NativeTask {
                        id: t.id.clone(),
                        job: t.job.unwrap_or(0),
                        turn: t.turn.unwrap_or(0),
                        label: t.label.clone(),
                        state: t.state.to_owned(),
                        last_tool: t.last_tool.clone(),
                        ts: t.ts,
                        title: t.title.clone(),
                        tool_uses: t.tool_uses,
                    }
                })
                .collect(),
            latest: self.latest.clone(),
        }
    }

    fn update_ambient(&mut self, ctx: &Ctx, id: &str, ambient: Option<&Value>) {
        let scoped = ctx.key(Some(id));
        if matches!(ambient, Some(Value::Bool(false))) {
            self.ambient.remove(&scoped);
        }
        if !is_true(ambient) {
            return;
        }
        self.ambient.insert(scoped.clone());
        if let Some(rid) = self.tasks.remove(&scoped) {
            self.order.retain(|r| *r != rid);
            self.recs.remove(&rid);
            self.parents
                .retain(|_, link| !matches!(link, Some(l) if l.rec == rid));
        }
    }

    fn upsert(
        &mut self,
        ctx: &Ctx,
        native: &Value,
        id: &str,
        next_state: &'static str,
    ) -> Option<u64> {
        let scoped = ctx.key(Some(id));
        if self.ambient.contains(&scoped) {
            return None;
        }
        let existing = self.tasks.get(&scoped).copied();
        let run = native_id(get(Some(native), "run_id"));
        let rec_run = existing
            .and_then(|r| self.recs.get(&r))
            .and_then(|r| r.run.clone());
        // The SDK guarantees lexical start order for the run IDs of a single resumed task.
        if let (Some(run), Some(prev)) = (&run, &rec_run)
            && run < prev
        {
            return None;
        }
        let resumed = matches!((&run, &rec_run), (Some(a), Some(b)) if a > b);
        if let Some(rec) = existing.and_then(|r| self.recs.get(&r))
            && !resumed
            && terminal(rec.task.state)
            && !terminal(next_state)
        {
            return None;
        }
        let rid = match existing {
            None => {
                let rid = self.next_rec;
                self.next_rec += 1;
                let task = Task {
                    id: id.to_owned(),
                    job: ctx.job,
                    turn: ctx.turn,
                    label: task_label(native),
                    state: next_state,
                    last_tool: String::new(),
                    ts: ctx.ts,
                    title: None,
                    tool_uses: None,
                };
                self.recs.insert(
                    rid,
                    Rec {
                        task,
                        run: run.clone(),
                    },
                );
                self.tasks.insert(scoped, rid);
                self.order.push(rid);
                rid
            }
            Some(rid) => {
                if resumed && let Some(rec) = self.recs.get_mut(&rid) {
                    rec.task.last_tool.clear();
                    rec.task.tool_uses = None;
                }
                rid
            }
        };
        let rec = self.recs.get_mut(&rid)?;
        if run.is_some() {
            rec.run = run;
        }
        if let Some(title) = task_title(native) {
            rec.task.title = Some(title);
        }
        rec.task.state = next_state;
        rec.task.ts = ctx.ts;
        if role(get(Some(native), "subagent_type")).is_some() || rec.task.label == "原生任务" {
            rec.task.label = task_label(native);
        }
        if let Some(parent) = native_id(get(Some(native), "tool_use_id")) {
            let alias = ctx.key(Some(&parent));
            let link = Link {
                rec: rid,
                run: rec.run.clone(),
            };
            // An ambiguous tool-use link cannot establish ownership of later nested calls.
            let value = match self.parents.get(&alias) {
                None => Some(link),
                Some(Some(prev)) if prev.rec == rid => Some(link),
                _ => None,
            };
            self.parents.insert(alias, value);
        }
        Some(rid)
    }

    /// `applyActivity(state, row)`: consume rows in increasing sequence order.
    pub fn apply(&mut self, row: &NativeRow) -> Option<Activity> {
        if row.seq <= self.seq {
            return None;
        }
        self.seq = row.seq;
        let e = event_for(row)?;
        let ev = Some(&e);
        let ctx = Ctx {
            job: row.job,
            turn: row.turn,
            ts: row.ts,
        };
        let kind = get_str(ev, "type");
        if kind == Some("system")
            && get_str(ev, "subtype") == Some("background_tasks_changed")
            && let Some(Value::Array(entries)) = get(ev, "tasks")
        {
            let scope = ctx.key(None);
            let mut live = HashSet::new();
            for entry in entries {
                let Some(id) = native_id(get(Some(entry), "task_id")) else {
                    continue;
                };
                self.update_ambient(&ctx, &id, get(Some(entry), "ambient"));
                let scoped = ctx.key(Some(&id));
                live.insert(scoped.clone());
                let run_id = get(Some(entry), "run_id");
                let refresh = match self.tasks.get(&scoped).and_then(|r| self.recs.get(r)) {
                    None => true,
                    Some(rec) => {
                        rec.task.state == "unknown"
                            || (truthy(run_id)
                                && !matches!((run_id.and_then(Value::as_str), rec.run.as_deref()),
                                    (Some(a), Some(b)) if a == b))
                    }
                };
                if refresh {
                    self.upsert(&ctx, entry, &id, "running");
                }
            }
            // Absence from a new full snapshot ends observed liveness, without supplying a terminal outcome.
            if let Some(previous) = self.backgrounds.get(&scope) {
                for scoped in previous {
                    if live.contains(scoped) {
                        continue;
                    }
                    if let Some(rec) = self.tasks.get(scoped).and_then(|r| self.recs.get_mut(r))
                        && rec.task.state == "running"
                    {
                        rec.task.state = "unknown";
                        rec.task.ts = ctx.ts;
                    }
                }
            }
            self.backgrounds.insert(scope, live);
            return None;
        }
        if kind == Some("system")
            && let Some(id) = native_id(get(ev, "task_id"))
        {
            self.update_ambient(&ctx, &id, get(ev, "ambient"));
        }
        let blocks = tool_blocks(&e);
        let mut activity = {
            let calls = &self.calls;
            let lookup =
                |id: Option<&Value>| calls.get(&ctx.key(native_id(id).as_deref())).cloned();
            classify(&e, &blocks, Some(&lookup))?
        };
        if activity.category == "task"
            && let Some(task_id) = activity.task_id.clone()
        {
            let rid = self.upsert(&ctx, &e, &task_id, activity.state)?;
            let task = &mut self.recs.get_mut(&rid)?.task;
            if let Some(uses) = safe_integer(get(get(ev, "usage"), "tool_uses"))
                && uses >= 0
            {
                task.tool_uses = Some(task.tool_uses.unwrap_or(0).max(uses));
            }
            if let Some(tool) = &activity.tool {
                task.last_tool = tool.clone();
            }
            if let Some(title) = &task.title {
                activity.title = Some(title.clone());
            }
            activity.label = activity.label.replacen(&task_label(&e), &task.label, 1);
        } else if !blocks.is_empty() || kind == Some("tool_progress") {
            let link_key = ctx.key(native_id(get(ev, "parent_tool_use_id")).as_deref());
            let owner = match self.parents.get(&link_key) {
                Some(Some(link)) => self
                    .recs
                    .get(&link.rec)
                    .filter(|rec| rec.run == link.run)
                    .map(|_| link.rec),
                _ => None,
            };
            for block in &blocks {
                let b = Some(*block);
                let raw = if truthy(get(b, "id")) {
                    get(b, "id")
                } else {
                    get(b, "tool_use_id")
                };
                let Some(id) = native_id(raw) else { continue };
                if get_str(b, "type") == Some("tool_use") {
                    self.calls
                        .insert(ctx.key(Some(&id)), tool_label(get(b, "name")));
                } else {
                    self.calls.remove(&ctx.key(Some(&id)));
                }
            }
            if let Some(task) = owner
                .and_then(|rid| self.recs.get_mut(&rid))
                .map(|r| &mut r.task)
            {
                activity.task_id = Some(task.id.clone());
                if !terminal(task.state) {
                    task.last_tool = activity.tool.clone().unwrap_or_default();
                    task.ts = ctx.ts;
                }
            }
        }
        self.latest = Some(LatestActivity {
            job: row.job.unwrap_or(0),
            turn: row.turn.unwrap_or(0),
            label: activity.label.clone(),
            ts: row.ts,
        });
        Some(activity)
    }
}

#[cfg(test)]
mod tests {
    //! Port of `tests-node/native-activity.test.cjs`.
    use super::*;
    use serde_json::json;

    struct Owned {
        seq: i64,
        job: i64,
        turn: i64,
        kind: &'static str,
        payload: String,
    }

    impl Owned {
        fn row(&self) -> NativeRow<'_> {
            NativeRow {
                seq: self.seq,
                job: Some(self.job),
                turn: Some(self.turn),
                ts: 1000.0 + self.seq as f64,
                kind: self.kind,
                payload: Some(&self.payload),
            }
        }
    }

    fn row_at(seq: i64, payload: Value, job: i64, turn: i64) -> Owned {
        Owned {
            seq,
            job,
            turn,
            kind: "native",
            payload: payload.to_string(),
        }
    }
    fn row(seq: i64, payload: Value) -> Owned {
        row_at(seq, payload, 15, 1)
    }
    fn task(subtype: &str, fields: Value) -> Value {
        let mut v = json!({"type":"system","subtype":subtype});
        for (k, val) in fields.as_object().unwrap() {
            v[k] = val.clone();
        }
        v
    }
    fn merge(base: &Value, fields: Value) -> Value {
        let mut v = base.clone();
        for (k, val) in fields.as_object().unwrap() {
            v[k] = val.clone();
        }
        v
    }
    fn started() -> Value {
        task(
            "task_started",
            json!({"task_id":"a-one","tool_use_id":"tool-start","task_type":"local_agent",
                   "subagent_type":"Explore","description":"PRIVATE","prompt":"PRIVATE"}),
        )
    }
    fn tool(kind: &str, content: Value, parent: Option<&str>) -> Value {
        let mut v = json!({"type":kind,"message":{"content":content}});
        if let Some(p) = parent {
            v["parent_tool_use_id"] = json!(p);
        }
        v
    }
    fn command() -> Value {
        json!({"type":"tool_use","name":"Bash","id":"cmd-1","input":{"command":"PRIVATE"}})
    }
    fn result() -> Value {
        json!({"type":"tool_result","tool_use_id":"cmd-1","content":"PRIVATE"})
    }
    fn apply(state: &mut NativeState, r: Owned) -> Option<Activity> {
        state.apply(&r.row())
    }
    fn project(rows: &[Owned]) -> NativeState {
        let mut s = NativeState::new();
        for r in rows {
            s.apply(&r.row());
        }
        s
    }
    fn dump(state: &NativeState) -> String {
        serde_json::to_string(&state.view()).unwrap()
    }

    #[test]
    fn task_cards_use_short_title_without_prompt_or_shell_input() {
        let mut state = NativeState::new();
        let event = apply(
            &mut state,
            row(
                1,
                merge(&started(), json!({"description":"Audit gameplay loop"})),
            ),
        )
        .unwrap();
        assert_eq!(event.title.as_deref(), Some("Audit gameplay loop"));
        assert_eq!(
            state.view().tasks[0].title.as_deref(),
            Some("Audit gameplay loop")
        );
        apply(
            &mut state,
            row(
                2,
                merge(
                    &started(),
                    json!({"task_id":"second","description":"read C:\\private\\file.json"}),
                ),
            ),
        );
        assert_eq!(state.view().tasks[1].title, None);
        assert!(!dump(&state).contains("PRIVATE"));
    }

    #[test]
    fn chinese_titles_are_kept_and_paths_rejected() {
        let title = |d: &str| task_title(&merge(&started(), json!({ "description": d })));
        assert_eq!(title("  审查 构建流程  ").as_deref(), Some("审查 构建流程"));
        assert_eq!(title("审查"), Some("审查".to_owned()));
        assert_eq!(title("check /etc/passwd"), None);
        assert_eq!(title("a//b c"), None);
        assert_eq!(title(&format!("x {}", "a".repeat(32))), None);
        assert_eq!(title("NoSpaceAscii"), None);
        assert_eq!(title(&"审".repeat(141)), None);
    }

    #[test]
    fn background_agent_results_do_not_complete_tasks() {
        let mut state = NativeState::new();
        apply(
            &mut state,
            row(
                1,
                tool(
                    "assistant",
                    json!([{"type":"tool_use","name":"Agent","id":"tool-start"}]),
                    None,
                ),
            ),
        );
        assert_eq!(state.view().tasks.len(), 0);
        apply(
            &mut state,
            row(
                2,
                task("background_tasks_changed", json!({ "tasks": [started()] })),
            ),
        );
        apply(&mut state, row(3, started()));
        apply(
            &mut state,
            row(
                4,
                tool(
                    "user",
                    json!([{"type":"tool_result","tool_use_id":"tool-start"}]),
                    None,
                ),
            ),
        );
        assert_eq!(state.view().tasks[0].state, "running");
        let five = apply(
            &mut state,
            row(5, tool("assistant", json!([command()]), Some("tool-start"))),
        )
        .unwrap();
        assert_eq!(five.task_id.as_deref(), Some("a-one"));
        let reply = apply(
            &mut state,
            row(6, tool("user", json!([result()]), Some("tool-start"))),
        )
        .unwrap();
        assert_eq!(reply.task_id.as_deref(), Some("a-one"));
        assert_eq!(reply.tool.as_deref(), Some("Bash"));
        apply(
            &mut state,
            row(
                7,
                task(
                    "task_progress",
                    json!({"task_id":"a-one","last_tool_name":"Read","usage":{"tool_uses":2}}),
                ),
            ),
        );
        apply(
            &mut state,
            row(
                8,
                task(
                    "task_updated",
                    json!({"task_id":"a-one","patch":{"status":"completed"}}),
                ),
            ),
        );
        apply(
            &mut state,
            row(
                9,
                task(
                    "task_notification",
                    json!({"task_id":"a-one","status":"completed","summary":"PRIVATE"}),
                ),
            ),
        );
        assert_eq!(
            apply(
                &mut state,
                row(
                    10,
                    task(
                        "task_progress",
                        json!({"task_id":"a-one","last_tool_name":"Bash"})
                    )
                )
            ),
            None
        );
        assert_eq!(
            state.view().tasks,
            vec![NativeTask {
                id: "a-one".into(),
                job: 15,
                turn: 1,
                state: "completed".into(),
                ts: 1009.0,
                label: "子代理 · Explore".into(),
                last_tool: "Read".into(),
                title: None,
                tool_uses: Some(2),
            }]
        );
        assert_eq!(state.view().latest.unwrap().ts, 1009.0);
        let all = format!("{}{}", dump(&state), reply.to_value());
        for secret in ["PRIVATE", "tool-start", "cmd-1"] {
            assert!(!all.contains(secret), "{secret} leaked: {all}");
        }
    }

    #[test]
    fn identity_is_scoped_by_job_and_turn() {
        let state = project(&[
            row_at(1, started(), 15, 1),
            row_at(2, started(), 16, 1),
            row_at(3, started(), 15, 2),
            row(4, task("background_tasks_changed", json!({"tasks":[]}))),
            row_at(
                5,
                task(
                    "task_notification",
                    json!({"task_id":"a-one","status":"failed"}),
                ),
                16,
                1,
            ),
            row_at(
                6,
                task(
                    "task_updated",
                    json!({"task_id":"a-one","patch":{"status":"stopped"}}),
                ),
                15,
                2,
            ),
        ]);
        let states: Vec<_> = state.view().tasks.into_iter().map(|t| t.state).collect();
        assert_eq!(states, ["running", "failed", "stopped"]);
    }

    #[test]
    fn only_whitelisted_names_and_batched_errors() {
        let mut state = NativeState::new();
        let r = row(
            1,
            task(
                "task_started",
                merge(&started(), json!({"subagent_type":"PRIVATE"})),
            ),
        );
        assert_eq!(activity_for(&r.row()).unwrap().label, "子代理已启动");
        let mcp = merge(
            &command(),
            json!({"name":"mcp__PRIVATE__tool","id":"cmd-2"}),
        );
        let activity = apply(
            &mut state,
            row(1, tool("assistant", json!([command(), mcp]), None)),
        )
        .unwrap();
        assert!(
            activity.label.contains("Bash、MCP 工具"),
            "{}",
            activity.label
        );
        assert!(activity.label.contains("2 项"));
        let outcome = apply(
            &mut state,
            row(
                2,
                tool(
                    "user",
                    json!([
                        result(),
                        merge(&result(), json!({"tool_use_id":"cmd-2","is_error":true}))
                    ]),
                    None,
                ),
            ),
        )
        .unwrap();
        assert_eq!(outcome.state, "failed");
        assert_eq!(outcome.label, "Bash、MCP 工具返回错误（2 项）");
        assert_eq!(state.view().tasks.len(), 0);
        let all = format!(
            "{}{}{}",
            dump(&state),
            activity.to_value(),
            outcome.to_value()
        );
        assert!(!all.contains("PRIVATE"));
    }

    #[test]
    fn malformed_reasoning_and_identityless_rows_are_omitted() {
        let raw = |kind: &'static str, payload: &str| Owned {
            seq: 0,
            job: 15,
            turn: 1,
            kind,
            payload: payload.to_owned(),
        };
        let cases = vec![
            raw("native", "{"),
            raw("native", "null"),
            raw("delta", "PRIVATE"),
            row(
                1,
                tool(
                    "assistant",
                    json!([{"type":"thinking","thinking":"PRIVATE"},{"type":"text","text":"PRIVATE"}]),
                    None,
                ),
            ),
            row(
                2,
                json!({"type":"stream_event","event":{"delta":{"type":"input_json_delta","partial_json":"PRIVATE"}}}),
            ),
            row(3, task("task_progress", json!({"description":"PRIVATE"}))),
            row(4, task("thinking_tokens", json!({"tokens":12}))),
            row(
                5,
                json!({"type":"tool_progress","tool_name":"Bash","heartbeat":true}),
            ),
        ];
        for r in &cases {
            assert_eq!(activity_for(&r.row()), None);
        }
        let projected = project(&cases).view();
        assert_eq!(
            projected,
            NativeActivity {
                tasks: vec![],
                latest: None
            }
        );
        let unknown = row(
            5,
            task(
                "task_notification",
                json!({"task_id":"a","status":"unexpected"}),
            ),
        );
        assert_eq!(activity_for(&unknown.row()).unwrap().state, "unknown");
        let ok = row(
            6,
            json!({"type":"result","subtype":"success","result":"PRIVATE"}),
        );
        assert_eq!(activity_for(&ok.row()).unwrap().state, "completed");
        let failed = row(
            7,
            json!({"type":"result","subtype":"error_during_execution","is_error":true}),
        );
        assert_eq!(activity_for(&failed.row()).unwrap().state, "failed");
    }

    #[test]
    fn backoff_compaction_and_tool_progress_are_safe() {
        let mut state = NativeState::new();
        apply(&mut state, row(1, started()));
        let event = json!({"type":"tool_progress","tool_name":"Bash","parent_tool_use_id":"tool-start",
                           "subagent_retry":{"error":"PRIVATE"}});
        let latest = apply(&mut state, row(2, event)).unwrap();
        assert_eq!(latest.task_id.as_deref(), Some("a-one"));
        assert_eq!(state.view().tasks[0].last_tool, "Bash");
        assert!(latest.label.contains("重试"));
        assert!(!latest.to_value().to_string().contains("PRIVATE"));
        let compact = row(3, task("status", json!({"status":"compacting"})));
        assert!(
            activity_for(&compact.row())
                .unwrap()
                .label
                .contains("上下文")
        );
        let retry = row(4, task("api_retry", json!({})));
        assert!(activity_for(&retry.row()).unwrap().label.contains("重试"));
    }

    #[test]
    fn resumed_task_ignores_late_events_from_earlier_run() {
        let mut state = NativeState::new();
        apply(&mut state, row(1, merge(&started(), json!({"run_id":"1"}))));
        apply(
            &mut state,
            row(
                2,
                task(
                    "task_progress",
                    json!({"task_id":"a-one","run_id":"1","last_tool_name":"Bash","usage":{"tool_uses":8}}),
                ),
            ),
        );
        apply(
            &mut state,
            row(
                3,
                task(
                    "task_notification",
                    json!({"task_id":"a-one","run_id":"1","status":"completed"}),
                ),
            ),
        );
        apply(
            &mut state,
            row(
                4,
                merge(&started(), json!({"run_id":"2","tool_use_id":"new-start"})),
            ),
        );
        assert_eq!(
            apply(
                &mut state,
                row(
                    5,
                    task(
                        "task_notification",
                        json!({"task_id":"a-one","run_id":"1","status":"failed"})
                    )
                )
            ),
            None
        );
        let tasks = state.view().tasks;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].state, "running");
        assert_eq!(tasks[0].last_tool, "");
        assert_eq!(tasks[0].tool_uses, None);
        apply(
            &mut state,
            row(
                6,
                task(
                    "task_notification",
                    json!({"task_id":"a-one","status":"completed"}),
                ),
            ),
        );
        assert_eq!(state.view().tasks[0].state, "completed");
    }

    #[test]
    fn ambient_tasks_hidden_and_removals_lose_running_status() {
        let snap = |tasks: Value| task("background_tasks_changed", json!({ "tasks": tasks }));
        let mut state = project(&[
            row(
                1,
                snap(json!([started(), {"task_id":"watcher","ambient":true}])),
            ),
            row(2, started()),
            row(
                3,
                task(
                    "task_progress",
                    json!({"task_id":"watcher","usage":{"tool_uses":8}}),
                ),
            ),
            row(4, snap(json!([]))),
        ]);
        let tasks = state.view().tasks;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].state, "unknown");
        apply(
            &mut state,
            row(5, snap(json!([merge(&started(), json!({"ambient":true}))]))),
        );
        assert_eq!(state.view().tasks.len(), 0);
        apply(
            &mut state,
            row(
                6,
                snap(json!([merge(&started(), json!({"ambient":false}))])),
            ),
        );
        assert_eq!(state.view().tasks[0].state, "running");
    }

    #[test]
    fn shared_parent_id_does_not_misattribute_nested_work() {
        let mut state = NativeState::new();
        apply(&mut state, row(1, started()));
        apply(
            &mut state,
            row(2, merge(&started(), json!({"task_id":"a-two"}))),
        );
        let event = apply(
            &mut state,
            row(3, tool("assistant", json!([command()]), Some("tool-start"))),
        )
        .unwrap();
        assert_eq!(event.task_id, None);
        let tools: Vec<_> = state
            .view()
            .tasks
            .into_iter()
            .map(|t| t.last_tool)
            .collect();
        assert_eq!(tools, ["", ""]);
    }

    #[test]
    fn harness_rows_flow_through_native_projection() {
        let mut state = NativeState::new();
        let a = apply(
            &mut state,
            row(
                1,
                json!({"method":"turn/started","params":{"turn":{"id":"t"}}}),
            ),
        )
        .unwrap();
        assert_eq!(a.label, "Codex 轮次执行中");
        let b = apply(
            &mut state,
            row(2, json!({"type":"tool_call","callId":"c1","tool":"grep"})),
        )
        .unwrap();
        assert_eq!(b.label, "搜索内容 · 执行中");
        assert_eq!(state.view().latest.unwrap().label, "搜索内容 · 执行中");
    }
}
