//! Scheduler loop (port of `runtime/engine.cjs` and the write paths of
//! `runtime/engine-store.cjs`).
//!
//! Admission, the per-turn envelope, streamed control lines and the commit
//! follow the JS engine (spec §2-§5). Failure handling goes through the
//! [`TurnRunner`]: the default [`ResilientRunner`] applies classified,
//! bounded retries for every job, circuit breakers and optional provider
//! rotation (spec §9.2); [`crate::runner::PassThroughRunner`] keeps the
//! legacy regex retry for auto roots.
//!
//! Must be used inside a multi-threaded tokio runtime: admitted turns run as
//! spawned tasks. One engine per journal (`desktop.lock`, spec §2.9) is the
//! caller's responsibility.

use crate::adapter::{AGENTS, Adapter, AdapterError, AdapterEvent, ErrorClass, Route, RunRequest, TurnHost};
use crate::autonomy;
use crate::context::{self, MAX_PROMPT_BYTES};
use crate::db::{Db, Q, Who, int_of, now, str_of, who_of};
use crate::dispatch::{self, CmdError};
use crate::guides;
use crate::job::{JobCtx, envelope};
use crate::protocol::{self as p, END_SQL, digest, stringify};
use crate::resilience::{Health, HealthTransition, Recovery, TurnLedger, TurnResult, classify};
use crate::runner::{NoRotation, ResilientRunner, RotationSource, TurnRunner};
use crate::services::{Extensions, FsSnapshotter, Snapshotter, realpath};
use anyhow::{Result, anyhow, bail};
use futures::FutureExt;
use futures::future::{BoxFuture, Shared, join_all};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex, MutexGuard, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

/// Yield after each admission slice regardless of how many peers run (E:10).
const READS_PER_SLICE: usize = 8;
const TIMEOUT_REASON: &str = "任务超时，重试前须核对实际副作用。";
const BUDGET_REASON: &str = "旧版自动工单达到运行预算，请追加目标继续。";
const UNAVAILABLE: &str = "所选供应商、模型或执行端当前不可用";
/// Retries per hour allowed to a continuous root after its class budget is
/// spent (spec §9.2 A `retry_max`).
const CONTINUOUS_RETRIES_PER_HOUR: i64 = 30;

/// Legacy retry pattern for auto roots (E:515).
static TRANSIENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)HTTP (429|50[0234])|connection reset|timed out|rate limit|service unavailable|stream disconnected|暂时不可用|ECONNRESET|ETIMEDOUT",
    )
    .expect("valid regex")
});
static CONTROL_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^SA_(SEND|NOTE|ROLE|TOOL|PROGRESS) ").expect("valid regex"));

/// Pluggable engine services.
pub struct EngineOptions {
    pub runner: Arc<dyn TurnRunner>,
    pub rotation: Arc<dyn RotationSource>,
    pub extensions: Option<Arc<dyn Extensions>>,
    pub snapshotter: Arc<dyn Snapshotter>,
    /// Admission interval (80 ms in the JS engine).
    pub tick: Duration,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            runner: Arc::new(ResilientRunner::default()),
            rotation: Arc::new(NoRotation),
            extensions: None,
            snapshotter: Arc::new(FsSnapshotter),
            tick: Duration::from_millis(80),
        }
    }
}

/// `create(params)` input (S:85-143, ST:299-317).
#[derive(Clone, Debug, Default)]
pub struct CreateParams {
    pub goal: String,
    pub cwd: PathBuf,
    /// Default `deepseek`.
    pub lead: Option<String>,
    /// Default `readonly`.
    pub permission_profile: Option<String>,
    /// Default `true`.
    pub allow_lead_routing: Option<bool>,
    pub files: Vec<String>,
    /// Per-agent routes; default `route_defaults`.
    pub selections: Option<Value>,
    pub extensions: Option<Value>,
}

impl CreateParams {
    pub fn new(goal: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            goal: goal.into(),
            cwd: cwd.into(),
            ..Default::default()
        }
    }
    pub fn lead(mut self, lead: &str) -> Self {
        self.lead = Some(lead.into());
        self
    }
    pub fn profile(mut self, profile: &str) -> Self {
        self.permission_profile = Some(profile.into());
        self
    }
    pub fn files(mut self, files: &[&str]) -> Self {
        self.files = files.iter().map(|f| f.to_string()).collect();
        self
    }
}

type CatalogFuture = Shared<BoxFuture<'static, Result<Value, String>>>;

struct Slot {
    ticket: i64,
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Sched {
    active: HashMap<i64, Slot>,
    writer: Option<i64>,
    timer: Option<JoinHandle<()>>,
}

pub(crate) struct Inner {
    pub(crate) db: Db,
    data_dir: PathBuf,
    adapters: HashMap<String, Arc<dyn Adapter>>,
    opts: EngineOptions,
    sched: Mutex<Sched>,
    catalogs: Mutex<HashMap<String, CatalogFuture>>,
    started: AtomicBool,
    stopping: AtomicBool,
    fault: Mutex<Option<String>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One admitted turn (the JS `ctx`).
pub struct Turn {
    inner: Weak<Inner>,
    pub(crate) job: JobCtx,
    packet: Mutex<Map<String, Value>>,
    reports: Vec<Value>,
    menus: Mutex<Map<String, Value>>,
    pub(crate) cancel: CancellationToken,
    timed_out: AtomicBool,
    pub(crate) tool_follow: AtomicBool,
    route: Mutex<Option<Route>>,
}

impl Turn {
    /// The engine outlives every turn it runs (`close` awaits them).
    pub(crate) fn inner(&self) -> Arc<Inner> {
        self.inner.upgrade().expect("engine dropped while a turn runs")
    }
    pub(crate) fn aborted(&self) -> bool {
        self.cancel.is_cancelled()
    }
    pub(crate) fn live(&self) -> bool {
        self.inner
            .upgrade()
            .is_some_and(|i| i.db.with(|q| q.live(self.job.who())).unwrap_or(false))
    }
    fn ensure_current(&self) -> Result<()> {
        if self.aborted() || !self.live() {
            bail!("任务已中止");
        }
        Ok(())
    }
    pub(crate) fn packet(&self) -> MutexGuard<'_, Map<String, Value>> {
        lock(&self.packet)
    }
    pub(crate) fn menus(&self) -> MutexGuard<'_, Map<String, Value>> {
        lock(&self.menus)
    }
}

/// Why a turn did not commit.
#[derive(Debug)]
enum TurnError {
    /// Classified harness or route failure.
    Adapter(AdapterError),
    /// The route's breaker is open; requeue without spending an attempt.
    Wait(f64),
    Engine(anyhow::Error),
}

impl TurnError {
    fn message(&self) -> String {
        match self {
            Self::Adapter(e) => e.message.clone(),
            Self::Wait(_) => classify::message_for(ErrorClass::RouteUnavailable).into(),
            Self::Engine(e) => e.to_string(),
        }
    }
}

impl From<anyhow::Error> for TurnError {
    fn from(e: anyhow::Error) -> Self {
        Self::Engine(e)
    }
}

fn route_error(class_code: &str, message: String) -> TurnError {
    let mut e = AdapterError::new(ErrorClass::RouteUnavailable, message);
    e.code = Some(class_code.into());
    TurnError::Adapter(e)
}

fn class_name(class: ErrorClass) -> Value {
    serde_json::to_value(class).unwrap_or(Value::Null)
}

fn route_json(r: &Route) -> Value {
    p::route_value(r)
}

/// Parsed JSON column, `null` when absent or invalid.
fn parse(text: Option<&str>) -> Value {
    text.and_then(|t| serde_json::from_str(t).ok()).unwrap_or(Value::Null)
}

/// The scheduler. Cheap to clone; all clones share one engine.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

impl Engine {
    /// Open (and migrate) `<dir>/hub.sqlite3`. Recovery runs on [`start`](Self::start).
    pub fn open(dir: impl AsRef<Path>, adapters: Vec<Arc<dyn Adapter>>, opts: EngineOptions) -> Result<Self> {
        let data_dir = std::path::absolute(dir.as_ref())?;
        let db = Db::open(&data_dir)?;
        let adapters = adapters.into_iter().map(|a| (a.agent().to_string(), a)).collect();
        Ok(Self {
            inner: Arc::new(Inner {
                db,
                data_dir,
                adapters,
                opts,
                sched: Mutex::new(Sched::default()),
                catalogs: Mutex::new(HashMap::new()),
                started: AtomicBool::new(false),
                stopping: AtomicBool::new(false),
                fault: Mutex::new(None),
            }),
        })
    }

    pub fn db(&self) -> &Db {
        &self.inner.db
    }

    pub fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }

    /// Set when the scheduler halted on an unrecoverable journal error.
    pub fn fault(&self) -> Option<String> {
        lock(&self.inner.fault).clone()
    }

    pub fn active_count(&self) -> usize {
        lock(&self.inner.sched).active.len()
    }

    /// `start()` (E:32-39): recover interrupted work, then admit every tick.
    pub fn start(&self) -> Result<()> {
        let inner = &self.inner;
        if inner.db.is_closed() || inner.stopping.load(Ordering::SeqCst) {
            bail!("引擎正在关闭");
        }
        if inner.started.load(Ordering::SeqCst) {
            return Ok(());
        }
        inner.db.tx(|q| q.recover())?;
        inner.started.store(true, Ordering::SeqCst);
        let weak = Arc::downgrade(inner);
        let tick = inner.opts.tick;
        let timer = tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let Some(inner) = weak.upgrade() else { break };
                if inner.stopping.load(Ordering::SeqCst) || lock(&inner.fault).is_some() {
                    break;
                }
                // A full slice may leave more queued work: yield, then continue.
                while inner.step() >= READS_PER_SLICE {
                    tokio::task::yield_now().await;
                }
            }
        });
        lock(&inner.sched).timer = Some(timer);
        Ok(())
    }

    /// One admission pass; returns the number of admitted turns.
    pub fn step(&self) -> usize {
        self.inner.step()
    }

    /// `close()` (E:40-54): abort running turns, wait for them, close the DB.
    pub async fn close(&self) {
        let inner = &self.inner;
        inner.stopping.store(true, Ordering::SeqCst);
        let tasks: Vec<JoinHandle<()>> = {
            let mut s = lock(&inner.sched);
            if let Some(t) = s.timer.take() {
                t.abort();
            }
            s.active
                .values_mut()
                .filter_map(|slot| {
                    slot.cancel.cancel();
                    slot.task.take()
                })
                .collect()
        };
        for t in tasks {
            let _ = t.await;
        }
        inner.db.close();
    }

    /// `cancel({ticket})` (E:63-73).
    pub fn cancel(&self, ticket: i64) -> Result<()> {
        self.inner.db.tx(|q| {
            q.root(ticket)?;
            q.run(
                "UPDATE approvals SET decision='expired' WHERE ticket=? AND decision='pending'",
                &[&ticket],
            )?;
            for job in q.all(
                &format!("SELECT * FROM jobs WHERE ticket=? AND state NOT IN {END_SQL}"),
                &[&ticket],
            )? {
                q.run("UPDATE jobs SET state='cancelled' WHERE id=?", &[&int_of(&job, "id")])?;
                q.log(who_of(&job), "cancelled", &json!("用户停止了工单"))?;
            }
            Ok(())
        })?;
        for slot in lock(&self.inner.sched).active.values() {
            if slot.ticket == ticket {
                slot.cancel.cancel();
            }
        }
        Ok(())
    }

    /// `decide({id, allow})` (E:74-87).
    pub fn decide(&self, id: i64, allow: bool) -> Result<()> {
        self.inner.db.tx(|q| {
            let row = q.one(
                "SELECT j.*,a.turn AS requested FROM approvals a JOIN jobs j ON j.id=a.job AND j.ticket=a.ticket
        WHERE a.id=? AND a.decision='pending'",
                &[&id],
            )?;
            let Some(row) = row.filter(|r| str_of(r, "state") == "running" && r["turn"] == r["requested"]) else {
                bail!("审批已失效");
            };
            let decision = if allow { "allow" } else { "deny" };
            q.run("UPDATE approvals SET decision=? WHERE id=?", &[&decision, &id])?;
            q.log(who_of(&row), "approval.decision", &json!({"approval": id, "decision": decision}))?;
            Ok(())
        })
    }

    /// `create(params)` (S:85-143); returns the ticket id.
    pub async fn create(&self, params: CreateParams) -> Result<i64> {
        let inner = &self.inner;
        let lead = params.lead.clone().unwrap_or_else(|| "deepseek".into());
        let profile = params.permission_profile.clone().unwrap_or_else(|| "readonly".into());
        let routing = params.allow_lead_routing.unwrap_or(true);
        if !p::valid_str(&params.goal, usize::MAX)
            || params.cwd.as_os_str().is_empty()
            || !AGENTS.contains(&lead.as_str())
            || !p::PROFILES.contains(&profile.as_str())
        {
            bail!("工单的目标、目录、主控或权限无效");
        }
        let dir = realpath(&params.cwd).map_err(|_| anyhow!("工作目录无效"))?;
        if !dir.is_dir() {
            bail!("工作目录无效");
        }
        let dir_text = dir.to_string_lossy().into_owned();
        if params.extensions.is_some() && inner.opts.extensions.is_none() {
            bail!("当前未配置项目扩展服务");
        }
        let extensions = match &inner.opts.extensions {
            Some(x) => Some(x.prepare(params.extensions.clone(), &dir).await?),
            None => None,
        };
        let manifest = inner.snapshot(&dir_text, &params.files).await?;
        inner.db.tx(|q| {
            let defaults = q.one("SELECT routes FROM route_defaults WHERE singleton=1", &[])?;
            let raw = match &params.selections {
                Some(s) => s.clone(),
                None => parse(Some(defaults.as_ref().map_or("{}", |d| str_of(d, "routes")))),
            };
            let picked = p::routes(&raw).map_err(|e| anyhow!(e))?;
            let writes = profile != "readonly";
            let ticket = q.insert(
                "INSERT INTO tickets(goal,cwd,writes,lead,created,claude_route,agent_routes,
        follow_config,permission_profile,allow_lead_routing) VALUES(?,?,?,?,?,'user',?,1,?,?)",
                &[
                    &params.goal,
                    &dir_text,
                    &(writes as i64),
                    &lead,
                    &now(),
                    &stringify(&Value::Object(picked)),
                    &profile,
                    &(routing as i64),
                ],
            )?;
            if let (Some(x), Some(prepared)) = (&inner.opts.extensions, &extensions) {
                x.bind(q.conn(), ticket, prepared)?;
            }
            if profile == "auto" {
                q.run("INSERT INTO continuous_runs(ticket) VALUES(?)", &[&ticket])?;
            }
            let writing = (writes && p::can_write(&lead, &profile)) as i64;
            let id = q.insert(
                "INSERT INTO jobs(ticket,sender,agent,body,writing,depth,state)
        VALUES(?,'user',?,?,?,0,'queued')",
                &[&ticket, &lead, &params.goal, &writing],
            )?;
            if !manifest.is_empty() {
                q.run(
                    "INSERT INTO job_files VALUES(?,?)",
                    &[&id, &stringify(&Value::Array(manifest.clone()))],
                )?;
            }
            let who = Who { ticket, id, turn: 1 };
            q.log(who, "user", &json!(params.goal))?;
            if !manifest.is_empty() {
                q.log(who, "files", &Value::Array(manifest.clone()))?;
            }
            Ok(ticket)
        })
    }

    /// `follow({ticket, body, resume})` (S:175-203); returns the event seq.
    pub fn follow(&self, ticket: i64, body: &str, resume: bool) -> Result<i64> {
        if !p::valid_str(body, usize::MAX) {
            bail!("追加指令为空");
        }
        self.inner.db.tx(|q| {
            let root = q.root(ticket)?;
            let state = str_of(&root, "state");
            let cont = q.exists("SELECT 1 FROM continuous_runs WHERE ticket=?", &[&ticket])?;
            if !cont && matches!(state, "interrupted" | "draining") {
                bail!("请先处理上级中断或失败");
            }
            if cont && state == "cancelled" && !resume {
                bail!("停止的工单需要明确恢复");
            }
            let id = int_of(&root, "id");
            let seq = q.log(who_of(&root), "user.follow", &json!(body))?;
            if cont {
                q.run(
                    "UPDATE continuous_runs SET phase='work',probe=0,success=0,stalls=0,last_signal='',
          boundary_seq=? WHERE ticket=?",
                    &[&seq, &ticket],
                )?;
                q.run("UPDATE jobs SET ready_after=0 WHERE id=?", &[&id])?;
            }
            let auto = q
                .one("SELECT permission_profile FROM tickets WHERE id=?", &[&ticket])?
                .is_some_and(|t| str_of(&t, "permission_profile") == "auto");
            if auto {
                q.run("UPDATE tickets SET auto_cycles=0,objective=? WHERE id=?", &[&body, &ticket])?;
            }
            if p::is_terminal(state)
                || (auto && (state == "waiting" || (cont && matches!(state, "interrupted" | "draining"))))
            {
                q.run(
                    "UPDATE jobs SET state='queued',turn=turn+1,dispatch_turn=NULL WHERE id=?",
                    &[&id],
                )?;
            }
            Ok(seq)
        })
    }

    /// `retry({job})` (S:204-219): same logical turn, receipts kept.
    pub fn retry(&self, id: i64) -> Result<()> {
        self.inner.db.tx(|q| {
            let job = q.one("SELECT * FROM jobs WHERE id=?", &[&id])?;
            let Some(job) = job.filter(|j| matches!(str_of(j, "state"), "failed" | "interrupted")) else {
                bail!("只能重试失败或中断的任务");
            };
            let mut parent = job.get("parent").and_then(Value::as_i64);
            while let Some(pid) = parent {
                let row = q.one("SELECT * FROM jobs WHERE id=?", &[&pid])?;
                let Some(row) = row.filter(|r| !matches!(str_of(r, "state"), "running" | "cancelled" | "interrupted"))
                else {
                    bail!("请先处理上级任务");
                };
                if str_of(&row, "state") != "draining" {
                    q.run("UPDATE jobs SET state='waiting' WHERE id=?", &[&pid])?;
                }
                parent = row.get("parent").and_then(Value::as_i64);
            }
            q.run("UPDATE jobs SET state='queued',ready_after=0 WHERE id=?", &[&id])?;
            q.log(who_of(&job), "retry", &json!("重试同一逻辑轮次，保留已提交消息的幂等收据。"))?;
            Ok(())
        })
    }

    /// `note({ticket, job, body})` (S:273-275) from the operator.
    pub fn note(&self, ticket: i64, job: i64, body: &str) -> Result<i64> {
        self.inner
            .db
            .tx(|q| q.record_note(ticket, Some(&json!(job)), Some(&json!(body)), None, None))
    }

    /// `notes(id)` (S:276-283).
    pub fn notes(&self, job: i64) -> Result<Vec<Value>> {
        self.inner.db.with(|q| q.notes(job, false))
    }

    /// `root(ticket)`.
    pub fn root(&self, ticket: i64) -> Result<Map<String, Value>> {
        self.inner.db.with(|q| q.root(ticket))
    }

    /// Unhealthy routes known to the runner.
    pub fn health(&self) -> Vec<(String, Route, Health)> {
        self.inner.opts.runner.health()
    }
}

impl Inner {
    pub(crate) fn extensions(&self) -> Option<Arc<dyn Extensions>> {
        self.opts.extensions.clone()
    }

    fn step(self: &Arc<Self>) -> usize {
        if !self.started.load(Ordering::SeqCst) || self.stopping.load(Ordering::SeqCst) || lock(&self.fault).is_some()
        {
            return 0;
        }
        match self.admit() {
            Ok(n) => n,
            Err(e) => {
                *lock(&self.fault) = Some(e.to_string());
                if let Some(t) = lock(&self.sched).timer.take() {
                    t.abort();
                }
                0
            }
        }
    }

    /// `harness_health` envelope field: unhealthy routes per agent.
    fn health_value(&self) -> Value {
        let mut out = Map::new();
        for (agent, route, health) in self.opts.runner.health() {
            if health == Health::Healthy {
                continue;
            }
            let entry = out.entry(agent).or_insert_with(|| json!([]));
            if let Value::Array(a) = entry {
                a.push(json!({
                    "provider": route.provider,
                    "model": route.model,
                    "mode": route.mode,
                    "state": serde_json::to_value(health).unwrap_or(Value::Null),
                }));
            }
        }
        Value::Object(out)
    }

    fn log_transitions(&self, q: &Q, who: Who) -> Result<()> {
        for t in self.opts.runner.take_transitions() {
            log_transition(q, who, &t)?;
        }
        Ok(())
    }

    /// `step()` body (E:138-206). The scheduler lock is held for the whole
    /// pass so the writer slot cannot be granted twice.
    fn admit(self: &Arc<Self>) -> Result<usize> {
        let mut sched = lock(&self.sched);
        let health = self.health_value();
        let admitted = self.db.tx(|q| {
            q.wake()?;
            let queued = "SELECT j.*,t.cwd,t.claude_route,t.follow_config,t.agent_routes,t.allow_lead_routing,t.writes,
        t.permission_profile AS ticket_profile,coalesce(j.permission_override,t.permission_profile) AS profile,
        t.auto_cycles,r.route AS bound_route,c.ticket AS continuous_ticket,coalesce(c.boundary_job,0) AS floor
        FROM jobs j JOIN tickets t ON t.id=j.ticket LEFT JOIN native_routes r ON r.job=j.id
        LEFT JOIN continuous_runs c ON c.ticket=j.ticket WHERE j.state='queued' AND j.ready_after<=?
        AND j.writing=? ORDER BY j.id LIMIT ?";
            let time = now();
            let mut candidates = q.all(queued, &[&time, &0, &(READS_PER_SLICE as i64)])?;
            if sched.writer.is_none() {
                candidates.extend(q.all(queued, &[&time, &1, &1])?);
            }
            candidates.sort_by_key(|r| int_of(r, "id"));
            let plan = q.exists("SELECT 1 FROM sqlite_master WHERE type='table' AND name='studio_plan'", &[])?;
            let mut slots = vec![];
            for row in candidates {
                let mut job = JobCtx::from_row(&row);
                if sched.active.contains_key(&job.id) {
                    continue;
                }
                if let Some(until) = self.route_blocked(&job, time) {
                    q.run("UPDATE jobs SET ready_after=? WHERE id=?", &[&until, &job.id])?;
                    q.log(job.who(), "turn.deferred", &json!({"until": p::seconds_json(until), "reason": "circuit_open"}))?;
                    continue;
                }
                if job.auto_root() {
                    if !job.continuous && job.auto_cycles >= 64 {
                        let state = if q.pending_children(job.id)? { "draining" } else { "failed" };
                        q.run("UPDATE jobs SET state=?,result=? WHERE id=?", &[&state, &BUDGET_REASON, &job.id])?;
                        q.log(job.who(), state, &json!(BUDGET_REASON))?;
                        continue;
                    }
                    q.run("UPDATE tickets SET auto_cycles=auto_cycles+1 WHERE id=?", &[&job.ticket])?;
                }
                let mut packet = envelope(q, &job, health.clone())?;
                if plan {
                    let rows = q.all("SELECT id,body,done FROM studio_plan WHERE ticket=? ORDER BY id", &[&job.ticket])?;
                    packet.insert("plan".into(), Value::Array(rows.into_iter().map(Value::Object).collect()));
                }
                q.run("UPDATE jobs SET state='running' WHERE id=?", &[&job.id])?;
                job.admission = q.log(job.who(), "turn.admitted", &json!({}))?;
                q.run(
                    "UPDATE mailbox SET consumed_turn=? WHERE target=? AND consumed_turn IS NULL",
                    &[&job.turn, &job.id],
                )?;
                let reports = packet
                    .get("reports")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                slots.push(Turn {
                    inner: Arc::downgrade(self),
                    job,
                    packet: Mutex::new(packet),
                    reports,
                    menus: Mutex::new(Map::new()),
                    cancel: CancellationToken::new(),
                    timed_out: AtomicBool::new(false),
                    tool_follow: AtomicBool::new(false),
                    route: Mutex::new(None),
                });
            }
            Ok(slots)
        })?;
        let n = admitted.len();
        for turn in admitted {
            let turn = Arc::new(turn);
            let id = turn.job.id;
            if turn.job.writing {
                sched.writer = Some(id);
            }
            let inner = self.clone();
            let t = turn.clone();
            // The task's cleanup waits for the scheduler lock held here, so
            // the slot is always registered before it can be removed.
            let task = tokio::spawn(async move {
                if let Err(e) = inner.execute(&t).await {
                    *lock(&inner.fault) = Some(format!("无法保存任务状态：{e}"));
                }
                let mut s = lock(&inner.sched);
                s.active.remove(&t.job.id);
                if s.writer == Some(t.job.id) {
                    s.writer = None;
                }
            });
            sched.active.insert(
                id,
                Slot {
                    ticket: turn.job.ticket,
                    cancel: turn.cancel.clone(),
                    task: Some(task),
                },
            );
        }
        Ok(n)
    }

    /// Open breaker on a job's bound route (spec §9.2 E).
    fn route_blocked(&self, job: &JobCtx, time: f64) -> Option<f64> {
        let bound = parse(job.bound_route.as_deref());
        let route = p::route(Some(&bound)).ok()?;
        self.opts.runner.admission_blocked_until(&job.agent, &route, time)
    }

    /// `catalogRequest(name)` (E:207-215): one in-flight probe per harness.
    fn catalog(self: &Arc<Self>, name: &str) -> CatalogFuture {
        let mut map = lock(&self.catalogs);
        if let Some(f) = map.get(name) {
            return f.clone();
        }
        let adapter = self.adapters.get(name).cloned();
        let weak = Arc::downgrade(self);
        let key = name.to_string();
        let fut: BoxFuture<'static, Result<Value, String>> = Box::pin(async move {
            let r = match adapter {
                Some(a) => a.catalog().await.map(|c| c.value).map_err(|e| e.to_string()),
                None => Err("执行端不可用".to_string()),
            };
            if let Some(inner) = weak.upgrade() {
                lock(&inner.catalogs).remove(&key);
            }
            r
        });
        let shared = fut.shared();
        map.insert(name.to_string(), shared.clone());
        shared
    }

    /// `snapshot(cwd, names)` (S:144-174).
    pub(crate) async fn snapshot(&self, cwd: &str, names: &[String]) -> Result<Vec<Value>> {
        let unsafe_name = |n: &str| {
            let b = n.as_bytes();
            !p::valid_str(n, usize::MAX)
                || n.starts_with('/')
                || n.starts_with('\\')
                || (b.len() >= 2 && b[1] == b':')
                || n.split(['/', '\\']).any(|s| s == "..")
        };
        if names.len() > 16 || names.iter().any(|n| unsafe_name(n)) {
            bail!("附件需要最多 16 个项目相对路径");
        }
        if names.is_empty() {
            return Ok(vec![]);
        }
        let result = self
            .opts
            .snapshotter
            .snapshot(PathBuf::from(cwd), names.to_vec(), self.data_dir.join("objects"))
            .await?;
        let sha = |s: &str| s.len() == 64 && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c));
        let rows = result.as_array().cloned().unwrap_or_default();
        let ok = rows.len() == names.len()
            && rows.iter().zip(names).all(|(f, n)| {
                f.get("name").and_then(Value::as_str) == Some(n)
                    && p::safe_int(f.get("bytes")).is_some_and(|b| b >= 0)
                    && f.get("sha256").and_then(Value::as_str).is_some_and(sha)
                    && f.get("object_path").and_then(Value::as_str).is_some_and(|o| Path::new(o).is_absolute())
            });
        if !ok {
            bail!("快照清单无效");
        }
        Ok(rows)
    }

    /// `execute(ctx)` (E:263-408). `Err` only when the journal cannot record
    /// the outcome.
    async fn execute(self: &Arc<Self>, turn: &Arc<Turn>) -> Result<()> {
        let job = &turn.job;
        let outcome = self.run(turn).await;
        let saved = match outcome {
            Ok(()) => Ok(()),
            Err(err) => self.db.tx(|q| self.fail(q, turn, &err)),
        };
        let expire = self.db.with(|q| {
            q.run(
                "UPDATE approvals SET decision='expired' WHERE job=? AND turn=? AND decision='pending'",
                &[&job.id, &job.turn],
            )
        });
        // A failed write for one job should not halt every ticket (spec §9.2
        // H): fall back to the restart semantics before giving up.
        if let Err(e) = saved {
            let fallback = self.db.tx(|q| {
                if q.live(job.who())? {
                    q.interrupt(job.who(), &job.ticket_profile)?;
                }
                Ok(())
            });
            if fallback.is_err() {
                return Err(e);
            }
        }
        expire.map(|_| ())
    }

    /// Route selection, prompt assembly, the supervised native turn and the
    /// commit. Every error goes to `fail`.
    async fn run(self: &Arc<Self>, turn: &Arc<Turn>) -> Result<(), TurnError> {
        let job = &turn.job;
        let primary = job.primary();
        turn.ensure_current()?;
        let (route, fresh) = self.execution_route(turn).await?;
        if fresh && let Some(until) = self.opts.runner.admission_blocked_until(&job.agent, &route, now()) {
            return Err(TurnError::Wait(until));
        }
        let mut guide = guides::BASE.to_string();
        if primary {
            guide.push('\n');
            guide.push_str(guides::CONTINUOUS);
        } else if job.profile == "auto" && !job.continuous {
            guide.push('\n');
            guide.push_str(guides::LEGACY_AUTO);
        }
        let mut packet = turn.packet().clone();
        if primary && packet.get("autonomy").and_then(|a| a.get("phase")).and_then(Value::as_str) == Some("discover") {
            guide.push_str(guides::DISCOVERY);
        }
        if let Some(ext) = &self.opts.extensions {
            let ctx = ext.context(job.ticket, &job.profile);
            let allow = job.is_root() && !matches!(job.profile.as_str(), "legacy" | "readonly");
            let list = |k: &str| ctx.get(k).filter(|v| v.is_array()).cloned().unwrap_or_else(|| json!([]));
            let mcp = if allow { list("mcp") } else { json!([]) };
            let has_mcp = mcp.as_array().is_some_and(|a| !a.is_empty());
            packet.insert("project_extensions".into(), json!({"skills": list("skills"), "mcp": mcp}));
            let results = if job.is_root() { ext.results(job.id) } else { vec![] };
            packet.insert("mcp_results".into(), Value::Array(results));
            if has_mcp {
                guide.push_str(guides::MCP);
            }
        }
        if packet.get("recovery").is_some_and(|r| !r.is_null()) {
            guide.push_str(guides::RECOVERY);
        }
        let failing_report = turn.reports.iter().any(|r| {
            r.get("failure").and_then(|f| f.get("retryable")) == Some(&Value::Bool(false))
        });
        let unhealthy = packet
            .get("harness_health")
            .and_then(Value::as_object)
            .is_some_and(|h| !h.is_empty());
        if failing_report || unhealthy {
            guide.push_str(guides::REROUTE);
        }
        *turn.packet() = packet.clone();
        let mut prompt = format!("{guide}\n{}", stringify(&Value::Object(packet.clone())));
        if prompt.len() > MAX_PROMPT_BYTES {
            packet = context::compact(&packet, &guide, &self.data_dir, MAX_PROMPT_BYTES)?;
            prompt = format!("{guide}\n{}", stringify(&Value::Object(packet.clone())));
        }
        turn.ensure_current()?;
        self.db.tx(|q| {
            q.log(job.who(), "input", &Value::Object(packet.clone()))?;
            q.log(job.who(), "running", &json!({"guide": guide, "prompt_sha256": digest(&prompt)}))?;
            Ok(())
        })?;
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(anyhow!("输入超过 6 MiB 上限，请拆分任务").into());
        }
        if job.writing && (!p::can_write(&job.agent, &job.profile) || job.profile == "readonly") {
            return Err(anyhow!("该任务缺少写权限").into());
        }
        if !p::PROFILES.contains(&job.profile.as_str()) && job.profile != "legacy" {
            return Err(anyhow!("未知的权限档").into());
        }
        let adapter = self
            .adapters
            .get(&job.agent)
            .cloned()
            .ok_or_else(|| route_error(crate::resilience::CODE_ROUTE_NOT_CONFIGURED, "执行端不可用".into()))?;
        let request = RunRequest {
            ticket: job.ticket,
            job: job.id,
            turn: job.turn,
            agent: job.agent.clone(),
            prompt,
            cwd: PathBuf::from(&job.cwd),
            route: route.clone(),
            profile: if job.profile == "legacy" {
                if job.writing { "partial" } else { "readonly" }.into()
            } else {
                job.profile.clone()
            },
            writable: p::is_full(&job.profile) || job.writing,
            session_id: job.session.clone(),
        };
        let host: Arc<dyn TurnHost> = Arc::new(Host(turn.clone()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let runner = self.opts.runner.clone();
        let mut native = tokio::spawn(runner.run_turn(adapter, request, host, tx, turn.cancel.clone()));
        let wall = self.opts.runner.wall_clock();
        let deadline = tokio::time::sleep(wall.unwrap_or(Duration::MAX / 4));
        tokio::pin!(deadline);
        let mut stream = Stream::default();
        let result = loop {
            tokio::select! {
                biased;
                Some(ev) = rx.recv() => {
                    let mut batch = vec![ev];
                    while let Ok(ev) = rx.try_recv() {
                        batch.push(ev);
                    }
                    self.absorb(turn, batch, &route, &mut stream).await;
                }
                r = &mut native => break r,
                () = &mut deadline, if wall.is_some() && !turn.timed_out.load(Ordering::SeqCst) => {
                    turn.timed_out.store(true, Ordering::SeqCst);
                    turn.cancel.cancel();
                }
            }
        };
        let mut rest = vec![];
        while let Ok(ev) = rx.try_recv() {
            rest.push(ev);
        }
        if !rest.is_empty() {
            self.absorb(turn, rest, &route, &mut stream).await;
        }
        let outcome = match result {
            Ok(TurnResult::Done(outcome)) => outcome,
            Ok(TurnResult::Failed(e)) => return Err(TurnError::Adapter(e)),
            Err(e) => return Err(anyhow!("执行端任务异常：{e}").into()),
        };
        turn.ensure_current()?;
        if !p::valid_str(&outcome.text, usize::MAX) {
            return Err(anyhow!("适配器未返回有效正文").into());
        }
        for line in outcome.text.split('\n') {
            self.submit(turn, line, false, &mut stream).await;
        }
        if let Some(fatal) = stream.fatal.take() {
            return Err(fatal.into());
        }
        turn.ensure_current()?;
        self.opts.runner.on_success(&job.agent, &route);
        self.db.tx(|q| {
            self.log_transitions(q, job.who())?;
            self.commit_reply(q, turn, &outcome.text, outcome.session_id.as_deref(), &stream.invalid)
        })?;
        Ok(())
    }

    /// `executionRoute(ctx)` (E:219-262). Returns the route and whether it was
    /// freshly selected (not bound by an earlier turn).
    async fn execution_route(self: &Arc<Self>, turn: &Arc<Turn>) -> Result<(Route, bool), TurnError> {
        let job = &turn.job;
        let assigned = parse(Some(&job.agent_routes));
        let pick = |name: &str, cat: Option<&Value>| -> Option<Value> {
            if let Some(a) = assigned.get(name).filter(|v| p::truthy_value(v)) {
                return Some(a.clone());
            }
            let cat = cat?;
            if name == "claude" && job.follow_config != 1 {
                cat.get("legacy_defaults").and_then(|d| d.get(&job.claude_route)).cloned()
            } else {
                cat.get("default").cloned()
            }
        };
        let mut memo: HashMap<String, Result<Value, String>> = HashMap::new();
        let routing = turn.packet().get("allow_lead_routing") == Some(&Value::Bool(true));
        if routing {
            let probes = AGENTS.iter().map(|name| {
                let f = self.catalog(name);
                async move { (*name, f.await) }
            });
            let mut menus = Map::new();
            for (name, r) in join_all(probes).await {
                match &r {
                    Ok(cat) => match p::options(Some(cat), pick(name, Some(cat)).as_ref()) {
                        Ok(Some(menu)) => {
                            menus.insert(name.into(), menu);
                        }
                        Ok(None) => {
                            menus.insert(name.into(), json!({"available": false, "reason": "unavailable"}));
                        }
                        // An unavailable local provider cannot grant delegation rights.
                        Err(_) => {}
                    },
                    Err(_) => {
                        menus.insert(name.into(), json!({"available": false, "reason": "probe_failed"}));
                    }
                }
                memo.insert(name.to_string(), r);
            }
            *turn.menus() = menus;
        }
        turn.ensure_current()?;
        let menus = Value::Object(turn.menus().clone());
        let selected_execution = {
            let mut packet = turn.packet();
            packet.insert("delegate_models".into(), menus);
            packet.get("selected_execution").cloned().unwrap_or(Value::Null)
        };
        let bound = parse(job.bound_route.as_deref());
        let fresh = !p::truthy_value(&bound);
        let cat = match memo.remove(&job.agent) {
            Some(r) => r,
            None => self.catalog(&job.agent).await,
        }
        .map_err(|e| route_error(crate::resilience::CODE_ROUTE_PROBE_FAILED, e))?;
        let picked = if fresh {
            if p::truthy_value(&selected_execution) {
                Some(selected_execution)
            } else {
                pick(&job.agent, Some(&cat))
            }
        } else {
            Some(bound)
        };
        let selected = p::route(picked.as_ref()).map_err(|e| TurnError::Engine(anyhow!(e)))?;
        // Existing native conversations stay on their bound route. A missing
        // model requires an explicit choice; never substitute an upstream.
        if !matches!(p::options(Some(&cat), Some(&route_json(&selected))), Ok(Some(_))) {
            return Err(route_error(crate::resilience::CODE_ROUTE_NOT_CONFIGURED, UNAVAILABLE.into()));
        }
        turn.ensure_current()?;
        self.db.tx(|q| {
            q.run(
                "INSERT OR REPLACE INTO native_routes VALUES(?,?)",
                &[&job.id, &stringify(&route_json(&selected))],
            )?;
            q.log(job.who(), "route.requested", &route_json(&selected))?;
            Ok(())
        })?;
        *lock(&turn.route) = Some(selected.clone());
        Ok((selected, fresh))
    }

    /// Journal one group of adapter events in one transaction, then project
    /// streamed control lines (E:303-326).
    async fn absorb(self: &Arc<Self>, turn: &Arc<Turn>, events: Vec<AdapterEvent>, route: &Route, st: &mut Stream) {
        let job = &turn.job;
        let mut journal = vec![];
        let mut sessions = vec![];
        for ev in events {
            match ev {
                AdapterEvent::Journal { kind, data } => journal.push((kind, data)),
                AdapterEvent::Session(s) => sessions.push(s),
                _ => {}
            }
        }
        if turn.aborted() || (journal.is_empty() && sessions.is_empty()) {
            return;
        }
        let route_text = stringify(&route_json(route));
        let saved = self.db.tx(|q| {
            if !q.live(job.who())? {
                return Ok(false);
            }
            for (kind, data) in &journal {
                let seq = q.log(job.who(), kind, data)?;
                if kind == "native"
                    && job.continuous
                    && let Some(proof) = p::tools_proof(data)
                {
                    q.run(
                        "UPDATE continuous_runs SET probe=?,success=CASE WHEN ?='ok' THEN ? ELSE success END
            WHERE ticket=? AND phase='discover' AND boundary_seq<?",
                        &[&seq, &proof, &seq, &job.ticket, &job.admission],
                    )?;
                }
            }
            // Early-persisted native session (spec §9.2 D.1).
            for s in &sessions {
                q.run(
                    "INSERT OR IGNORE INTO native_sessions(job,turn,agent,route,session) VALUES(?,?,?,?,?)",
                    &[&job.id, &job.turn, &job.agent, &route_text, s],
                )?;
            }
            Ok(true)
        });
        match saved {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => {
                st.fatal.get_or_insert(e);
                turn.cancel.cancel();
                return;
            }
        }
        for (kind, data) in journal {
            if kind == "message.boundary" {
                let line = std::mem::take(&mut st.pending);
                if !line.is_empty() {
                    self.submit(turn, &line, true, st).await;
                }
            }
            if kind == "delta"
                && let Value::String(text) = data
            {
                let joined = std::mem::take(&mut st.pending) + &text;
                let mut lines: Vec<&str> = joined.split('\n').collect();
                st.pending = lines.pop().unwrap_or_default().to_string();
                for line in lines {
                    self.submit(turn, line, true, st).await;
                }
            }
        }
    }

    /// `submit(line, streaming)` (E:275-291); commands run in order.
    async fn submit(self: &Arc<Self>, turn: &Arc<Turn>, line: &str, streaming: bool, st: &mut Stream) {
        if !CONTROL_LINE.is_match(line) {
            return;
        }
        // External effects require the authoritative final reply.
        if streaming && line.starts_with("SA_TOOL ") {
            return;
        }
        if st.fatal.is_some() || turn.aborted() {
            return;
        }
        let job = &turn.job;
        let cleaned = line.strip_suffix('\r').unwrap_or(line);
        let Err(err) = dispatch::command(turn, cleaned).await else {
            return;
        };
        let tag = line.split(' ').next().unwrap_or_default().to_string();
        if streaming && matches!(err, CmdError::Syntax(_)) {
            let _ = self.db.with(|q| q.log(job.who(), "protocol.preview_invalid", &json!({"tag": tag})));
        } else if job.primary() {
            let reason = err.message();
            let _ = self.db.with(|q| q.log(job.who(), "auto.protocol", &json!({"tag": tag, "reason": reason})));
            match st.invalid.iter_mut().find(|(t, _)| *t == tag) {
                Some(entry) => entry.1 = reason,
                None => st.invalid.push((tag, reason)),
            }
        } else {
            st.fatal = Some(err.into_anyhow());
        }
    }

    /// `commitReply(ctx, result, reports, primary, invalid)` (E:409-485).
    fn commit_reply(
        &self,
        q: &Q,
        turn: &Turn,
        text: &str,
        session: Option<&str>,
        invalid: &[(String, String)],
    ) -> Result<()> {
        let job = &turn.job;
        if !q.live(job.who())? {
            return Ok(());
        }
        let reports = &turn.reports;
        let seen: HashMap<i64, &Value> = reports
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_i64).map(|id| (id, r)))
            .collect();
        let children = q.all(
            "SELECT id,state,result FROM jobs WHERE parent=? AND id>?",
            &[&job.id, &job.floor],
        )?;
        let outstanding = children.iter().any(|c| {
            let old = seen.get(&int_of(c, "id"));
            !p::is_terminal(str_of(c, "state"))
                || old.and_then(|o| o.get("state")) != c.get("state")
                || old.map(|o| o.get("result").unwrap_or(&Value::Null)) != Some(&c["result"])
        });
        let mail = q.exists(
            "SELECT 1 FROM mailbox WHERE target=? AND consumed_turn IS NULL LIMIT 1",
            &[&job.id],
        )?;
        let follow = job.is_root() && q.followed_after(job.ticket, job.admission)?;
        let mut state = if outstanding || mail || follow || turn.tool_follow.load(Ordering::SeqCst) {
            "waiting"
        } else {
            "done"
        };
        let mut session = session.map(str::to_string);
        let primary = job.primary();
        if job.auto_root() && !follow {
            let mut actions = Map::new();
            let mut correction = None;
            let parsed = if invalid.is_empty() {
                p::controls(text, primary)
            } else {
                Err(invalid.iter().map(|(_, m)| m.as_str()).collect::<Vec<_>>().join("；"))
            };
            match parsed {
                Ok(a) => actions = a,
                Err(e) => correction = Some(format!("请修正控制消息：{e}")),
            }
            if primary {
                let run = q
                    .one("SELECT * FROM continuous_runs WHERE ticket=?", &[&job.ticket])?
                    .unwrap_or_default();
                let decision = autonomy::advance(q, job, &run, &actions, reports, state == "done", correction)?;
                state = decision.state;
                if decision.reset {
                    session = None;
                }
            } else {
                if let Some(goal) = actions.get("SA_GOAL") {
                    q.run(
                        "UPDATE tickets SET objective=? WHERE id=?",
                        &[&goal["objective"].as_str(), &job.ticket],
                    )?;
                    q.log(job.who(), "auto.goal", goal)?;
                }
                match actions.get("SA_FINISH") {
                    Some(finish) if state == "done" => {
                        if finish["status"] == "done" && !p::unresolved(reports, finish).is_empty() {
                            state = "waiting";
                            q.log(job.who(), "auto.next", &json!({"body": "请核对失败的子任务并记录证据。"}))?;
                        } else {
                            q.log(job.who(), "auto.finish", finish)?;
                            if finish["status"] == "blocked" {
                                state = "failed";
                            }
                        }
                    }
                    Some(_) => {}
                    None => {
                        state = "waiting";
                        let next = actions.get("SA_NEXT").cloned().unwrap_or_else(|| {
                            json!({"body": correction.clone().unwrap_or_else(|| "请核对状态，继续或明确结束工单。".into())})
                        });
                        q.log(job.who(), "auto.next", &next)?;
                    }
                }
            }
        }
        if state == "done"
            && !job.auto_root()
            && children
                .iter()
                .any(|c| matches!(str_of(c, "state"), "partial" | "failed" | "cancelled"))
        {
            state = "partial";
        }
        let redirect = job.profile == "auto" && follow;
        if redirect {
            state = "queued";
        }
        q.run(
            "UPDATE jobs SET result=?,session=?,state=?,dispatch_turn=NULL,turn=turn+? WHERE id=?",
            &[&text, &session, &state, &(redirect as i64), &job.id],
        )?;
        q.log(
            job.who(),
            "reply",
            &json!({"text": text, "sha256": digest(text), "session": session, "state": state}),
        )?;
        Ok(())
    }

    /// `fail(ctx, err)` (E:486-540) with the Rust resilience policy.
    fn fail(&self, q: &Q, turn: &Turn, err: &TurnError) -> Result<()> {
        let job = &turn.job;
        let who = job.who();
        if !q.live(who)? {
            return Ok(());
        }
        let timed_out = turn.timed_out.load(Ordering::SeqCst);
        let reason = if timed_out { TIMEOUT_REASON.to_string() } else { err.message() };
        if self.stopping.load(Ordering::SeqCst) {
            return q.interrupt(who, &job.ticket_profile);
        }
        let auto_root = job.auto_root();
        if auto_root && job.continuous && q.followed_after(job.ticket, job.admission)? {
            q.run(
                "UPDATE jobs SET state='queued',turn=turn+1,dispatch_turn=NULL,ready_after=0,result=? WHERE id=?",
                &[&reason, &job.id],
            )?;
            q.log(who, "auto.retry", &json!({"after_seconds": 0, "reason": "按新追加的要求重新核对。"}))?;
            return Ok(());
        }
        let route = lock(&turn.route).clone();
        if let TurnError::Wait(until) = err {
            return self.requeue(q, job, *until, None, "turn.route_wait", json!({
                "until": p::seconds_json(*until),
                "class": "route_unavailable",
                "route": route.as_ref().map(route_json),
            }));
        }
        if let (TurnError::Adapter(e), false) = (err, timed_out) {
            // Catalog probes fail before a route is chosen; give them their
            // own breaker key per harness.
            let route = route.unwrap_or_else(|| Route {
                provider: "catalog".into(),
                model: "*".into(),
                mode: "cli".into(),
            });
            let mut ledger = ledger(q, job.id)?;
            let rotation = self.opts.rotation.candidates(&job.agent);
            let decision = self.opts.runner.decide(&mut ledger, &job.agent, &route, e, &rotation, now());
            self.log_transitions(q, who)?;
            if let Some(decision) = decision {
                return self.recover(q, job, &route, e, decision, ledger.failures);
            }
        }
        if auto_root && (timed_out || TRANSIENT.is_match(&reason)) {
            let attempts = q.int(
                "SELECT count(*) AS n FROM events WHERE job=? AND kind='auto.retry' AND seq>
        coalesce((SELECT max(seq) FROM events WHERE job=? AND kind IN ('reply','user.follow')),0)",
                &[&job.id, &job.id],
            )?;
            if job.continuous || attempts < 2 {
                let delay = 2f64.powi((attempts + 1).min(9) as i32).min(300.0);
                q.interrupt(who, &job.ticket_profile)?;
                q.run("UPDATE jobs SET ready_after=? WHERE id=?", &[&(now() + delay), &job.id])?;
                q.log(who, "auto.retry", &json!({"after_seconds": p::seconds_json(delay), "reason": reason}))?;
                return Ok(());
            }
        }
        self.give_up(q, job, &reason)
    }

    /// `failed`, or `draining` while children still run (E:533-539).
    fn give_up(&self, q: &Q, job: &JobCtx, reason: &str) -> Result<()> {
        let state = if q.pending_children(job.id)? { "draining" } else { "failed" };
        q.run("UPDATE jobs SET state=?,result=? WHERE id=?", &[&state, &reason, &job.id])?;
        q.log(job.who(), state, &json!(reason))?;
        Ok(())
    }

    /// Requeue the next attempt of the same logical turn. `turn+1` makes the
    /// old worker non-live; `dispatch_turn` keeps idempotency receipts; the
    /// last real reply stays in `result` (spec §9.2 D.2/D.4).
    fn requeue(&self, q: &Q, job: &JobCtx, ready_after: f64, session: Option<Option<&str>>, kind: &str, payload: Value) -> Result<()> {
        q.run(
            "UPDATE jobs SET state='queued',dispatch_turn=coalesce(dispatch_turn,turn),turn=turn+1,ready_after=? WHERE id=?",
            &[&ready_after, &job.id],
        )?;
        if let Some(s) = session {
            q.run("UPDATE jobs SET session=? WHERE id=?", &[&s, &job.id])?;
        }
        q.log(job.who(), kind, &payload)?;
        Ok(())
    }

    fn recover(
        &self,
        q: &Q,
        job: &JobCtx,
        route: &Route,
        err: &AdapterError,
        decision: Recovery,
        failures: u32,
    ) -> Result<()> {
        let at = now();
        let notice = |q: &Q, delay: f64| -> Result<()> {
            q.log(job.who(), "auto.retry", &json!({"after_seconds": p::seconds_json(delay.round()), "reason": err.message}))?;
            if failures == 1 {
                notify_parent(q, job, err.class, failures, at + delay)?;
            }
            Ok(())
        };
        match decision {
            Recovery::Retry { delay, attempt, max, resume_session } => {
                let session = if err.class == ErrorClass::SessionMismatch {
                    Some(None)
                } else {
                    resume_session.as_deref().map(Some)
                };
                self.requeue(q, job, at + delay.as_secs_f64(), session, "turn.retry_scheduled", json!({
                    "class": class_name(err.class),
                    "attempt": attempt,
                    "max": max,
                    "after_ms": delay.as_millis() as u64,
                    "resume_session": resume_session.is_some(),
                    "route": route_json(route),
                    "code": err.code,
                }))?;
                notice(q, delay.as_secs_f64())
            }
            Recovery::Rotate { to, delay, attempt, reason } => {
                q.run(
                    "INSERT OR REPLACE INTO native_routes VALUES(?,?)",
                    &[&job.id, &stringify(&route_json(&to))],
                )?;
                // A different upstream cannot resume the old native session.
                self.requeue(q, job, at + delay.as_secs_f64(), Some(None), "route.rotated", json!({
                    "from": route_json(route),
                    "to": route_json(&to),
                    "reason": class_name(reason),
                    "attempt": attempt,
                    "after_ms": delay.as_millis() as u64,
                }))?;
                notice(q, delay.as_secs_f64())
            }
            Recovery::WaitForRoute { until_secs } => self.requeue(q, job, until_secs, None, "turn.route_wait", json!({
                "until": p::seconds_json(until_secs),
                "class": class_name(err.class),
                "route": route_json(route),
            })),
            Recovery::GiveUp { class, attempts } => {
                // A continuous project keeps retrying transient failures,
                // bounded per hour (spec §9.2 A).
                if job.primary() && classify::is_retryable(class) {
                    let recent = q.int(
                        "SELECT count(*) FROM events WHERE job=? AND kind IN ('turn.retry_scheduled','route.rotated') AND ts>?",
                        &[&job.id, &(at - 3600.0)],
                    )?;
                    if recent < CONTINUOUS_RETRIES_PER_HOUR {
                        let cfg = crate::resilience::ResilienceConfig::default();
                        let delay = cfg.retry_cap;
                        self.requeue(q, job, at + delay.as_secs_f64(), None, "turn.retry_scheduled", json!({
                            "class": class_name(class),
                            "attempt": attempts,
                            "max": Value::Null,
                            "after_ms": delay.as_millis() as u64,
                            "resume_session": false,
                            "route": route_json(route),
                            "code": err.code,
                        }))?;
                        return notice(q, delay.as_secs_f64());
                    }
                }
                q.log(job.who(), "turn.gave_up", &json!({
                    "class": class_name(class),
                    "attempts": attempts,
                    "code": err.code,
                }))?;
                self.give_up(q, job, &err.message)
            }
        }
    }
}

/// Rebuild the logical turn's [`TurnLedger`] from journaled recoveries since
/// the last reply, follow, manual retry or give-up.
fn ledger(q: &Q, job: i64) -> Result<TurnLedger> {
    let rows = q.all(
        "SELECT kind,payload FROM events WHERE job=? AND kind IN ('turn.retry_scheduled','route.rotated','turn.route_wait')
      AND seq>coalesce((SELECT max(seq) FROM events WHERE job=? AND kind IN ('reply','user.follow','retry','turn.gave_up')),0)
      ORDER BY seq",
        &[&job, &job],
    )?;
    let mut l = TurnLedger::default();
    let parse_route = |v: Option<&Value>| v.and_then(|r| p::route(Some(r)).ok());
    for r in rows {
        let payload = parse(Some(str_of(&r, "payload")));
        let (failed_on, rotated_to) = match str_of(&r, "kind") {
            "route.rotated" => (parse_route(payload.get("from")), parse_route(payload.get("to"))),
            _ => (parse_route(payload.get("route")), None),
        };
        if let Some(route) = failed_on
            && l.current.as_ref() != Some(&route)
        {
            l.current = Some(route);
            l.route_failures = 0;
        }
        l.failures += 1;
        l.route_failures += 1;
        if let Some(to) = rotated_to {
            l.rotations += 1;
            l.current = Some(to);
            l.route_failures = 0;
        }
    }
    Ok(l)
}

/// Early wake of the parent after a child's first failed attempt (spec §9.2
/// F). `source=NULL` keeps it outside the 128-note budget.
fn notify_parent(q: &Q, job: &JobCtx, class: ErrorClass, attempt: u32, until: f64) -> Result<()> {
    let Some(parent) = job.parent else { return Ok(()) };
    let open = q.exists(
        "SELECT 1 FROM jobs WHERE id=? AND ticket=? AND state IN ('queued','running','waiting')",
        &[&parent, &job.ticket],
    )?;
    if !open {
        return Ok(());
    }
    let body = stringify(&json!({
        "child": job.id,
        "class": class_name(class),
        "attempt": attempt,
        "retrying_until": p::seconds_json(until.round()),
    }));
    let seq = q.log(
        job.who(),
        "note",
        &json!({"job": parent, "sender": "platform", "body": body, "sha256": digest(&body)}),
    )?;
    q.run(
        "INSERT INTO mailbox(seq,target,source,source_turn,msgkey) VALUES(?,?,NULL,NULL,NULL)",
        &[&seq, &parent],
    )?;
    Ok(())
}

fn log_transition(q: &Q, who: Who, t: &HealthTransition) -> Result<()> {
    q.log(
        who,
        "harness.health",
        &json!({
            "agent": t.agent,
            "provider": t.route.provider,
            "model": t.route.model,
            "mode": t.route.mode,
            "from": serde_json::to_value(t.from)?,
            "state": serde_json::to_value(t.to)?,
            "reason_class": t.reason_class.map(class_name),
            "until": t.until.map(p::seconds_json),
        }),
    )?;
    Ok(())
}

/// Per-turn stream state: the unterminated delta tail, primary protocol
/// errors (by tag, first-insertion order) and the first fatal error.
#[derive(Default)]
struct Stream {
    pending: String,
    invalid: Vec<(String, String)>,
    fatal: Option<anyhow::Error>,
}

/// `approve(ctx, action)` (E:88-125).
pub(crate) async fn approve(turn: Arc<Turn>, action: Value) -> bool {
    let job = &turn.job;
    let Some(inner) = turn.inner.upgrade() else { return false };
    if turn.aborted() || !matches!(job.profile.as_str(), "ask" | "partial") {
        return false;
    }
    let id = inner.db.tx(|q| {
        if !q.live(job.who())? {
            return Ok(None);
        }
        if job.ticket_profile == "auto" {
            q.log(
                job.who(),
                "approval.unattended_decline",
                &json!({"action": action, "reason": "自动工单不会代替用户批准受限子任务。"}),
            )?;
            return Ok(None);
        }
        let id = q.insert(
            "INSERT INTO approvals(ticket,job,turn,action) VALUES(?,?,?,?)",
            &[&job.ticket, &job.id, &job.turn, &stringify(&action)],
        )?;
        q.log(job.who(), "approval.request", &json!({"approval": id, "action": action}))?;
        Ok(Some(id))
    });
    let Ok(Some(id)) = id else { return false };
    while !turn.aborted() && !inner.db.is_closed() && turn.live() {
        let row = inner
            .db
            .with(|q| q.one("SELECT decision FROM approvals WHERE id=?", &[&id]))
            .ok()
            .flatten();
        let decision = row.as_ref().map(|r| str_of(r, "decision")).unwrap_or_default();
        if decision != "pending" {
            return decision == "allow";
        }
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
            () = turn.cancel.cancelled() => break,
        }
    }
    if !inner.db.is_closed() {
        let _ = inner.db.with(|q| {
            q.run(
                "UPDATE approvals SET decision='expired' WHERE id=? AND decision='pending'",
                &[&id],
            )
        });
    }
    false
}

/// Engine services handed to the adapter for one turn.
struct Host(Arc<Turn>);

impl TurnHost for Host {
    fn approve(&self, action: Value) -> BoxFuture<'static, bool> {
        Box::pin(approve(self.0.clone(), action))
    }

    fn pending_notes(&self) -> Vec<Value> {
        let turn = &self.0;
        if turn.aborted() {
            return vec![];
        }
        let Some(inner) = turn.inner.upgrade() else { return vec![] };
        inner
            .db
            .with(|q| q.notes(turn.job.id, true))
            .unwrap_or_default()
            .into_iter()
            .map(|n| json!({"seq": n["seq"], "sender": n["sender"], "body": n["body"]}))
            .collect()
    }

    fn accept_notes(&self, seqs: &[i64]) {
        let turn = &self.0;
        if turn.aborted() {
            return;
        }
        if let Some(inner) = turn.inner.upgrade() {
            let _ = inner.db.tx(|q| q.accept(turn.job.who(), seqs));
        }
    }
}
