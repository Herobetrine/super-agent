//! Platform operation receipts and restart reconciliation.
//!
//! A covered side effect (Git commands the platform issues, publication,
//! platform-owned check processes) first persists an `operations` row in
//! `started`, then performs the effect, then finishes the row with a known
//! outcome, in the same transaction as the existing result write where one
//! exists. On start-up every row still `started`/`cancel_requested` is
//! classified by inspecting postconditions. The effect is never re-run.
//!
//! Every state transition of an operation that belongs to a ticket logs one
//! event of kind `operation.<state>`, payload `{operation, kind, state,
//! detail}`. Operations created before their ticket exists (project Git
//! bootstrap of a new ticket) are attached to the ticket afterwards without
//! back-dated events; the row itself is the receipt.
//!
//! An `unknown_effect` never blocks [`Engine::cancel`](crate::Engine::cancel),
//! never resumes a stopped ticket and never starts model execution. The
//! operator marks it checked with
//! [`Engine::resolve_operation`](crate::Engine::resolve_operation).
use crate::db::{Db, Q, Who, int_of, now, opt_int, str_of, who_of};
use crate::protocol::{digest, stringify};
use crate::workspaces::{self, Workspace};
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS operations(id INTEGER PRIMARY KEY,ticket INTEGER,job INTEGER,kind TEXT NOT NULL,
 intent TEXT NOT NULL,intent_sha256 TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN('planned','started','confirmed','failed_before_effect','unknown_effect','cancel_requested','cancelled','resolved')),
 attempts INTEGER NOT NULL DEFAULT 0,detail TEXT,created REAL NOT NULL,updated REAL NOT NULL);
CREATE INDEX IF NOT EXISTS operations_open ON operations(state,id) WHERE state IN('planned','started','cancel_requested','unknown_effect');
CREATE INDEX IF NOT EXISTS operations_ticket ON operations(ticket,id);";

pub const PLANNED: &str = "planned";
pub const STARTED: &str = "started";
pub const CONFIRMED: &str = "confirmed";
pub const FAILED_BEFORE_EFFECT: &str = "failed_before_effect";
pub const UNKNOWN_EFFECT: &str = "unknown_effect";
pub const CANCEL_REQUESTED: &str = "cancel_requested";
pub const CANCELLED: &str = "cancelled";
pub const RESOLVED: &str = "resolved";

/// Operation kinds. The intent of each holds only what reconciliation needs:
/// paths, branches and commit ids, never prompts, arguments or secrets.
pub mod kind {
    /// `git init` of a project directory. Intent `{directory}`.
    pub const PROJECT_INIT: &str = "project.git_init";
    /// Initial project commit. Intent `{root}`.
    pub const PROJECT_COMMIT: &str = "project.initial_commit";
    /// `git worktree add -b`. Intent `{job, source, path, branch, base}`.
    pub const WORKTREE_ADD: &str = "workspace.worktree_add";
    /// Prerequisite merge into a dependent writer worktree.
    /// Intent `{job, path, branch, pre_head, merge_head, prerequisite}`.
    pub const DEPENDENCY_MERGE: &str = "git.dependency_merge";
    /// Checkpoint commit inside an isolated worktree. Intent `{job, path, branch, pre_head}`.
    pub const CHECKPOINT: &str = "git.checkpoint";
    /// Queue candidate preparation (checkpoint, candidate worktree, merge, checks).
    /// Intent `{job, source, baseline, home, prefix}`.
    pub const CANDIDATE: &str = "integration.candidate";
    /// Queue publication by fast-forward into the source repository.
    /// Intent `{job, source, source_ref, baseline, target}`.
    pub const PUBLISH: &str = "integration.publish";
    /// Legacy operator integration (same intent as [`PUBLISH`]).
    pub const LEGACY_INTEGRATE: &str = "workspace.integrate";
    /// Legacy operator validation command. Intent `{job, path, pre_head, argv_sha256}`.
    pub const LEGACY_VALIDATE: &str = "process.validate";
    /// Delivery behavior check process. Intent `{attempt, requirement, snapshot, binding, argv_sha256}`.
    pub const BEHAVIOR: &str = "delivery.behavior_check";
    /// `SA_TOOL` MCP call found `pending` after a restart. Intent `{job, logical, key, server, tool, request_sha256}`.
    pub const MCP: &str = "mcp.tool_call";
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Operation {
    pub id: i64,
    pub ticket: Option<i64>,
    pub job: Option<i64>,
    pub kind: String,
    pub intent: Value,
    pub intent_sha256: String,
    pub state: String,
    pub attempts: i64,
    pub detail: Value,
    pub created: f64,
    pub updated: f64,
}

impl Operation {
    fn from_row(r: &serde_json::Map<String, Value>) -> Result<Self> {
        Ok(Self {
            id: int_of(r, "id"),
            ticket: opt_int(r, "ticket"),
            job: opt_int(r, "job"),
            kind: str_of(r, "kind").into(),
            intent: serde_json::from_str(str_of(r, "intent"))?,
            intent_sha256: str_of(r, "intent_sha256").into(),
            state: str_of(r, "state").into(),
            attempts: int_of(r, "attempts"),
            detail: r
                .get("detail")
                .and_then(Value::as_str)
                .map(serde_json::from_str)
                .transpose()?
                .unwrap_or(Value::Null),
            created: r.get("created").and_then(Value::as_f64).unwrap_or_default(),
            updated: r.get("updated").and_then(Value::as_f64).unwrap_or_default(),
        })
    }

    pub fn open(&self) -> bool {
        matches!(self.state.as_str(), PLANNED | STARTED | CANCEL_REQUESTED)
    }
}

/// Ticket and job an operation belongs to. Both are optional: a new ticket's
/// project bootstrap runs before its ticket exists.
#[derive(Clone, Copy, Debug, Default)]
pub struct Owner {
    pub ticket: Option<i64>,
    pub job: Option<i64>,
}

impl Owner {
    pub fn job(ticket: i64, job: i64) -> Self {
        Self { ticket: Some(ticket), job: Some(job) }
    }
    pub fn ticket(ticket: i64) -> Self {
        Self { ticket: Some(ticket), job: None }
    }
}

// ---------------------------------------------------------------------------
// Process and spawn markers (error contexts).

/// The child process was never started; no user code ran.
#[derive(Debug)]
pub(crate) struct NotStarted;
impl std::fmt::Display for NotStarted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("无法启动验证命令或 Git")
    }
}

/// A started process tree could not be confirmed terminated, or its outcome
/// was lost after it ran.
#[derive(Debug)]
pub(crate) struct ProcessUnconfirmed;
impl std::fmt::Display for ProcessUnconfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("无法确认验证进程已终止或取得其结果")
    }
}

// ---------------------------------------------------------------------------
// Fault injection (test seam). `EngineOptions::failpoints` is `None` in
// production; every check is then a single branch.

/// Action of a failpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Return an ordinary error at this point; the normal error path runs.
    Error,
    /// Simulate the process dying here: return an error and suppress every
    /// later journal write of the affected operation scope.
    Crash,
}

/// Named failpoints `"<kind>.<stage>"` where stage is `begin` (before the
/// receipt is written), `after_begin` (receipt written, before the effect),
/// `after_effect` (effect done, before the result) or `receipt` (inside the
/// successful result transaction).
#[derive(Default, Debug)]
pub struct Failpoints {
    points: Mutex<HashMap<String, Fault>>,
    hits: Mutex<Vec<String>>,
}

impl Failpoints {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }
    pub fn set(&self, name: &str, fault: Fault) {
        self.points.lock().unwrap_or_else(|e| e.into_inner()).insert(name.into(), fault);
    }
    pub fn clear(&self, name: &str) {
        self.points.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
    }
    /// Names of failpoints that fired, in order.
    pub fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn fire(&self, name: &str) -> Option<Fault> {
        let fault = self.points.lock().unwrap_or_else(|e| e.into_inner()).get(name).copied()?;
        self.hits.lock().unwrap_or_else(|e| e.into_inner()).push(name.into());
        Some(fault)
    }
}

/// Error returned by a [`Fault::Crash`] failpoint.
#[derive(Debug)]
pub struct SimulatedCrash(pub String);
impl std::fmt::Display for SimulatedCrash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "故障注入：模拟程序在 {} 处退出", self.0)
    }
}
impl std::error::Error for SimulatedCrash {}

// ---------------------------------------------------------------------------
// In-process registry of operations being executed by this engine.

struct Live {
    ticket: Option<i64>,
    job: Option<i64>,
    cancel: Option<Arc<AtomicBool>>,
}

/// Per-engine operation state: failpoints and the operations this process
/// is currently executing (excluded from reconciliation, cancellable).
#[derive(Default)]
pub(crate) struct Hub {
    failpoints: Option<Arc<Failpoints>>,
    live: Mutex<HashMap<i64, Live>>,
}

impl Hub {
    pub(crate) fn new(failpoints: Option<Arc<Failpoints>>) -> Self {
        Self { failpoints, live: Mutex::default() }
    }

    fn live(&self) -> std::sync::MutexGuard<'_, HashMap<i64, Live>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn fault(&self, name: impl FnOnce() -> String) -> Result<()> {
        let Some(points) = &self.failpoints else { return Ok(()) };
        let name = name();
        match points.fire(&name) {
            None => Ok(()),
            Some(Fault::Error) => Err(anyhow!("故障注入：{name}")),
            Some(Fault::Crash) => Err(SimulatedCrash(name).into()),
        }
    }

    fn is_live(&self, id: i64) -> bool {
        self.live().contains_key(&id)
    }

    fn live_job(&self, job: i64) -> bool {
        self.live().values().any(|l| l.job == Some(job))
    }

    /// Live cancellable operations of a ticket.
    pub(crate) fn cancellable_for(&self, ticket: i64) -> Vec<i64> {
        let mut ids: Vec<i64> = self
            .live()
            .iter()
            .filter(|(_, l)| l.ticket == Some(ticket) && l.cancel.is_some())
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Signal the owned processes; the operation finishes `cancelled` only
    /// once their termination is confirmed.
    pub(crate) fn signal(&self, ids: &[i64]) {
        for id in ids {
            if let Some(flag) = self.live().get(id).and_then(|l| l.cancel.clone()) {
                flag.store(true, Ordering::SeqCst);
            }
        }
    }

    /// Write verdicts inside the caller's transaction. `receipt` marks the
    /// successful result transaction, where the `<kind>.receipt` failpoint fires.
    pub(crate) fn record(&self, q: &Q, verdicts: &[Verdict], receipt: bool) -> Result<()> {
        for v in verdicts {
            if receipt {
                self.fault(|| format!("{}.receipt", v.kind))?;
            }
            finish(q, v.id, v.state, &v.detail)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Journal API.

fn who(q: &Q, ticket: Option<i64>, job: Option<i64>) -> Result<Option<Who>> {
    let Some(ticket) = ticket else { return Ok(None) };
    let row = match job {
        Some(job) => q.one("SELECT id,ticket,turn FROM jobs WHERE id=? AND ticket=?", &[&job, &ticket])?,
        None => None,
    };
    let row = match row {
        Some(row) => Some(row),
        None => q.one("SELECT id,ticket,turn FROM jobs WHERE ticket=? AND parent IS NULL", &[&ticket])?,
    };
    Ok(row.map(|r| who_of(&r)))
}

fn event(q: &Q, id: i64, state: &str, detail: &Value) -> Result<()> {
    let row = q.one("SELECT ticket,job,kind FROM operations WHERE id=?", &[&id])?
        .ok_or_else(|| anyhow!("操作记录不存在"))?;
    if let Some(who) = who(q, opt_int(&row, "ticket"), opt_int(&row, "job"))? {
        q.log(who, &format!("operation.{state}"),
            &json!({"operation": id, "kind": str_of(&row, "kind"), "state": state, "detail": detail}))?;
    }
    Ok(())
}

/// Persist the intent before the effect. Call in its own small transaction
/// that commits before the effect starts.
pub fn begin(q: &Q, owner: Owner, kind: &str, intent: Value) -> Result<i64> {
    let text = stringify(&intent);
    let at = now();
    let id = q.insert(
        "INSERT INTO operations(ticket,job,kind,intent,intent_sha256,state,attempts,created,updated) VALUES(?,?,?,?,?,'started',1,?,?)",
        &[&owner.ticket, &owner.job, &kind, &text, &digest(&text), &at, &at],
    )?;
    event(q, id, STARTED, &Value::Null)?;
    Ok(id)
}

fn transition(q: &Q, id: i64, state: &str, detail: &Value) -> Result<()> {
    let text = (!detail.is_null()).then(|| stringify(detail));
    q.run("UPDATE operations SET state=?,detail=coalesce(?,detail),updated=? WHERE id=?", &[&state, &text, &now(), &id])?;
    event(q, id, state, detail)
}

/// Record a known outcome. Returns `false` when the operation already has a
/// terminal state (a late duplicate finish never rewrites it). `cancelled`
/// implies a preceding `cancel_requested`.
pub fn finish(q: &Q, id: i64, state: &str, detail: &Value) -> Result<bool> {
    ensure!(
        matches!(state, CONFIRMED | FAILED_BEFORE_EFFECT | UNKNOWN_EFFECT | CANCEL_REQUESTED | CANCELLED),
        "操作状态无效：{state}"
    );
    let current = q.one("SELECT state FROM operations WHERE id=?", &[&id])?
        .ok_or_else(|| anyhow!("操作记录不存在"))?;
    let current = str_of(&current, "state");
    let open = matches!(current, PLANNED | STARTED | CANCEL_REQUESTED);
    if !open || (state == CANCEL_REQUESTED && current != STARTED) {
        return Ok(false);
    }
    if state == CANCELLED && current != CANCEL_REQUESTED {
        transition(q, id, CANCEL_REQUESTED, &Value::Null)?;
    }
    transition(q, id, state, detail)?;
    Ok(true)
}

/// Mark live operations as cancellation requested (before signalling them).
pub(crate) fn request_cancel(q: &Q, ids: &[i64]) -> Result<()> {
    for id in ids {
        finish(q, *id, CANCEL_REQUESTED, &Value::Null)?;
    }
    Ok(())
}

/// Attach operations created before their ticket existed.
pub(crate) fn attach(q: &Q, ids: &[i64], who: Who) -> Result<()> {
    for id in ids {
        q.run("UPDATE operations SET ticket=?,job=? WHERE id=? AND ticket IS NULL", &[&who.ticket, &who.id, id])?;
    }
    Ok(())
}

/// Safe for an observer Q on a journal predating the table.
pub fn list(q: &Q, ticket: i64) -> Result<Vec<Operation>> {
    if !q.exists("SELECT 1 FROM sqlite_master WHERE type='table' AND name='operations'", &[])? {
        return Ok(vec![]);
    }
    q.all("SELECT * FROM operations WHERE ticket=? ORDER BY id", &[&ticket])?
        .iter()
        .map(Operation::from_row)
        .collect()
}

pub fn get(q: &Q, id: i64) -> Result<Option<Operation>> {
    q.one("SELECT * FROM operations WHERE id=?", &[&id])?
        .map(|r| Operation::from_row(&r))
        .transpose()
}

/// The operator checked an unknown effect. Only `unknown_effect` can be
/// resolved; the previous detail is preserved next to the note.
pub fn resolve(q: &Q, id: i64, note: &str) -> Result<()> {
    let note = note.trim();
    ensure!(!note.is_empty() && note.chars().count() <= 2000, "请填写 1-2000 字的核对说明");
    let op = get(q, id)?.ok_or_else(|| anyhow!("操作记录不存在"))?;
    ensure!(op.state == UNKNOWN_EFFECT, "只有结果未知的操作需要人工核对");
    transition(q, id, RESOLVED, &json!({"previous": op.detail, "note": note}))
}

// ---------------------------------------------------------------------------
// Scope: operations begun by one platform action.

#[derive(Clone, Debug)]
pub(crate) struct Verdict {
    pub id: i64,
    pub kind: String,
    pub intent: Value,
    pub state: &'static str,
    pub detail: Value,
}

#[derive(Clone, Debug)]
struct Open {
    id: i64,
    kind: String,
    intent: Value,
}

/// Operations begun by one platform action. Without a journal (`detached`)
/// nothing is recorded and failpoints never fire.
pub(crate) struct Scope<'a> {
    db: Option<&'a Db>,
    hub: Option<&'a Hub>,
    owner: Owner,
    open: Mutex<Vec<Open>>,
    decided: Mutex<Vec<Verdict>>,
    crashed: AtomicBool,
}

impl<'a> Scope<'a> {
    pub(crate) fn detached() -> Scope<'static> {
        Scope { db: None, hub: None, owner: Owner::default(), open: Mutex::default(),
            decided: Mutex::default(), crashed: AtomicBool::new(false) }
    }

    pub(crate) fn new(db: &'a Db, hub: &'a Hub, owner: Owner) -> Self {
        Scope { db: Some(db), hub: Some(hub), owner, open: Mutex::default(),
            decided: Mutex::default(), crashed: AtomicBool::new(false) }
    }

    /// Fire the failpoint `<kind>.<stage>` if configured.
    pub(crate) fn point(&self, kind: &str, stage: &str) -> Result<()> {
        let Some(hub) = self.hub else { return Ok(()) };
        let result = hub.fault(|| format!("{kind}.{stage}"));
        if let Err(error) = &result
            && error.is::<SimulatedCrash>()
        {
            self.crashed.store(true, Ordering::SeqCst);
        }
        result
    }

    /// A failpoint simulated a crash: callers write nothing further.
    pub(crate) fn crashed(&self) -> bool {
        self.crashed.load(Ordering::SeqCst)
    }

    fn track(&self, id: i64, kind: &str, intent: Value) {
        if let Some(hub) = self.hub {
            hub.live().insert(id, Live { ticket: self.owner.ticket, job: self.owner.job, cancel: None });
        }
        self.open.lock().unwrap_or_else(|e| e.into_inner()).push(Open { id, kind: kind.into(), intent });
    }

    /// Persist the intent in its own transaction, before the effect.
    pub(crate) fn begin(&self, kind: &str, intent: Value) -> Result<i64> {
        self.point(kind, "begin")?;
        let Some(db) = self.db else { return Ok(0) };
        let id = db.tx(|q| begin(q, self.owner, kind, intent.clone()))?;
        self.track(id, kind, intent);
        self.point(kind, "after_begin")?;
        Ok(id)
    }

    /// Persist the intent inside the caller's transaction. The caller fires
    /// `after_begin` after its commit.
    pub(crate) fn begin_in(&self, q: &Q, kind: &str, intent: Value) -> Result<i64> {
        self.point(kind, "begin")?;
        if self.db.is_none() {
            return Ok(0);
        }
        let id = begin(q, self.owner, kind, intent.clone())?;
        self.track(id, kind, intent);
        Ok(id)
    }

    /// The effect returned; its result is not yet recorded.
    pub(crate) fn effect(&self, kind: &str) -> Result<()> {
        self.point(kind, "after_effect")
    }

    /// A cancellation flag for an owned process tree, signalled by
    /// [`Engine::cancel`](crate::Engine::cancel) of the owning ticket.
    pub(crate) fn cancellable(&self, id: i64) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        if let Some(hub) = self.hub
            && let Some(live) = hub.live().get_mut(&id)
        {
            live.cancel = Some(flag.clone());
        }
        flag
    }

    /// Fix the outcome of one operation now (still recorded by the caller).
    pub(crate) fn decide(&self, id: i64, state: &'static str, detail: Value) {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(index) = open.iter().position(|o| o.id == id) {
            let o = open.remove(index);
            self.decided.lock().unwrap_or_else(|e| e.into_inner())
                .push(Verdict { id, kind: o.kind, intent: o.intent, state, detail });
        }
    }

    /// Classify every operation of this scope: decided ones as decided, the
    /// rest `confirmed` on success, otherwise from inspected postconditions.
    /// Runs Git; never call while holding the journal.
    pub(crate) fn settle(&self, ok: bool) -> Vec<Verdict> {
        if self.crashed() {
            return vec![];
        }
        let mut verdicts: Vec<Verdict> = std::mem::take(&mut *self.decided.lock().unwrap_or_else(|e| e.into_inner()));
        for o in std::mem::take(&mut *self.open.lock().unwrap_or_else(|e| e.into_inner())) {
            let (state, detail) = if ok { (CONFIRMED, Value::Null) } else { inspect(&o.kind, &o.intent) };
            verdicts.push(Verdict { id: o.id, kind: o.kind, intent: o.intent, state, detail });
        }
        verdicts.sort_by_key(|v| v.id);
        if let Some(hub) = self.hub {
            let mut live = hub.live();
            for v in &verdicts {
                live.remove(&v.id);
            }
        }
        verdicts
    }

    /// Ids of every operation begun in this scope so far.
    pub(crate) fn ids(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self.open.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|o| o.id)
            .chain(self.decided.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|v| v.id))
            .filter(|id| *id > 0)
            .collect();
        ids.sort_unstable();
        ids
    }

    pub(crate) fn record(&self, q: &Q, verdicts: &[Verdict], receipt: bool) -> Result<()> {
        if self.db.is_none() || self.crashed() {
            return Ok(());
        }
        let result = match self.hub {
            Some(hub) => hub.record(q, verdicts, receipt),
            None => Ok(()),
        };
        if let Err(error) = &result
            && error.is::<SimulatedCrash>()
        {
            self.crashed.store(true, Ordering::SeqCst);
        }
        result
    }
}

impl crate::project_git::Observer for Scope<'_> {
    fn before(&self, effect: crate::project_git::Effect, path: &Path) -> Result<()> {
        use crate::project_git::Effect;
        let path = path.to_string_lossy();
        match effect {
            Effect::Init => self.begin(kind::PROJECT_INIT, json!({"directory": path})),
            Effect::InitialCommit => self.begin(kind::PROJECT_COMMIT, json!({"root": path})),
        }
        .map(drop)
    }
    fn after(&self, effect: crate::project_git::Effect) -> Result<()> {
        self.effect(match effect {
            crate::project_git::Effect::Init => kind::PROJECT_INIT,
            crate::project_git::Effect::InitialCommit => kind::PROJECT_COMMIT,
        })
    }
}

/// Re-check verdicts whose success was assumed, e.g. after their receipt
/// transaction failed.
pub(crate) fn reinspect(verdicts: &[Verdict]) -> Vec<Verdict> {
    verdicts.iter().map(|v| {
        let (state, detail) = if v.state == CONFIRMED && v.detail.is_null() { inspect(&v.kind, &v.intent) } else { (v.state, v.detail.clone()) };
        Verdict { state, detail, ..v.clone() }
    }).collect()
}

/// Classify a failed process run by its error markers.
pub(crate) fn process_failure(error: &anyhow::Error, unmarked: &'static str) -> &'static str {
    if error.is::<ProcessUnconfirmed>() {
        UNKNOWN_EFFECT
    } else if error.is::<NotStarted>() {
        FAILED_BEFORE_EFFECT
    } else {
        unmarked
    }
}

// ---------------------------------------------------------------------------
// Postcondition inspection. Read-only Git; never aborts, resets or re-runs.

fn probe(cwd: &Path, args: &[&str]) -> Option<String> {
    if !cwd.is_dir() {
        return None;
    }
    workspaces::git(cwd, args).ok()
}

fn ancestor(cwd: &Path, commit: &str, of: &str) -> bool {
    !commit.is_empty() && probe(cwd, &["merge-base", "--is-ancestor", commit, of]).is_some()
}

fn unknown(reason: impl Into<String>) -> (&'static str, Value) {
    (UNKNOWN_EFFECT, json!({"reason": reason.into()}))
}

/// Classify an operation from repository state. Inspection failures are an
/// unknown effect, never an assumed absence.
pub(crate) fn inspect(kind: &str, intent: &Value) -> (&'static str, Value) {
    use kind::*;
    let text = |key: &str| intent[key].as_str().unwrap_or_default().to_string();
    match kind {
        PROJECT_INIT => {
            let directory = text("directory");
            let dir = Path::new(&directory);
            if probe(dir, &["rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true") {
                (CONFIRMED, json!({"repository": true}))
            } else if dir.is_dir() && !dir.join(".git").exists() {
                (FAILED_BEFORE_EFFECT, json!({"reason": "目录中没有 Git 仓库"}))
            } else {
                unknown("目录存在 Git 元数据但无法读取仓库，请人工核对")
            }
        }
        PROJECT_COMMIT => {
            let root = text("root");
            let root = Path::new(&root);
            if let Some(head) = probe(root, &["rev-parse", "--verify", "HEAD^{commit}"]) {
                (CONFIRMED, json!({"head": head}))
            } else if probe(root, &["rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true") {
                (FAILED_BEFORE_EFFECT, json!({"reason": "仓库尚无初始提交"}))
            } else {
                unknown("无法读取仓库，请人工核对初始提交")
            }
        }
        WORKTREE_ADD => {
            let (source, path, branch) = (text("source"), text("path"), text("branch"));
            if Path::new(&path).exists() {
                match workspaces::existing_worktree(Path::new(&source), Path::new(&path), &branch, &path) {
                    Ok(head) => (CONFIRMED, json!({"head": head})),
                    Err(error) => unknown(format!("工作区目录存在但与记录不符：{error}")),
                }
            } else if probe(Path::new(&source), &["show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_some() {
                unknown("任务分支已创建但工作区目录缺失，请人工核对")
            } else if Path::new(&source).is_dir() {
                (FAILED_BEFORE_EFFECT, json!({"reason": "工作区目录和任务分支均不存在"}))
            } else {
                unknown("原仓库不可读取")
            }
        }
        DEPENDENCY_MERGE => {
            let path = text("path");
            let path = Path::new(&path);
            let Some(head) = probe(path, &["rev-parse", "--verify", "HEAD"]) else {
                return unknown("工作区不可读取");
            };
            if probe(path, &["rev-parse", "-q", "--verify", "MERGE_HEAD"]).is_some() {
                return unknown("前置分支合并未完成（存在 MERGE_HEAD），已保留现场，未自动中止");
            }
            let merge_head = text("merge_head");
            if ancestor(path, &merge_head, "HEAD") {
                (CONFIRMED, json!({"head": head}))
            } else if head == text("pre_head") && workspaces::clean(path).is_ok() {
                (FAILED_BEFORE_EFFECT, json!({"reason": "工作区仍停在合并前提交"}))
            } else {
                unknown("工作区提交既不是合并前也不包含前置提交")
            }
        }
        CHECKPOINT => {
            let path = text("path");
            let path = Path::new(&path);
            let Some(head) = probe(path, &["rev-parse", "--verify", "HEAD"]) else {
                return unknown("工作区不可读取");
            };
            let pre = text("pre_head");
            if head == pre {
                (FAILED_BEFORE_EFFECT, json!({"reason": "检查点提交未产生"}))
            } else if probe(path, &["rev-parse", "--verify", "HEAD^"]).as_deref() == Some(pre.as_str())
                && workspaces::clean(path).is_ok()
            {
                (CONFIRMED, json!({"head": head}))
            } else {
                unknown("工作区提交已改变但不是预期的检查点")
            }
        }
        PUBLISH | LEGACY_INTEGRATE => {
            let source = text("source");
            let source = Path::new(&source);
            let Some(head) = probe(source, &["rev-parse", "--verify", "HEAD"]) else {
                return unknown("原项目不可读取");
            };
            let recorded = text("source_ref");
            if !recorded.is_empty() && probe(source, &["symbolic-ref", "HEAD"]).as_deref() != Some(recorded.as_str()) {
                return unknown("原项目分支已切换，请人工核对合入结果");
            }
            if head == text("target") {
                (CONFIRMED, json!({"head": head}))
            } else if head == text("baseline") && workspaces::clean(source).is_ok() {
                (FAILED_BEFORE_EFFECT, json!({"reason": "原项目仍停在合入前提交"}))
            } else {
                unknown("原项目 HEAD 既不是合入前提交也不是候选提交")
            }
        }
        CANDIDATE => {
            let source = text("source");
            let source = Path::new(&source);
            let candidates = orphan_candidates(source, &text("home"), &text("prefix"));
            let baseline = intent["baseline"].as_str();
            match probe(source, &["rev-parse", "--verify", "HEAD"]) {
                Some(head) if baseline.is_none_or(|b| b == head) => (FAILED_BEFORE_EFFECT,
                    json!({"reason": "候选准备未完成；原项目未改变，需重新验证", "candidates": candidates})),
                _ => (UNKNOWN_EFFECT, json!({"reason": "原项目已在候选准备期间改变，请人工核对", "candidates": candidates})),
            }
        }
        _ => unknown("进程结果未记录"),
    }
}

/// Candidate worktrees retained under `home` for this job (never removed
/// automatically; they are listed for the operator).
fn orphan_candidates(source: &Path, home: &str, prefix: &str) -> Vec<String> {
    let Some(list) = probe(source, &["worktree", "list", "--porcelain"]) else { return vec![] };
    if home.is_empty() || prefix.is_empty() {
        return vec![];
    }
    let home = workspaces::scope(home);
    let prefix = if cfg!(windows) { prefix.to_lowercase() } else { prefix.to_string() };
    list.lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .filter(|path| {
            workspaces::scope(path).strip_prefix(&home).and_then(|rest| rest.strip_prefix('/'))
                .is_some_and(|name| name.starts_with(&prefix))
        })
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------------------
// Publication receipts shared by confirmation and reconciliation.

pub(crate) fn merged_workspace(w: &Workspace, validation: &Value, detail: &str) -> Workspace {
    let mut merged = w.clone();
    merged.state = "merged".into();
    merged.validation = validation.clone();
    merged.validation["integrated_head"] = validation["candidate_head"].clone();
    merged.detail = detail.into();
    merged
}

/// Record a queue publication exactly as a successful confirmation does.
pub(crate) fn record_publication(q: &Q, job: i64, ticket: i64, merged: &Workspace, reconciled: bool) -> Result<()> {
    workspaces::save(q, merged)?;
    q.run("UPDATE integration_queue SET state='merged',detail=?,validation=? WHERE job=?",
        &[&merged.detail, &merged.validation.to_string(), &job])?;
    q.run("UPDATE integration_queue SET state='queued',validation='{}',detail='原项目已合入其他候选，重新验证当前候选' WHERE job!=? AND state='awaiting_confirmation'
      AND job IN (SELECT job FROM job_workspaces WHERE source=?)", &[&job, &merged.source])?;
    let root = q.root(ticket)?;
    let mut payload = json!({"job": job, "ok": true});
    if reconciled {
        payload["reconciled"] = json!(true);
    }
    q.log(who_of(&root), "integration.confirmed", &payload)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Start-up reconciliation.

fn with_reconciled(mut detail: Value) -> Value {
    if !detail.is_object() {
        detail = json!({"detail": detail});
    }
    detail["reconciled"] = json!(true);
    detail
}

/// Classify every operation left open by a previous process, then sweep
/// legacy rows that predate receipts. Call before jobs are requeued or
/// admitted. Git inspection runs outside journal transactions.
pub(crate) fn reconcile(db: &Db, hub: &Hub) -> Result<()> {
    let open = db.with(|q| q.all(
        "SELECT * FROM operations WHERE state IN ('planned','started','cancel_requested') ORDER BY id", &[]))?;
    for row in open {
        let op = Operation::from_row(&row)?;
        if hub.is_live(op.id) {
            continue;
        }
        let (state, detail) = if op.state == PLANNED {
            (FAILED_BEFORE_EFFECT, json!({"reason": "仅保存了计划，未开始执行"}))
        } else {
            match op.kind.as_str() {
                kind::BEHAVIOR => db.with(|q| behavior_verdict(q, &op))?,
                kind::LEGACY_VALIDATE => unknown("验证进程随上次程序退出而结束，结果未记录；不会自动重跑"),
                other => inspect(other, &op.intent),
            }
        };
        let detail = with_reconciled(detail);
        db.tx(|q| {
            if !get(q, op.id)?.is_some_and(|current| current.open()) {
                return Ok(());
            }
            apply(q, &op, state)?;
            finish(q, op.id, state, &detail)?;
            Ok(())
        })?;
    }
    legacy_workspaces(db, hub)?;
    mcp_receipts(db)?;
    Ok(())
}

fn job_of(op: &Operation) -> Option<i64> {
    op.job.or_else(|| op.intent["job"].as_i64())
}

/// Bring the rows an operation guards in line with its classified outcome.
fn apply(q: &Q, op: &Operation, state: &str) -> Result<()> {
    let Some(job) = job_of(op) else { return Ok(()) };
    match op.kind.as_str() {
        kind::PUBLISH => {
            let Ok(item) = crate::integration_queue::get(q, job) else { return Ok(()) };
            if item.state == "merged" {
                return Ok(());
            }
            match state {
                CONFIRMED => {
                    let w = workspaces::get(q, job)?.ok_or_else(|| anyhow!("工作区不存在"))?;
                    let merged = merged_workspace(&w, &item.validation, "操作者已确认；重启后核对原项目确认已本地合入");
                    record_publication(q, job, item.ticket, &merged, true)?;
                }
                FAILED_BEFORE_EFFECT if item.state == "publishing" => {
                    if crate::integration_queue::current(q, &item).is_ok() {
                        q.run("UPDATE integration_queue SET state='awaiting_confirmation',detail='上次合入在修改原项目前中断；可重新确认' WHERE job=?", &[&job])?;
                    } else {
                        q.run("UPDATE integration_queue SET state='stale',detail='上次合入在修改原项目前中断，验证已失效，需重新验证' WHERE job=?", &[&job])?;
                    }
                }
                UNKNOWN_EFFECT => {
                    q.run("UPDATE integration_queue SET state='blocked',detail='合入结果未知：原项目状态与合入前和候选提交都不一致，请人工核对后重新验证' WHERE job=?", &[&job])?;
                }
                _ => {}
            }
        }
        kind::LEGACY_INTEGRATE => {
            let Some(w) = workspaces::get(q, job)? else { return Ok(()) };
            if w.state != "integrating" {
                return Ok(());
            }
            if state == CONFIRMED {
                let mut merged = w.clone();
                merged.state = "merged".into();
                merged.detail = "上次合入已在原项目完成；重启后核对确认".into();
                merged.validation["integrated_head"] = op.intent["target"].clone();
                workspaces::save(q, &merged)?;
                note(q, w.ticket, "workspace.result", &json!({"job":job,"state":merged.state,"detail":merged.detail,"validation":merged.validation,"reconciled":true}))?;
            } else {
                let reason = if state == FAILED_BEFORE_EFFECT { "上次合入在修改原项目前中断，需重新验证" }
                    else { "上次合入结果未知，请人工核对原项目后重新验证" };
                q.run("UPDATE job_workspaces SET state='failed',detail=? WHERE job=?", &[&reason, &job])?;
                note(q, w.ticket, "workspace.error", &json!({"job":job,"reason":reason,"reconciled":true}))?;
            }
        }
        kind::LEGACY_VALIDATE => {
            if let Some(w) = workspaces::get(q, job)?
                && w.state == "validating"
            {
                let reason = "上次验证中断，结果未记录，需重新验证";
                q.run("UPDATE job_workspaces SET state='failed',detail=? WHERE job=?", &[&reason, &job])?;
                note(q, w.ticket, "workspace.error", &json!({"job":job,"reason":reason,"reconciled":true}))?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Log under the ticket's root job; an orphaned row must not stop start-up.
fn note(q: &Q, ticket: i64, kind: &str, payload: &Value) -> Result<()> {
    if let Some(who) = who(q, Some(ticket), None)? {
        q.log(who, kind, payload)?;
    }
    Ok(())
}

/// A behavior attempt without its result stays without a pass.
fn behavior_verdict(q: &Q, op: &Operation) -> Result<(&'static str, Value)> {
    let attempt = op.intent["attempt"].as_i64().unwrap_or_default();
    Ok(match q.one("SELECT status FROM delivery_behavior_results WHERE attempt_id=?", &[&attempt])? {
        Some(row) => (CONFIRMED, json!({"attempt": attempt, "status": str_of(&row, "status")})),
        None => (UNKNOWN_EFFECT, json!({"attempt": attempt,
            "reason": "行为检查进程随上次程序退出而结束，未取得结果；该次尝试保持未通过，不会自动重跑"})),
    })
}

/// Legacy validate/integrate rows without a live operation.
fn legacy_workspaces(db: &Db, hub: &Hub) -> Result<()> {
    let rows = db.with(|q| q.all("SELECT job FROM job_workspaces WHERE state IN ('validating','integrating') ORDER BY job", &[]))?;
    for row in rows {
        let job = int_of(&row, "job");
        if hub.live_job(job) {
            continue;
        }
        let Some(w) = db.with(|q| workspaces::get(q, job))? else { continue };
        // Inspect outside the journal: does the source already contain the head?
        let contained = w.state == "integrating" && ancestor(Path::new(&w.source), &w.head, "HEAD");
        db.tx(|q| {
            if !workspaces::get(q, job)?.is_some_and(|current| current.state == w.state) {
                return Ok(());
            }
            let reason = match (w.state.as_str(), contained) {
                ("integrating", true) => {
                    let id = begin(q, Owner::job(w.ticket, job), kind::LEGACY_INTEGRATE,
                        json!({"job":job,"source":w.source,"target":w.head,"legacy":true}))?;
                    finish(q, id, UNKNOWN_EFFECT, &json!({"reason":"原项目已包含该任务提交，但没有合入回执；请人工核对","reconciled":true}))?;
                    "原项目已包含该任务提交但缺少合入回执，请人工核对后重新验证"
                }
                ("integrating", false) => "上次合入中断，原项目未包含该任务提交，需重新验证",
                _ => "上次验证中断，结果未记录，需重新验证",
            };
            q.run("UPDATE job_workspaces SET state='failed',detail=? WHERE job=?", &[&reason, &job])?;
            note(q, w.ticket, "workspace.error", &json!({"job":job,"reason":reason,"reconciled":true}))?;
            Ok(())
        })?;
    }
    Ok(())
}

/// `SA_TOOL` receipts still `pending` were issued (or about to be) by a dead
/// process. They become `uncertain` and are never re-issued.
fn mcp_receipts(db: &Db) -> Result<()> {
    db.tx(|q| {
        if !q.exists("SELECT 1 FROM sqlite_master WHERE type='table' AND name='studio_tool_receipts'", &[])? {
            return Ok(());
        }
        let rows = q.all("SELECT r.job,r.logical,r.key,r.request,j.ticket,j.turn,j.id FROM studio_tool_receipts r
            JOIN jobs j ON j.id=r.job WHERE r.state='pending' ORDER BY r.rowid", &[])?;
        for row in rows {
            let (job, logical, key) = (int_of(&row, "job"), int_of(&row, "logical"), str_of(&row, "key"));
            let request: Value = serde_json::from_str(str_of(&row, "request")).unwrap_or(Value::Null);
            let reason = "上次程序退出时 MCP 调用尚未返回，工具可能已执行；不会自动重试";
            let result = json!({"error": reason});
            q.run("UPDATE studio_tool_receipts SET state='uncertain',result=? WHERE job=? AND logical=? AND key=? AND state='pending'",
                &[&stringify(&result), &job, &logical, &key])?;
            q.log(who_of(&row), "mcp.result", &json!({"id": key, "server": request["server"], "tool": request["tool"],
                "state": "uncertain", "result": result, "reconciled": true}))?;
            let id = begin(q, Owner::job(int_of(&row, "ticket"), job), kind::MCP, json!({"job": job, "logical": logical, "key": key,
                "server": request["server"], "tool": request["tool"], "request_sha256": digest(str_of(&row, "request"))}))?;
            finish(q, id, UNKNOWN_EFFECT, &json!({"reason": reason, "reconciled": true}))?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        db.tx(|q| {
            q.run("INSERT INTO tickets(id,goal,cwd,writes,lead,created) VALUES(1,'g','.',0,'codex',1)", &[])?;
            q.run("INSERT INTO jobs(id,ticket,agent,body,writing,depth,state) VALUES(1,1,'codex','b',0,0,'queued')", &[])?;
            Ok(())
        }).unwrap();
        (dir, db)
    }

    fn kinds(db: &Db) -> Vec<String> {
        db.with(|q| q.all("SELECT kind FROM events ORDER BY seq", &[])).unwrap()
            .iter().map(|r| str_of(r, "kind").to_string()).collect()
    }

    #[test]
    fn transitions_are_logged_terminal_states_are_final_and_only_unknown_resolves() {
        let (_dir, db) = db();
        let id = db.tx(|q| begin(q, Owner::ticket(1), "test.kind", json!({"path": "x"}))).unwrap();
        assert!(db.tx(|q| finish(q, id, CANCELLED, &json!({"why": "stop"}))).unwrap());
        assert!(!db.tx(|q| finish(q, id, CONFIRMED, &Value::Null)).unwrap(), "terminal state is final");
        assert!(db.tx(|q| resolve(q, id, "checked")).is_err());
        let unknown = db.tx(|q| begin(q, Owner::ticket(1), "test.kind", json!({}))).unwrap();
        db.tx(|q| finish(q, unknown, UNKNOWN_EFFECT, &json!({"reason": "r"}))).unwrap();
        assert!(db.tx(|q| resolve(q, unknown, "  ")).is_err());
        db.tx(|q| resolve(q, unknown, "operator checked the repository")).unwrap();
        let op = db.with(|q| get(q, unknown)).unwrap().unwrap();
        assert_eq!(op.state, RESOLVED);
        assert_eq!(op.detail["previous"]["reason"], "r");
        assert_eq!(op.intent_sha256, digest("{}"));
        assert_eq!(kinds(&db), ["operation.started", "operation.cancel_requested", "operation.cancelled",
            "operation.started", "operation.unknown_effect", "operation.resolved"]);
        assert_eq!(db.with(|q| list(q, 1)).unwrap().len(), 2);
    }

    #[test]
    fn ticketless_operations_have_no_events_until_attached() {
        let (_dir, db) = db();
        let id = db.tx(|q| begin(q, Owner::default(), kind::PROJECT_INIT, json!({"directory": "d"}))).unwrap();
        db.tx(|q| finish(q, id, CONFIRMED, &Value::Null)).unwrap();
        assert!(kinds(&db).is_empty());
        db.tx(|q| attach(q, &[id], Who { ticket: 1, id: 1, turn: 1 })).unwrap();
        assert_eq!(db.with(|q| list(q, 1)).unwrap()[0].job, Some(1));
    }

    #[test]
    fn failpoints_fire_by_name_and_crash_suppresses_later_writes() {
        let (_dir, db) = db();
        let points = Failpoints::new();
        let hub = Hub::new(Some(points.clone()));
        points.set("k.after_begin", Fault::Crash);
        let scope = Scope::new(&db, &hub, Owner::ticket(1));
        let error = scope.begin("k", json!({})).unwrap_err();
        assert!(error.is::<SimulatedCrash>());
        assert!(scope.crashed());
        assert!(scope.settle(false).is_empty());
        assert_eq!(points.hits(), ["k.after_begin"]);
        let open = db.with(|q| q.int("SELECT count(*) FROM operations WHERE state='started'", &[])).unwrap();
        assert_eq!(open, 1, "a crash leaves the receipt started");
        points.set("k.begin", Fault::Error);
        let scope = Scope::new(&db, &hub, Owner::ticket(1));
        assert!(scope.begin("k", json!({})).is_err());
        assert!(!scope.crashed());
        assert_eq!(db.with(|q| q.int("SELECT count(*) FROM operations", &[])).unwrap(), 1, "no receipt before the begin point");
        // Detached scopes never record and never fire.
        let detached = Scope::detached();
        assert_eq!(detached.begin("k", json!({})).unwrap(), 0);
        assert!(detached.settle(true).is_empty());
    }
}
