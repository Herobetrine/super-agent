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

use crate::activity::{ActivityPhase, JobActivity, RetryInfo, Tracker};
use crate::adapter::{AGENTS, Adapter, AdapterError, AdapterEvent, ErrorClass, Route, RunRequest, TurnHost};
use crate::autonomy;
use crate::checks;
use crate::context::{self, MAX_PROMPT_BYTES};
use crate::db::{BeginBusy, Db, Q, Who, int_of, now, sqlite_error_codes, str_of, who_of};
use crate::dispatch::{self, CmdError};
use crate::guides;
use crate::job::{JobCtx, envelope};
use crate::knowledge;
use crate::pause::{self, PauseStatus};
use crate::protocol::{self as p, END_SQL, digest, stringify};
use crate::resilience::{Health, HealthTransition, Recovery, ResilienceConfig, TurnLedger, TurnResult, classify};
use crate::runner::{NoRotation, ResilientRunner, RotationSource, TurnRunner};
use crate::services::{Extensions, FsSnapshotter, Snapshotter, realpath};
use anyhow::{Result, anyhow, bail, ensure};
use futures::FutureExt;
use futures::future::{BoxFuture, Shared, join_all};
use regex::Regex;
use serde_json::{Map, Value, json};
use serde::Serialize;
use std::panic::AssertUnwindSafe;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex, MutexGuard, Weak,
        atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
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
    LazyLock::new(|| Regex::new(r"^SA_(SEND|NOTE|ROLE|TOOL|PROGRESS|MEMO|CANCEL|REPORT|ASK) ").expect("valid regex"));

/// Admission-loop status, independent of ticket pause and model activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerState {
    NotStarted,
    Running,
    DatabaseBusy,
    Unresponsive,
    Failed,
    Stopped,
}

/// A lock-free snapshot that remains available when admission is blocked.
#[derive(Clone, Debug, Serialize)]
pub struct SchedulerHealth {
    pub state: SchedulerState,
    pub last_progress_seconds_ago: Option<f64>,
    pub last_sqlite_code: Option<i32>,
    pub last_sqlite_extended_code: Option<i32>,
}

const SCHEDULER_NOT_STARTED: u8 = 0;
const SCHEDULER_RUNNING: u8 = 1;
const SCHEDULER_FAILED: u8 = 2;
const SCHEDULER_STOPPED: u8 = 3;

/// Detect a lost timer even when its future is cancelled before its first poll.
struct TimerExit(Weak<Inner>);

impl Drop for TimerExit {
    fn drop(&mut self) {
        if let Some(inner) = self.0.upgrade()
            && !inner.stopping.load(Ordering::SeqCst)
        {
            inner.scheduler_failed("unexpected_exit", "调度循环异常退出，后续准入已停止。".into());
        }
    }
}

/// Pluggable engine services.
pub struct EngineOptions {
    pub runner: Arc<dyn TurnRunner>,
    pub rotation: Arc<dyn RotationSource>,
    pub extensions: Option<Arc<dyn Extensions>>,
    /// Collaboration bus; `None` disables it.
    pub bus: Option<Arc<dyn crate::services::Bus>>,
    pub snapshotter: Arc<dyn Snapshotter>,
    /// Current context delivery preferences, shared with the desktop host.
    pub efficiency: sa_store::efficiency::Shared,
    /// Admission interval (80 ms in the JS engine).
    pub tick: Duration,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            runner: Arc::new(ResilientRunner::default()),
            rotation: Arc::new(NoRotation),
            extensions: None,
            bus: None,
            snapshotter: Arc::new(FsSnapshotter),
            efficiency: sa_store::efficiency::shared(Default::default()),
            tick: Duration::from_millis(80),
        }
    }
}

/// `create(params)` input (S:85-143, ST:299-317).
#[derive(Clone, Debug, Default)]
pub struct CreateParams {
    /// Operator-selected historical root reply. No native session is copied.
    pub checkpoint: Option<(i64,i64)>,
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
    /// Automatic failover of read-only children to another harness when
    /// their route is unavailable or circuit-open (spec §9.2 F). Default
    /// off; persisted in the side table `ticket_failover`.
    pub failover: Option<bool>,
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
    pub fn failover(mut self, enabled: bool) -> Self {
        self.failover = Some(enabled);
        self
    }
}

type CatalogFuture = Shared<BoxFuture<'static, Result<Value, String>>>;

#[derive(Clone, PartialEq, Eq, Hash)]
enum CatalogKey {
    Full(String),
    Selected(String, Route),
}

struct Slot {
    ticket: i64,
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
    turn: Arc<Turn>,
}

#[derive(Default)]
struct Sched {
    active: HashMap<i64, Slot>,
    /// Maintenance leases also prevent admission while validating/integrating.
    maintenance: Vec<String>,
    timer: Option<JoinHandle<()>>,
    integration_tasks: Vec<JoinHandle<()>>,
}

type ProjectPreparation = Arc<tokio::sync::Mutex<Option<String>>>;

pub(crate) struct Inner {
    pub(crate) db: Db,
    pub(crate) data_dir: PathBuf,
    adapters: HashMap<String, Arc<dyn Adapter>>,
    opts: EngineOptions,
    sched: Mutex<Sched>,
    catalogs: Mutex<HashMap<CatalogKey, CatalogFuture>>,
    project_preparations: Mutex<HashMap<String, ProjectPreparation>>,
    started: AtomicBool,
    stopping: AtomicBool,
    fault: Mutex<Option<String>>,
    scheduler_state: AtomicU8,
    scheduler_progress_ms: AtomicU64,
    scheduler_database_busy: AtomicBool,
    scheduler_sqlite_code: AtomicU64,
    scheduler_origin: Instant,
    /// Runner watchdog limits for activity thresholds.
    limits: Option<ResilienceConfig>,
    /// Jobs cancelled inside a journal transaction whose running turns must
    /// be aborted once it commits (the scheduler lock is taken before the
    /// journal lock, never inside it).
    pending_aborts: Mutex<Vec<i64>>,
    /// Claude prompt-cache clock per job for keep-alive (in memory only).
    cache_clocks: Mutex<HashMap<i64, CacheClock>>,
}

struct IntegrationLease(Arc<Inner>, Vec<String>);
impl Drop for IntegrationLease {
    fn drop(&mut self) {
        lock(&self.0.sched).maintenance.retain(|key| !self.1.contains(key));
    }
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
    loop_detected: AtomicBool,
    pub(crate) tool_follow: AtomicBool,
    /// `SA_MEMO` lines seen in this turn's final reply.
    pub(crate) memos: AtomicUsize,
    route: Mutex<Option<Route>>,
    /// In-memory heartbeat for [`Engine::activity`].
    activity: Mutex<Tracker>,
    workspace_dependencies: Vec<(crate::workspaces::Workspace, i64)>,
    /// Prerequisite workspaces are leased only while this turn builds its own
    /// snapshot from them (§10.1); afterwards it never reads them again.
    preparing: AtomicBool,
}

impl Turn {
    fn prerequisite_lease(&self) -> impl Iterator<Item = &crate::workspaces::Workspace> {
        let held = self.preparing.load(Ordering::SeqCst);
        self.workspace_dependencies.iter().filter(move |_| held).map(|(w, _)| w)
    }

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
    /// Subscription holds preserve the route's retry and breaker budgets.
    Quota { until: f64, kind: String, reserve: bool },
    Engine(anyhow::Error),
}

impl TurnError {
    fn message(&self) -> String {
        match self {
            Self::Adapter(e) => e.message.clone(),
            Self::Wait(_) => classify::message_for(ErrorClass::RouteUnavailable).into(),
            Self::Quota { until, reserve, .. } => format!("{}，预计 {} 后继续",
                if *reserve { "为主控保留订阅额度" } else { "订阅额度窗口已用尽" },
                classify::utc_minute(*until as i64)),
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

/// Project key: the canonical directory like `tickets.cwd`, or the text as
/// given when it does not resolve.
fn project_key(cwd: &str) -> String {
    realpath(Path::new(cwd))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| cwd.to_string())
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
        let limits = opts.runner.stall_limits();
        Ok(Self {
            inner: Arc::new(Inner {
                db,
                data_dir,
                adapters,
                limits,
                pending_aborts: Mutex::new(vec![]),
                cache_clocks: Mutex::default(),
                opts,
                sched: Mutex::new(Sched::default()),
                catalogs: Mutex::new(HashMap::new()),
                project_preparations: Mutex::default(),
                started: AtomicBool::new(false),
                stopping: AtomicBool::new(false),
                fault: Mutex::new(None),
                scheduler_state: AtomicU8::new(SCHEDULER_NOT_STARTED),
                scheduler_progress_ms: AtomicU64::new(0),
                scheduler_database_busy: AtomicBool::new(false),
                scheduler_sqlite_code: AtomicU64::new(0),
                scheduler_origin: Instant::now(),
            }),
        })
    }

    pub fn db(&self) -> &Db {
        &self.inner.db
    }

    pub fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }

    pub(crate) fn is_stopping(&self) -> bool {
        self.inner.stopping.load(Ordering::SeqCst)
    }

    /// Set when the scheduler halted on an unrecoverable journal error.
    pub fn fault(&self) -> Option<String> {
        lock(&self.inner.fault).clone()
    }

    /// Reads no engine locks or SQLite state. A stale heartbeat reports the
    /// admission loop as unresponsive without restarting any admitted work.
    pub fn scheduler_health(&self) -> SchedulerHealth {
        let inner = &self.inner;
        let progress = inner.scheduler_progress_ms.load(Ordering::SeqCst);
        let age = (progress > 0).then(|| {
            inner.scheduler_origin.elapsed().as_secs_f64() - (progress - 1) as f64 / 1000.0
        }).map(|seconds| seconds.max(0.0));
        let limit = Duration::from_secs(30).max(inner.opts.tick.saturating_mul(4));
        let state = match inner.scheduler_state.load(Ordering::SeqCst) {
            SCHEDULER_RUNNING if age.is_some_and(|age| age > limit.as_secs_f64()) => SchedulerState::Unresponsive,
            SCHEDULER_RUNNING if inner.scheduler_database_busy.load(Ordering::SeqCst) => SchedulerState::DatabaseBusy,
            SCHEDULER_RUNNING => SchedulerState::Running,
            SCHEDULER_FAILED => SchedulerState::Failed,
            SCHEDULER_STOPPED => SchedulerState::Stopped,
            _ => SchedulerState::NotStarted,
        };
        let sqlite_code = inner.scheduler_sqlite_code.load(Ordering::SeqCst);
        SchedulerHealth {
            state,
            last_progress_seconds_ago: age,
            last_sqlite_code: (sqlite_code > 0).then_some((sqlite_code & 0xff) as i32),
            last_sqlite_extended_code: (sqlite_code > 0).then_some(sqlite_code as i32),
        }
    }

    pub fn active_count(&self) -> usize {
        lock(&self.inner.sched).active.len()
    }

    /// UI callers can skip the count while admission owns the scheduler lock.
    pub fn try_active_count(&self) -> Option<usize> {
        match self.inner.sched.try_lock() {
            Ok(sched) => Some(sched.active.len()),
            Err(std::sync::TryLockError::Poisoned(error)) => Some(error.into_inner().active.len()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }

    /// `start()` (E:32-39): recover interrupted work, then admit every tick.
    pub fn start(&self) -> Result<()> {
        let inner = &self.inner;
        if inner.db.is_closed() || inner.stopping.load(Ordering::SeqCst) {
            bail!("引擎正在关闭");
        }
        if inner.started.load(Ordering::SeqCst) {
            ensure!(inner.scheduler_state.load(Ordering::SeqCst) != SCHEDULER_FAILED, "调度循环已异常退出，请核对现场后重新打开工作台。");
            return Ok(());
        }
        inner.db.tx(crate::integration_queue::recover)?;
        ensure!(!inner.opts.tick.is_zero(), "调度间隔必须大于零");
        inner.db.tx(|q| q.recover())?;
        inner.started.store(true, Ordering::SeqCst);
        let weak = Arc::downgrade(inner);
        let tick = inner.opts.tick;
        inner.scheduler_state.store(SCHEDULER_RUNNING, Ordering::SeqCst);
        inner.mark_scheduler_progress();
        let exit = TimerExit(weak.clone());
        let timer = tokio::spawn(async move {
            let _exit = exit;
            let failure_target = weak.clone();
            let result = AssertUnwindSafe(async move {
                let mut interval = tokio::time::interval(tick);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    interval.tick().await;
                    let Some(inner) = weak.upgrade() else { break };
                    if inner.stopping.load(Ordering::SeqCst) || lock(&inner.fault).is_some() {
                        break;
                    }
                    // SQLite and scheduler mutexes are synchronous. Keep a
                    // single pass in flight off the async executor so its timers,
                    // control service and native streams remain responsive.
                    loop {
                        let pass = inner.clone();
                        let admitted = tokio::task::spawn_blocking(move || {
                            let admitted = pass.step();
                            pass.keepalive_pass();
                            admitted
                        }).await.unwrap_or_else(|error| {
                            if error.is_panic() {
                                std::panic::resume_unwind(error.into_panic());
                            }
                            panic!("调度准入任务异常取消");
                        });
                        if admitted < READS_PER_SLICE {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                    if inner.scheduler_database_busy.load(Ordering::SeqCst) {
                        // The ordinary tick can be short. Bound contention retry
                        // frequency without keeping an async worker blocked.
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                }
            }).catch_unwind().await;
            if let Err(panic) = result
                && let Some(inner) = failure_target.upgrade()
            {
                let detail = panic.downcast_ref::<&str>().copied()
                    .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("未知 panic");
                inner.scheduler_failed("panic", format!("调度循环异常退出：{detail}"));
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
        self.quiesce().await;
        self.inner.db.close();
    }

    /// Stop admissions and model execution while retaining the journal for
    /// tracked maintenance tasks to persist their final receipts.
    pub async fn quiesce(&self) {
        let inner = &self.inner;
        inner.stopping.store(true, Ordering::SeqCst);
        inner.scheduler_state.store(SCHEDULER_STOPPED, Ordering::SeqCst);
        let tasks: Vec<JoinHandle<()>> = {
            let mut s = lock(&inner.sched);
            if let Some(t) = s.timer.take() {
                t.abort();
            }
            let mut tasks: Vec<_> = s.active
                .values_mut()
                .filter_map(|slot| {
                    slot.cancel.cancel();
                    slot.task.take()
                })
                .collect();
            tasks.append(&mut s.integration_tasks);
            tasks
        };
        for t in tasks {
            let _ = t.await;
        }
    }

    /// `cancel({ticket})` (E:63-73).
    pub fn cancel(&self, ticket: i64) -> Result<()> {
        self.inner.db.tx(|q| {
            cancel_ticket(q, ticket)?;
            q.run("UPDATE integration_policy SET enabled=0 WHERE ticket=?",&[&ticket])?;
            q.run("UPDATE integration_queue SET state='cancelled',detail='操作者已停止工单和合入队列' WHERE ticket=? AND state!='merged'",&[&ticket])?;
            Ok(())
        })?;
        for slot in lock(&self.inner.sched).active.values() {
            if slot.ticket == ticket {
                slot.cancel.cancel();
            }
        }
        Ok(())
    }

    /// Finish the current root turn, drain its children and make at most one
    /// final consolidation turn before blocking further ticket admissions.
    pub fn request_pause_after_handoff(&self, ticket: i64) -> Result<PauseStatus> {
        let _sched = lock(&self.inner.sched);
        self.inner.db.tx(|q| pause::request(q, ticket))
    }

    /// Explicitly remove the durable admission gate. Follow-up messages and
    /// job retries retain the gate until this operation is called.
    pub fn resume_paused(&self, ticket: i64) -> Result<PauseStatus> {
        let _sched = lock(&self.inner.sched);
        self.inner.db.tx(|q| pause::resume(q, ticket))
    }

    pub fn pause_status(&self, ticket: i64) -> Result<PauseStatus> {
        self.inner.db.with(|q| {
            q.root(ticket)?;
            pause::status(q, ticket)
        })
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

    /// Change whether the root may override collaborator model/effort and
    /// narrow permissions on subsequent dispatches. Accepted assignments
    /// retain their route and native session.
    pub fn set_lead_routing(&self, ticket: i64, enabled: bool) -> Result<()> {
        self.inner.db.tx(|q| q.set_lead_routing(ticket, enabled))
    }

    pub fn isolated_writers(&self, ticket: i64) -> Result<bool> {
        self.inner.db.with(|q| q.exists("SELECT 1 FROM workspace_policy WHERE ticket=? AND isolated=1", &[&ticket]))
    }
    pub fn checkpoints(&self,ticket:i64)->Result<Vec<Value>> {
        self.inner.db.with(|q|crate::checkpoints::list(q,ticket))
    }
    pub fn bind_scm(&self,job:i64,url:&str)->Result<()> {
        self.inner.db.tx(|q|crate::scm::bind(q,job,url))
    }
    pub fn scm_bindings(&self,ticket:i64)->Result<Vec<Value>> {
        self.inner.db.with(|q|crate::scm::bindings(q,ticket))
    }
    pub async fn refresh_scm(&self,job:i64)->Result<bool> {
        let (url,cwd)=self.inner.db.with(|q| {
            let r=q.one("SELECT b.url,t.cwd FROM scm_bindings b JOIN jobs j ON j.id=b.job JOIN tickets t ON t.id=j.ticket WHERE b.job=?",&[&job])?.ok_or_else(||anyhow!("任务尚未绑定 PR"))?;
            Ok((str_of(&r,"url").to_string(),PathBuf::from(str_of(&r,"cwd"))))
        })?;
        let observation=crate::scm::fetch(&url,&cwd).await?;
        self.inner.db.tx(|q|crate::scm::apply(q,job,&observation))
    }
    pub async fn fork_checkpoint(&self,ticket:i64,seq:i64,goal:String)->Result<i64> {
        let source=self.inner.db.with(|q|q.one("SELECT * FROM tickets WHERE id=?",&[&ticket]))?.ok_or_else(||anyhow!("工单不存在"))?;
        let mut params=CreateParams::new(goal,str_of(&source,"cwd")).lead(str_of(&source,"lead")).profile(str_of(&source,"permission_profile"));
        params.selections=Some(parse(Some(str_of(&source,"agent_routes"))));
        params.allow_lead_routing=Some(int_of(&source,"allow_lead_routing")!=0);
        params.checkpoint=Some((ticket,seq));
        // Do not accidentally apply changed global extension defaults.
        if self.inner.opts.extensions.is_some(){params.extensions=Some(json!({"skills":[],"servers":[]}));}
        self.create(params).await
    }

    pub fn set_isolated_writers(&self, ticket: i64, enabled: bool) -> Result<()> {
        self.inner.db.tx(|q| {
            let root = q.root(ticket)?;
            q.run("INSERT INTO workspace_policy(ticket,isolated) VALUES(?,?) ON CONFLICT(ticket) DO UPDATE SET isolated=excluded.isolated", &[&ticket,&i64::from(enabled)])?;
            q.log(who_of(&root), "workspace.policy", &json!({"isolated_writers":enabled}))?;
            Ok(())
        })
    }

    pub fn workspaces(&self, ticket: i64) -> Result<Vec<crate::workspaces::Workspace>> {
        self.inner.db.with(|q| {
            q.all("SELECT job FROM job_workspaces WHERE ticket=? ORDER BY job", &[&ticket])?.iter()
                .map(|r| crate::workspaces::get(q, int_of(r,"job"))?.ok_or_else(|| anyhow!("工作区不存在"))).collect()
        })
    }

    /// Operator-only operation. Hold scheduler leases through all blocking Git
    /// work, including if the caller stops awaiting the result.
    pub async fn validate_workspace(&self, job: i64, command: Vec<String>) -> Result<()> {
        self.workspace_operation(job, Some(command)).await
    }
    pub async fn integrate_workspace(&self, job: i64) -> Result<()> {
        self.workspace_operation(job, None).await
    }

    pub fn set_integration_queue(&self, ticket: i64, enabled: bool) -> Result<()> {
        self.inner.db.tx(|q| {
            let root=q.root(ticket)?;
            if enabled {
                ensure!(q.int("SELECT writes FROM tickets WHERE id=?",&[&ticket])?!=0,"只读工单不能启用自动合入队列");
                ensure!(q.exists("SELECT 1 FROM workspace_policy WHERE ticket=? AND isolated=1",&[&ticket])?,"请先启用隔离写工作区");
                ensure!(!crate::integration_queue::commands(q,ticket)?.is_empty(),"请先配置项目完成检查");
            }
            q.run("INSERT INTO integration_policy(ticket,enabled) VALUES(?,?) ON CONFLICT(ticket) DO UPDATE SET enabled=excluded.enabled",&[&ticket,&i64::from(enabled)])?;
            q.log(who_of(&root),"integration.policy",&json!({"enabled":enabled}))?;
            Ok(())
        })
    }

    pub fn integration_queue(
        &self,
        ticket: i64,
    ) -> Result<Vec<crate::integration_queue::IntegrationItem>> {
        self.inner.db.with(|q| {
            q.root(ticket)?;
            crate::integration_queue::list(q, ticket)
        })
    }

    pub fn revalidate_integration(&self, job: i64) -> Result<()> {
        self.inner.db.tx(|q| {
            let item=crate::integration_queue::get(q,job)?;
            ensure!(crate::integration_queue::enabled(q,item.ticket)?,"该工单未启用自动合入队列");
            ensure!(!matches!(item.state.as_str(),"validating"|"publishing"|"merged"),"当前状态不能重新验证");
            let row=q.one("SELECT state,turn FROM jobs WHERE id=?",&[&job])?.ok_or_else(||anyhow!("任务不存在"))?;
            ensure!(str_of(&row,"state")=="done" || (str_of(&row,"state")=="partial" && int_of(&row,"turn")==item.turn && item.state=="blocked"),"请等待任务完成");
            let commands=crate::integration_queue::commands(q,item.ticket)?;
            ensure!(!commands.is_empty(),"请先配置项目完成检查");
            q.run("UPDATE jobs SET state='done' WHERE id=? AND state='partial'",&[&job])?;
            q.run("UPDATE integration_queue SET state='queued',turn=?,version=?,commands=?,validation='{}',detail='操作者请求重新验证' WHERE job=?",
                &[&int_of(&row,"turn"),&crate::integration_queue::VERSION,&json!(commands).to_string(),&job])?;
            Ok(())
        })
    }

    /// This is the sole queue publication entry point and must be invoked by
    /// an explicit operator action. It never runs from the automatic scheduler.
    pub async fn confirm_integration(&self, job: i64, expected_candidate: &str) -> Result<()> {
        let inner = self.inner.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        {
            let mut sched = lock(&inner.sched);
            ensure!(!inner.stopping.load(Ordering::SeqCst), "引擎正在关闭");
            let (item, w) = inner.db.with(|q| {
                Ok((
                    crate::integration_queue::get(q, job)?,
                    crate::workspaces::get(q, job)?.ok_or_else(|| anyhow!("工作区不存在"))?,
                ))
            })?;
            ensure!(
                matches!(item.state.as_str(), "awaiting_confirmation" | "publishing"),
                "候选尚未通过验证或需要重新验证"
            );
            ensure!(
                item.validation["confirmation_token"].as_str() == Some(expected_candidate),
                "候选已变化，请重新查看后确认"
            );
            let keys = vec![
                crate::workspaces::scope(&w.path),
                crate::workspaces::scope(&w.source),
            ];
            ensure!(
                !inner.workspace_busy(&sched, &keys),
                "相关工作区仍有任务运行"
            );
            inner.db.tx(|q| {
                crate::integration_queue::current(q,&item)?;
                q.run("UPDATE integration_queue SET state='publishing',detail='操作者已确认，正在核对并合入' WHERE job=?",&[&job])?;
                Ok(())
            })?;
            sched.maintenance.extend(keys.clone());
            let owner = inner.clone();
            let task = tokio::task::spawn_blocking(move || {
                let _lease = IntegrationLease(owner.clone(), keys);
                // Freeze operator checks and task state through publication.
                // Cancellation here reads the atomic shutdown flag, avoiding
                // recursively taking the journal lock from a Git subprocess.
                let result=owner.db.tx(|q| {
                    crate::integration_queue::current(q,&item)?;
                    let merged=crate::integration_queue::publish(&w,&item.validation,&||owner.stopping.load(Ordering::SeqCst))?;
                    crate::workspaces::save(q,&merged)?;
                    q.run("UPDATE integration_queue SET state='merged',detail=?,validation=? WHERE job=?",&[&merged.detail,&merged.validation.to_string(),&job])?;
                    q.run("UPDATE integration_queue SET state='queued',validation='{}',detail='原项目已合入其他候选，重新验证当前候选' WHERE job!=? AND state='awaiting_confirmation'
                      AND job IN (SELECT job FROM job_workspaces WHERE source=?)",&[&job,&merged.source])?;
                    let root=q.root(item.ticket)?;
                    q.log(who_of(&root),"integration.confirmed",&json!({"job":job,"ok":true}))?;
                    Ok(())
                });
                if let Err(error) = &result {
                    let _ = owner.db.tx(|q| {
                        q.run(
                            "UPDATE integration_queue SET state='stale',detail=? WHERE job=?",
                            &[&error.to_string(), &job],
                        )?;
                        Ok(())
                    });
                }
                let _ = send.send(result);
            });
            sched.integration_tasks.push(task);
        }
        receive.await.map_err(|_| anyhow!("合入操作异常"))?
    }
    async fn workspace_operation(&self, job: i64, command: Option<Vec<String>>) -> Result<()> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let (workspace, keys) = {
                let mut sched = lock(&inner.sched);
                let w = inner.db.with(|q| crate::workspaces::get(q, job))?.ok_or_else(|| anyhow!("工作区不存在"))?;
                let keys = vec![crate::workspaces::scope(&w.path), crate::workspaces::scope(&w.source)];
                let busy = inner.workspace_busy(&sched,&keys);
                if busy { bail!("相关工作区仍有任务运行，请停止或等待后再验证/合入"); }
                sched.maintenance.extend(keys.clone());
                (w,keys)
            };
            struct Lease(Arc<Inner>, Vec<String>);
            impl Drop for Lease { fn drop(&mut self) { lock(&self.0.sched).maintenance.retain(|k| !self.1.contains(k)); } }
            let _lease = Lease(inner.clone(), keys);
            // Invalidate the previous receipt before a new attempt, including
            // spawn failures/timeouts and process crashes during validation.
            inner.db.tx(|q| {
                let mut pending=workspace.clone();
                pending.state=if command.is_some(){"validating"}else{"integrating"}.into();
                pending.validation["passed"]=json!(false);
                pending.detail="操作进行中；中断后需重新验证".into();
                crate::workspaces::save(q,&pending)
            })?;
            let result = match &command {
                Some(argv) => crate::workspaces::validate(&workspace, argv),
                None => crate::workspaces::integrate(&workspace, &inner.data_dir),
            };
            inner.db.tx(|q| {
                let root = q.root(workspace.ticket)?;
                match &result {
                    Ok(w) => {
                        crate::workspaces::save(q,w)?;
                        q.log(who_of(&root), "workspace.result", &json!({"job":job,"state":w.state,"detail":w.detail,"validation":w.validation}))?;
                    }
                    Err(e) => {
                        q.run("UPDATE job_workspaces SET state='failed',detail=? WHERE job=?",&[&e.to_string(),&job])?;
                        q.log(who_of(&root), "workspace.error", &json!({"job":job,"reason":e.to_string()}))?;
                    }
                }
                Ok(())
            })?;
            result.map(|_| ())
        }).await.map_err(|_| anyhow!("工作区操作异常"))?
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
        // Reject invalid requests before changing a previously non-Git directory.
        // The transaction repeats these checks against the final journal state.
        inner.db.with(|q| {
            let defaults = q.one("SELECT routes FROM route_defaults WHERE singleton=1", &[])?;
            let raw = params.selections.clone().unwrap_or_else(||
                parse(Some(defaults.as_ref().map_or("{}", |d| str_of(d, "routes")))));
            p::routes(&raw).map_err(|e| anyhow!(e))?;
            if let Some((ticket, seq)) = params.checkpoint { crate::checkpoints::capture(q, ticket, seq)?; }
            Ok(())
        })?;
        // Platform preparation is independent of the model's write permission.
        // Run Git outside the journal transaction and async executor.
        let repository = inner.prepare_project(dir.clone(), true, CancellationToken::new()).await?;
        inner.db.tx(|q| {
            let checkpoint=params.checkpoint.map(|(ticket,seq)|crate::checkpoints::capture(q,ticket,seq)).transpose()?;
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
            if let (Some((source,seq)),Some(context))=(params.checkpoint,&checkpoint) {
                let context=stringify(context);
                q.run("INSERT INTO ticket_branches VALUES(?,?,?,?,?)",&[&ticket,&source,&seq,&context,&digest(&context)])?;
            }
            if profile == "auto" {
                q.run("INSERT INTO continuous_runs(ticket) VALUES(?)", &[&ticket])?;
            }
            if params.failover == Some(true) {
                q.run("INSERT INTO ticket_failover(ticket,mode) VALUES(?,'readers')", &[&ticket])?;
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
            if repository.initialized || repository.initial_commit.is_some() {
                q.log(who, "project.git", &json!(repository))?;
            }
            if !manifest.is_empty() {
                q.log(who, "files", &Value::Array(manifest.clone()))?;
            }
            Ok(ticket)
        })
    }

    /// `follow({ticket, body, resume})` (S:175-203); returns the event seq.
    pub fn follow(&self, ticket: i64, body: &str, resume: bool) -> Result<i64> {
        self.follow_snapshot(ticket, body, resume, &[])
    }

    /// Add immutable attachments to a root follow-up. New images replace the
    /// current image set; text attachments replace matching names. An empty
    /// list preserves existing attachments, like `follow`.
    pub async fn follow_with_files(&self, ticket: i64, body: &str, resume: bool, files: &[String]) -> Result<i64> {
        if !p::valid_str(body, usize::MAX) {
            bail!("追加指令为空");
        }
        if files.is_empty() {
            return self.follow(ticket, body, resume);
        }
        let cwd = self.inner.db.with(|q| {
            let root = q.root(ticket)?;
            if files.iter().any(|name| crate::images::is_image_name(name))
                && !crate::images::supports_native_images(str_of(&root, "agent"))
            {
                bail!(crate::images::UNSUPPORTED);
            }
            q.one("SELECT cwd FROM tickets WHERE id=?", &[&ticket])?
                .map(|row| str_of(&row, "cwd").to_string())
                .ok_or_else(|| anyhow::anyhow!("工单不存在"))
        })?;
        let manifest = self.inner.snapshot(&cwd, files).await?;
        let validation = Value::Array(manifest.clone());
        let directory = self.inner.data_dir.join("objects");
        let _ = tokio::task::spawn_blocking(move || crate::images::from_manifest(&validation, &directory))
            .await??;
        self.follow_snapshot(ticket, body, resume, &manifest)
    }

    fn follow_snapshot(&self, ticket: i64, body: &str, resume: bool, additions: &[Value]) -> Result<i64> {
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
            if !additions.is_empty() {
                let new_images = additions.iter().any(|file| file["name"].as_str().is_some_and(crate::images::is_image_name));
                if new_images && !crate::images::supports_native_images(str_of(&root, "agent")) {
                    bail!(crate::images::UNSUPPORTED);
                }
                let mut manifest = q.one("SELECT manifest FROM job_files WHERE job=?", &[&id])?
                    .and_then(|row| parse(Some(str_of(&row, "manifest"))).as_array().cloned())
                    .unwrap_or_default();
                manifest.retain(|old| {
                    !(new_images && old["name"].as_str().is_some_and(crate::images::is_image_name))
                        && !additions.iter().any(|new| new["name"] == old["name"])
                });
                manifest.extend_from_slice(additions);
                if manifest.len() > 16
                    || manifest.iter().try_fold(0u64, |sum, file| sum.checked_add(file["bytes"].as_u64()?))
                        .is_none_or(|bytes| bytes > 64 * 1024 * 1024)
                {
                    bail!("当前附件最多 16 个文件，总量不得超过 64 MiB");
                }
                q.run("INSERT OR REPLACE INTO job_files(job,manifest) VALUES(?,?)", &[&id, &stringify(&json!(manifest))])?;
                q.log(who_of(&root), "files", &json!(manifest))?;
            }
            q.withdraw_question(who_of(&root))?;
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
            ensure!(!q.exists("SELECT 1 FROM job_workspaces WHERE job=? AND state='merged'", &[&id])?,"该工作区已合入，请创建新任务继续修改");
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
                q.run("UPDATE questions SET parked=0 WHERE job=? AND state='pending'", &[&pid])?;
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

    /// Answering and delivery share a transaction so a retry cannot deliver
    /// the same operator answer twice or lose it after closing the question.
    pub fn answer(&self, question: i64, body: &str) -> Result<()> {
        ensure!(p::valid_str(body, 4000), "回答不能为空且不得超过 4000 个 UTF-16 单位");
        let stopped = self.inner.db.tx(|q| {
            let row = q.one("SELECT a.msgkey AS question_key,a.origin AS question_origin,j.* FROM questions a JOIN jobs j ON j.id=a.job
                WHERE a.id=? AND a.state='pending'", &[&question])?
                .filter(|row| !p::is_terminal(str_of(row, "state")))
                .ok_or_else(|| anyhow!("问题已关闭或提问任务已结束"))?;
            q.run("UPDATE questions SET state='answered',parked=0,answer=?,closed=? WHERE id=?", &[&body, &now(), &question])?;
            let key = str_of(&row, "question_key");
            q.log(who_of(&row), "answer", &json!({"id":question,"key":key,"answer":body}))?;
            let ticket = int_of(&row, "ticket");
            let platform = str_of(&row, "question_origin") == "platform";
            if platform && body.trim() == "停止" {
                cancel_ticket(q, ticket)?;
                return Ok(Some(ticket));
            }
            let note = if platform {
                q.run("UPDATE continuous_runs SET stalls=0,last_signal='' WHERE ticket=?", &[&ticket])?;
                q.run("UPDATE jobs SET ready_after=0 WHERE id=?", &[&int_of(&row, "id")])?;
                q.run("DELETE FROM check_bounces WHERE job=?", &[&int_of(&row, "id")])?;
                format!("Operator decision after the platform paused this task: {body}")
            } else {
                format!("Answer to your question \"{key}\": {body}")
            };
            q.record_answer_note(ticket, int_of(&row, "id"), &note)?;
            Ok(None)
        })?;
        if let Some(ticket) = stopped {
            for slot in lock(&self.inner.sched).active.values() {
                if slot.ticket == ticket { slot.cancel.cancel(); }
            }
        }
        Ok(())
    }

    pub fn questions(&self, ticket: i64) -> Result<Vec<Value>> {
        self.inner.db.with(|q| {
            q.root(ticket)?;
            q.all("SELECT *,msgkey AS key FROM questions
                WHERE ticket=? AND state='pending' ORDER BY id", &[&ticket])?.into_iter().map(|mut row| {
                let options: Value = serde_json::from_str(str_of(&row, "options"))?;
                row.insert("options".into(), options);
                Ok(Value::Object(row))
            }).collect()
        })
    }

    /// `notes(id)` (S:276-283).
    pub fn notes(&self, job: i64) -> Result<Vec<Value>> {
        self.inner.db.with(|q| q.notes(job, false))
    }

    /// `root(ticket)`.
    pub fn root(&self, ticket: i64) -> Result<Map<String, Value>> {
        self.inner.db.with(|q| q.root(ticket))
    }

    /// Operator-owned checks. Protocol control lines cannot change this setting.
    pub fn project_checks(&self, cwd: &str) -> Result<Vec<Vec<String>>> {
        let project = project_key(cwd);
        self.inner.db.with(|q| checks::configured(q, &project))
    }

    pub fn set_project_checks(&self, cwd: &str, commands: Vec<Vec<String>>) -> Result<()> {
        ensure!(commands.len() <= 4, "项目完成检查最多 4 条命令");
        for argv in &commands { crate::workspaces::validate_command(argv)?; }
        let project = project_key(cwd);
        self.inner.db.tx(|q| {
            if commands.is_empty() {
                q.run("DELETE FROM project_checks WHERE project=?", &[&project])?;
            } else {
                q.run("INSERT OR REPLACE INTO project_checks(project,checks,updated) VALUES(?,?,?)",
                    &[&project, &stringify(&json!(commands)), &now()])?;
            }
            Ok(())
        })
    }

    /// Project knowledge entries of `cwd` (canonicalized when it exists),
    /// newest first, including archived ones.
    pub fn knowledge(&self, cwd: &str) -> Result<Vec<sa_store::knowledge::KnowledgeEntry>> {
        let cwd = project_key(cwd);
        self.inner.db.with(|q| sa_store::knowledge::list(q.conn(), &cwd))
    }

    /// Add an operator entry (`agent='operator'`) to the project `cwd`.
    /// With `ticket`, the ticket must belong to the project and
    /// `knowledge.add` is logged on its root job. Returns the entry id.
    pub fn add_knowledge(&self, cwd: &str, kind: &str, body: &str, ticket: Option<i64>) -> Result<i64> {
        let cwd = project_key(cwd);
        self.inner.db.tx(|q| knowledge::operator_add(q, &cwd, kind, body, ticket))
    }

    /// Archive (`true`) or restore (`false`) a knowledge entry. Archived
    /// entries stay listed but leave every envelope.
    pub fn archive_knowledge(&self, id: i64, archived: bool) -> Result<()> {
        self.inner.db.tx(|q| knowledge::set_archived(q, id, archived))
    }

    /// Unhealthy routes known to the runner.
    pub fn health(&self) -> Vec<(String, Route, Health)> {
        self.inner.opts.runner.health()
    }

    /// Whether automatic failover of read-only children is enabled for
    /// `ticket` (spec §9.2 F).
    pub fn failover(&self, ticket: i64) -> Result<bool> {
        self.inner.db.with(|q| {
            q.root(ticket)?;
            q.exists("SELECT 1 FROM ticket_failover WHERE ticket=?", &[&ticket])
        })
    }

    /// Enable or disable automatic failover of read-only children for an
    /// existing ticket. Logs `failover.changed {enabled}` on the root when
    /// the setting changes. Takes effect at the next failure of a child.
    pub fn set_failover(&self, ticket: i64, enabled: bool) -> Result<()> {
        self.inner.db.tx(|q| {
            let root = q.root(ticket)?;
            let changed = if enabled {
                q.run(
                    "INSERT OR IGNORE INTO ticket_failover(ticket,mode) VALUES(?,'readers')",
                    &[&ticket],
                )?
            } else {
                q.run("DELETE FROM ticket_failover WHERE ticket=?", &[&ticket])?
            };
            if changed > 0 {
                q.log(who_of(&root), "failover.changed", &json!({"enabled": enabled}))?;
            }
            Ok(())
        })
    }

    /// Live activity of `ticket` for the UI (spec §9.2 G): every running job
    /// (from memory: phase, idle time, open tools, watchdog threshold, route
    /// health) and every queued job waiting for a scheduled retry (from the
    /// journal: attempt, max, class, next_at), ordered by job id. Cheap
    /// enough to call every second: one scheduler-lock snapshot, then one
    /// indexed journal read per retrying job; locks are never nested.
    /// A closed or failing journal yields only the running part.
    pub fn activity(&self, ticket: i64) -> Vec<JobActivity> {
        let inner = &self.inner;
        let turns: Vec<Arc<Turn>> = lock(&inner.sched)
            .active
            .values()
            .filter(|s| s.ticket == ticket)
            .map(|s| s.turn.clone())
            .collect();
        let health = inner.opts.runner.health();
        let health_of = |agent: &str, route: Option<&Route>| {
            route
                .and_then(|r| health.iter().find(|(a, hr, _)| a == agent && hr == r))
                .map_or(Health::Healthy, |(_, _, h)| *h)
        };
        let at = now();
        let mut out = Vec::with_capacity(turns.len());
        for t in &turns {
            let snap = lock(&t.activity).snapshot(at, inner.limits.as_ref());
            let route = lock(&t.route)
                .clone()
                .or_else(|| p::route(Some(&parse(t.job.bound_route.as_deref()))).ok());
            out.push(JobActivity {
                job: t.job.id,
                agent: t.job.agent.clone(),
                phase: snap.phase,
                idle_ms: snap.idle_ms,
                open_tools: snap.open_tools,
                last_frame_at: snap.last_frame_at,
                started_at: Some(snap.started_at),
                threshold_ms: snap.threshold_ms,
                health: health_of(&t.job.agent, route.as_ref()),
                retry: None,
            });
        }
        let retrying = inner.db.with(|q| scheduled_retries(q, ticket, at)).unwrap_or_default();
        for (job, agent, route, retry) in retrying {
            if out.iter().any(|a| a.job == job) {
                continue;
            }
            out.push(JobActivity {
                job,
                health: health_of(&agent, route.as_ref()),
                agent,
                phase: ActivityPhase::RetryWait,
                idle_ms: 0,
                open_tools: vec![],
                last_frame_at: None,
                started_at: None,
                threshold_ms: None,
                retry: Some(retry),
            });
        }
        out.sort_by_key(|a| a.job);
        out
    }
}

/// `(job, agent, bound route, retry)` of a queued job awaiting a retry.
type ScheduledRetry = (i64, String, Option<Route>, RetryInfo);

/// Queued jobs of `ticket` with `ready_after` in the future and a journaled
/// retry since their last reply.
fn scheduled_retries(q: &Q, ticket: i64, at: f64) -> Result<Vec<ScheduledRetry>> {
    let jobs = q.all(
        "SELECT j.id,j.agent,j.ready_after,r.route AS bound_route FROM jobs j LEFT JOIN native_routes r ON r.job=j.id
      WHERE j.ticket=? AND j.state='queued' AND j.ready_after>? ORDER BY j.id",
        &[&ticket, &at],
    )?;
    let mut out = vec![];
    for j in jobs {
        let id = int_of(&j, "id");
        let rows = q.all(
            "SELECT kind,payload FROM events WHERE ticket=? AND job=? AND kind IN
        ('turn.retry_scheduled','route.rotated','turn.route_wait','turn.deferred','turn.quota_wait','auto.retry')
        AND seq>coalesce((SELECT max(seq) FROM events WHERE ticket=? AND job=? AND kind IN
        ('reply','user.follow','retry','turn.gave_up')),0) ORDER BY seq",
            &[&ticket, &id, &ticket, &id],
        )?;
        let (mut attempts, mut legacy) = (0u32, 0u32);
        let mut last: Option<(String, Value)> = None;
        for r in rows {
            let kind = str_of(&r, "kind");
            match kind {
                "auto.retry" => legacy += 1,
                _ => {
                    if matches!(kind, "turn.retry_scheduled" | "route.rotated") {
                        attempts += 1;
                    }
                    last = Some((kind.to_string(), parse(Some(str_of(&r, "payload")))));
                }
            }
        }
        let (class, max, attempt) = match &last {
            Some((kind, payload)) => {
                let field = |k: &str| payload.get(k).and_then(Value::as_str).map(str::to_string);
                match kind.as_str() {
                    "turn.retry_scheduled" => (
                        field("class"),
                        payload.get("max").and_then(Value::as_u64).map(|m| m as u32),
                        attempts,
                    ),
                    "route.rotated" => (field("reason"), None, attempts),
                    "turn.deferred" => (field("reason"), None, attempts),
                    "turn.quota_wait" => (field("reason"), None, attempts),
                    _ => (field("class"), None, attempts),
                }
            }
            None if legacy > 0 => (Some("transient".into()), None, legacy),
            None => continue,
        };
        let route = p::route(Some(&parse(j.get("bound_route").and_then(Value::as_str)))).ok();
        out.push((
            id,
            str_of(&j, "agent").to_string(),
            route,
            RetryInfo {
                attempt,
                max,
                class: class.unwrap_or_else(|| "other".into()),
                next_at: j.get("ready_after").and_then(Value::as_f64).unwrap_or(at),
            },
        ));
    }
    Ok(out)
}

impl Inner {
    /// Coalesce preparation for peers sharing a project. Roots and new tickets
    /// recheck Git; peers reuse readiness, never a clean-tree/validation receipt.
    async fn prepare_project(self: &Arc<Self>, directory: PathBuf, force: bool, cancel: CancellationToken)
        -> Result<crate::project_git::Bootstrap>
    {
        let key = project_key(&directory.to_string_lossy());
        let entry = lock(&self.project_preparations).entry(key)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None))).clone();
        let mut ready = tokio::select! {
            biased;
            () = cancel.cancelled() => bail!("Git 项目准备已取消"),
            ready = entry.lock() => ready,
        };
        ensure!(!self.stopping.load(Ordering::SeqCst) && !cancel.is_cancelled(), "Git 项目准备已取消");
        if !force && directory.is_dir() && let Some(root) = ready.as_ref()
            && Path::new(root).join(".git").exists()
        {
            return Ok(crate::project_git::Bootstrap { root: root.clone(), initialized: false, initial_commit: None });
        }
        *ready = None;
        let owner = self.clone();
        let repository = tokio::task::spawn_blocking(move || {
            crate::project_git::ensure_with_cancel(&directory, &||
                cancel.is_cancelled() || owner.stopping.load(Ordering::SeqCst))
        }).await.map_err(|_| anyhow!("工作目录 Git 准备异常"))??;
        *ready = Some(repository.root.clone());
        Ok(repository)
    }

    pub(crate) fn extensions(&self) -> Option<Arc<dyn Extensions>> {
        self.opts.extensions.clone()
    }

    /// Abort the running turns of `ids` (already cancelled in the journal).
    /// Never call while holding the journal lock.
    pub(crate) fn abort_jobs(&self, ids: &[i64]) {
        if ids.is_empty() {
            return;
        }
        let sched = lock(&self.sched);
        for id in ids {
            if let Some(slot) = sched.active.get(id) {
                slot.cancel.cancel();
            }
        }
    }

    /// Abort turns cancelled by a failover inside `fail`.
    fn flush_aborts(&self) {
        let ids = std::mem::take(&mut *lock(&self.pending_aborts));
        self.abort_jobs(&ids);
    }

    fn scheduler_failed(self: &Arc<Self>, reason: &'static str, message: String) {
        if self.stopping.load(Ordering::SeqCst) {
            return;
        }
        if self.scheduler_state.compare_exchange(
            SCHEDULER_RUNNING, SCHEDULER_FAILED, Ordering::SeqCst, Ordering::SeqCst,
        ).is_err() {
            return;
        }
        lock(&self.fault).get_or_insert(message);
        // Health changes immediately. Diagnostics must never wait behind a
        // blocked journal on the scheduler or UI thread, and contain no native
        // output, prompts, paths or panic payloads.
        let inner = self.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn_blocking(move || {
                // Journal I/O faults can prevent the event from being saved,
                // so retain a small independent diagnostic as well.
                let mut diagnostic = json!({"reason": reason, "at": now()});
                let sqlite_code = inner.scheduler_sqlite_code.load(Ordering::SeqCst);
                if sqlite_code > 0 {
                    diagnostic["sqlite_code"] = json!(sqlite_code & 0xff);
                    diagnostic["sqlite_extended_code"] = json!(sqlite_code);
                }
                if let Ok(bytes) = serde_json::to_vec(&diagnostic) {
                    let _ = std::fs::write(inner.data_dir.join("scheduler-failure.json"), bytes);
                }
                let _ = inner.db.tx(|q| {
                    let roots = q.all(
                        "SELECT id,ticket,turn FROM jobs WHERE parent IS NULL AND state NOT IN ('done','partial','failed','cancelled')",
                        &[],
                    )?;
                    for root in roots {
                        let mut payload = diagnostic.clone();
                        payload.as_object_mut().expect("diagnostic object").remove("at");
                        q.log(who_of(&root), "scheduler.failed", &payload)?;
                    }
                    Ok(())
                });
            });
        }
    }

    fn admission_open(&self) -> bool {
        self.started.load(Ordering::SeqCst)
            && !self.stopping.load(Ordering::SeqCst)
            && self.scheduler_state.load(Ordering::SeqCst) == SCHEDULER_RUNNING
    }

    fn interrupt_unstarted(&self, turn: &Turn) -> Result<()> {
        self.db.tx(|q| {
            if q.live(turn.job.who())? {
                q.interrupt(turn.job.who(), &turn.job.ticket_profile)?;
            }
            Ok(())
        })
    }

    fn mark_scheduler_progress(&self) {
        let elapsed = self.scheduler_origin.elapsed().as_millis().min(u128::from(u64::MAX - 1)) as u64;
        self.scheduler_progress_ms.store(elapsed + 1, Ordering::SeqCst);
    }

    fn step(self: &Arc<Self>) -> usize {
        if !self.started.load(Ordering::SeqCst) || self.stopping.load(Ordering::SeqCst) || lock(&self.fault).is_some()
        {
            return 0;
        }
        // Queue bookkeeping is part of admission and shares its recoverable
        // BEGIN-busy path. A contended journal must never stop the scheduler.
        match self.drive_integrations().and_then(|()| self.admit()) {
            Ok(n) => {
                self.scheduler_database_busy.store(false, Ordering::SeqCst);
                self.mark_scheduler_progress();
                n
            }
            Err(error) if error.downcast_ref::<BeginBusy>().is_some() => {
                let busy = error.downcast_ref::<BeginBusy>().expect("guard checked BEGIN busy");
                self.scheduler_sqlite_code.store(busy.extended_code as u64, Ordering::SeqCst);
                if !self.scheduler_database_busy.swap(true, Ordering::SeqCst) {
                    let diagnostic = json!({
                        "reason": "begin_busy", "at": now(),
                        "sqlite_code": busy.extended_code & 0xff,
                        "sqlite_extended_code": busy.extended_code,
                    });
                    let path = self.data_dir.join("scheduler-busy.json");
                    if let Ok(bytes) = serde_json::to_vec(&diagnostic) {
                        let _ = std::fs::write(path, bytes);
                    }
                }
                0
            }
            Err(e) => {
                if let Some((_, extended)) = sqlite_error_codes(&e) {
                    self.scheduler_sqlite_code.store(extended as u64, Ordering::SeqCst);
                }
                self.scheduler_failed("admission_error", e.to_string());
                if let Some(t) = lock(&self.sched).timer.take() {
                    t.abort();
                }
                0
            }
        }
    }

    fn workspace_busy(&self, sched: &Sched, keys: &[String]) -> bool {
        sched.active.values().any(|slot| {
            keys.iter().any(|key| {
                crate::workspaces::overlaps(key, &crate::workspaces::scope(&slot.turn.job.cwd))
                    || slot.turn.prerequisite_lease().any(|w| {
                        crate::workspaces::overlaps(key, &crate::workspaces::scope(&w.path))
                    })
            })
        }) || sched.maintenance.iter().any(|leased| {
            keys.iter()
                .any(|key| crate::workspaces::overlaps(key, leased))
        })
    }

    fn integration_cancelled(&self, ticket: i64) -> bool {
        self.stopping.load(Ordering::SeqCst) || self.db.with(|q| {
            Ok(!crate::integration_queue::enabled(q,ticket)? || q.exists(
                "SELECT 1 FROM jobs WHERE ticket=? AND parent IS NULL AND state IN ('cancelled','interrupted','failed')",&[&ticket])?)
        }).unwrap_or(true)
    }

    fn drive_integrations(self: &Arc<Self>) -> Result<()> {
        // Preserve admission's scheduler -> journal lock order, including
        // its read-only disabled-policy check and shutdown observation.
        let mut sched = lock(&self.sched);
        if self.stopping.load(Ordering::SeqCst) {
            return Ok(());
        }
        // Disabled queues do not acquire an extra SQLite writer transaction
        // or reconcile unrelated task state on every normal scheduler tick.
        if !self.db.with(|q| q.exists("SELECT 1 FROM integration_policy WHERE enabled=1", &[]))? {
            return Ok(());
        }
        if self.stopping.load(Ordering::SeqCst) {
            return Ok(());
        }
        sched.integration_tasks.retain(|task| !task.is_finished());
        let items=self.db.admission_tx(|q| {
            crate::integration_queue::reconcile(q)?;
            let mut items=vec![];
            for row in q.all("SELECT DISTINCT i.ticket FROM integration_queue i JOIN integration_policy p ON p.ticket=i.ticket AND p.enabled=1 WHERE i.state='queued'",&[])? {
                items.extend(crate::integration_queue::list(q,int_of(&row,"ticket"))?.into_iter().filter(|item|item.state=="queued"));
            }
            Ok(items)
        })?;
        for item in items {
            if self.integration_cancelled(item.ticket) {
                continue;
            }
            if self.db.with(|q| pause::status(q, item.ticket))? != PauseStatus::Running {
                continue;
            }
            let w = self
                .db
                .with(|q| crate::workspaces::get(q, item.job))?
                .ok_or_else(|| anyhow!("工作区不存在"))?;
            let keys = vec![
                crate::workspaces::scope(&w.path),
                crate::workspaces::scope(&w.source),
            ];
            if self.workspace_busy(&sched, &keys) {
                continue;
            }
            let accepted=self.db.admission_tx(|q| {
                if let Err(error)=crate::integration_queue::current(q,&item) {
                    q.run("UPDATE integration_queue SET state='stale',detail=? WHERE job=?",&[&error.to_string(),&item.job])?;
                    return Ok(false);
                }
                q.run("UPDATE integration_queue SET state='validating',detail='正在验证隔离任务和合并候选',validation='{}' WHERE job=?",&[&item.job])?;
                Ok(true)
            })?;
            if !accepted {
                continue;
            }
            sched.maintenance.extend(keys.clone());
            let owner = self.clone();
            let task = tokio::task::spawn_blocking(move || {
                let _lease = IntegrationLease(owner.clone(), keys);
                let result =
                    crate::integration_queue::prepare(&w, &owner.data_dir, &item.commands, &|| {
                        owner.integration_cancelled(item.ticket)
                    });
                let saved=owner.db.tx(|q| {
                    if !crate::integration_queue::enabled(q,item.ticket)? {
                        q.run("UPDATE integration_queue SET state='cancelled',detail='操作者已停用队列或停止工单' WHERE job=?",&[&item.job])?;
                        return Ok(());
                    }
                    if owner.stopping.load(Ordering::SeqCst) {
                        q.run("UPDATE integration_queue SET state='queued',detail='关闭时取消验证，重开后重新验证' WHERE job=?",&[&item.job])?;
                        return Ok(());
                    }
                    if let Err(error)=crate::integration_queue::current(q,&item) {
                        q.run("UPDATE integration_queue SET state='stale',detail=? WHERE job=?",&[&error.to_string(),&item.job])?;
                        return Ok(());
                    }
                    let ready=match result {
                        Ok(ready)=>ready,
                        Err(error)=>{
                            q.run("UPDATE integration_queue SET state='blocked',detail=? WHERE job=?",&[&error.to_string(),&item.job])?;
                            q.run("UPDATE jobs SET state='partial' WHERE id=? AND state='done'",&[&item.job])?;
                            let row=q.one("SELECT * FROM jobs WHERE id=?",&[&item.job])?.ok_or_else(||anyhow!("任务不存在"))?;
                            let job=JobCtx::from_row(&row);
                            if let Some(parent)=job.parent {
                                platform_note(q,&job,parent,&format!("隔离任务 #{} 的自动合入验证受阻：{}。原项目未改变，请处理后重新验证。",job.id,error))?;
                                q.run("UPDATE jobs SET state='waiting' WHERE id=? AND state='done'",&[&parent])?;
                            }
                            return Ok(());
                        }
                    };
                    crate::workspaces::save(q,&ready)?;
                    let mut state=ready.state.as_str();
                    let mut attempts=item.attempts;
                    if matches!(state,"failed"|"conflict") && pause::status(q,item.ticket)? != PauseStatus::Running {
                        q.run("UPDATE integration_queue SET state='queued',detail='工单已暂停，恢复后重新验证；未发送修复任务',validation=? WHERE job=?",&[&ready.validation.to_string(),&item.job])?;
                        return Ok(());
                    }
                    if matches!(state,"failed"|"conflict") {
                        state="blocked";
                        if attempts<2 && !q.exists("SELECT 1 FROM job_failovers WHERE original=?",&[&item.job])?
                            && !q.exists("SELECT 1 FROM continuous_runs WHERE ticket=? AND boundary_job>=?",&[&item.ticket,&item.job])?
                        {
                            let row=q.one("SELECT * FROM jobs WHERE id=?",&[&item.job])?.ok_or_else(||anyhow!("任务不存在"))?;
                            let job=JobCtx::from_row(&row);
                            let body=format!("平台自动合入验证未通过，请在原任务工作区修复，完成后平台会重新验证。第 {} 次修复（最多2次）。\n{}\n真实验证回执（含输出末尾、冲突路径和候选目录）：\n{}",attempts+1,ready.detail,ready.validation);
                            platform_note(q,&job,job.id,&body)?;
                            q.run("UPDATE jobs SET state='queued',turn=turn+1,dispatch_turn=NULL,ready_after=0 WHERE id=?",&[&job.id])?;
                            if let Some(parent)=job.parent { q.run("UPDATE jobs SET state='waiting' WHERE id=? AND state='done'",&[&parent])?; }
                            attempts+=1; state="repairing";
                            q.log(job.who(),"integration.bounced",&json!({"attempt":attempts,"validation":ready.validation}))?;
                        }
                        if state=="blocked" {
                            q.run("UPDATE jobs SET state='partial' WHERE id=? AND state='done'",&[&item.job])?;
                            let row=q.one("SELECT * FROM jobs WHERE id=?",&[&item.job])?.ok_or_else(||anyhow!("任务不存在"))?;
                            let job=JobCtx::from_row(&row);
                            if let Some(parent)=job.parent {
                                platform_note(q,&job,parent,&format!("隔离任务 #{} 自动修复已达到上限或不能安全续派。请处理后重新验证。\n{}",job.id,ready.validation))?;
                                q.run("UPDATE jobs SET state='waiting' WHERE id=? AND state='done'",&[&parent])?;
                            }
                        }
                    }
                    q.run("UPDATE integration_queue SET state=?,attempts=?,detail=?,validation=? WHERE job=?",&[&state,&attempts,&ready.detail,&ready.validation.to_string(),&item.job])?;
                    let root=q.root(item.ticket)?;
                    q.log(who_of(&root),"integration.result",&json!({"job":item.job,"state":state,"validation":ready.validation}))?;
                    Ok(())
                });
                if let Err(error) = saved {
                    owner.scheduler_failed("integration_result", error.to_string());
                }
            });
            sched.integration_tasks.push(task);
        }
        Ok(())
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
        if !self.started.load(Ordering::SeqCst) || self.stopping.load(Ordering::SeqCst) || lock(&self.fault).is_some() {
            return Ok(0);
        }
        let efficiency = self.opts.efficiency.read().unwrap_or_else(|e| e.into_inner()).clone();
        let health = self.health_value();
        let mut aborts = vec![];
        let admitted = self.db.admission_tx(|q| {
            if !self.started.load(Ordering::SeqCst) || self.stopping.load(Ordering::SeqCst) || lock(&self.fault).is_some() {
                return Ok(vec![]);
            }
            q.skip_dependents()?;
            q.wake()?;
            pause::reconcile(q)?;
            let mut collaborators = HashMap::new();
            if efficiency.max_collaborators > 0 {
                for row in q.all("SELECT ticket,count(*) AS n FROM jobs WHERE state='running' AND parent IS NOT NULL GROUP BY ticket", &[])? {
                    collaborators.insert(int_of(&row, "ticket"), int_of(&row, "n"));
                }
            }
            // Full tickets must not occupy the bounded reader slice.
            let mut full: Vec<i64> = collaborators.iter().filter_map(|(&ticket, &n)|
                (n >= i64::from(efficiency.max_collaborators)).then_some(ticket)).collect();
            full.sort_unstable();
            let exclusion = if full.is_empty() { String::new() } else {
                format!(" AND (j.parent IS NULL OR j.ticket NOT IN ({}))", full.iter().map(i64::to_string).collect::<Vec<_>>().join(","))
            };
            let queued = format!("{} SELECT j.*,t.cwd,t.claude_route,t.follow_config,t.agent_routes,t.allow_lead_routing,t.writes,
        t.permission_profile AS ticket_profile,coalesce(j.permission_override,t.permission_profile) AS profile,
        t.auto_cycles,r.route AS bound_route,c.ticket AS continuous_ticket,coalesce(c.boundary_job,0) AS floor
        FROM jobs j JOIN tickets t ON t.id=j.ticket LEFT JOIN native_routes r ON r.job=j.id
        LEFT JOIN continuous_runs c ON c.ticket=j.ticket
        WHERE j.state='queued' AND j.ready_after<=? AND j.writing=?
        AND NOT EXISTS(SELECT 1 FROM effective_deps d JOIN jobs n ON n.id=d.needs WHERE d.job=j.id AND n.state!='done')
        AND NOT EXISTS(SELECT 1 FROM effective_deps d JOIN jobs n ON n.id=d.needs
          JOIN job_workspaces w ON w.job=n.id JOIN integration_policy p ON p.ticket=n.ticket AND p.enabled=1
          LEFT JOIN integration_queue i ON i.job=n.id WHERE d.job=j.id
          AND (i.job IS NULL OR i.turn!=n.turn OR i.state NOT IN ('awaiting_confirmation','merged')))
        AND NOT EXISTS(SELECT 1 FROM ticket_pause p WHERE p.ticket=j.ticket
          AND (j.parent IS NOT NULL OR p.state='paused' OR p.state='draining'))
        {exclusion} ORDER BY j.id LIMIT ?", crate::job::EFFECTIVE_DEPS_SQL);
            let time = now();
            let mut candidates = q.all(&queued, &[&time, &0, &(READS_PER_SLICE as i64)])?;
            candidates.extend(q.all(&queued, &[&time, &1, &256])?);
            candidates.sort_by_key(|r| int_of(r, "id"));
            let plan = q.exists("SELECT 1 FROM sqlite_master WHERE type='table' AND name='studio_plan'", &[])?;
            let mut slots = vec![];
            let mut occupied: Vec<String> = sched.active.values().filter(|s| s.turn.job.writing)
                .flat_map(|s| std::iter::once(crate::workspaces::scope(&s.turn.job.cwd))
                    .chain(s.turn.prerequisite_lease().map(|w|crate::workspaces::scope(&w.path))))
                .chain(sched.maintenance.iter().cloned()).collect();
            for row in candidates {
                let mut job = JobCtx::from_row(&row);
                if let Some(w) = crate::workspaces::get(q,job.id)? {
                    if w.state == "merged" { continue; }
                    job.cwd = w.path;
                } else if let Some(row)=q.one("SELECT cwd FROM job_directories WHERE job=?",&[&job.id])? {
                    job.cwd=str_of(&row,"cwd").into();
                }
                if sched.active.contains_key(&job.id) {
                    continue;
                }
                let key = crate::workspaces::scope(&job.cwd);
                if sched.maintenance.iter().any(|s| crate::workspaces::overlaps(s,&key))
                    || (job.writing && occupied.iter().any(|s| crate::workspaces::overlaps(s,&key))) { continue; }
                let mut workspace_dependencies=vec![];
                // A dependent that already built its snapshot keeps that
                // version; it neither reads nor leases its prerequisites again.
                if crate::workspaces::get(q,job.id)?.is_some_and(|w| w.state=="planned"
                    || w.validation.get("prerequisite_heads").is_none()) {
                    for row in q.all(&format!("{} SELECT n.id,n.turn FROM effective_deps d JOIN jobs n ON n.id=d.needs WHERE d.job=? ORDER BY n.id",crate::job::EFFECTIVE_DEPS_SQL),&[&job.id])? {
                        if let Some(w)=crate::workspaces::get(q,int_of(&row,"id"))? { workspace_dependencies.push((w,int_of(&row,"turn"))); }
                    }
                }
                if workspace_dependencies.iter().any(|(w,_)|occupied.iter().any(|key|crate::workspaces::overlaps(key,&crate::workspaces::scope(&w.path)))) {
                    continue;
                }
                if !job.is_root() && efficiency.max_collaborators > 0
                    && collaborators.get(&job.ticket).copied().unwrap_or(0) >= i64::from(efficiency.max_collaborators) {
                    continue;
                }
                if let Some(until) = self.route_blocked(&job, time) {
                    if pause::after_failure(q, &job, "交接时执行路由熔断，暂停后续恢复尝试。")? {
                        continue;
                    }
                    if let Some(ids) = self.failover(q, &job, "circuit_open")? {
                        aborts.extend(ids);
                        continue;
                    }
                    q.run("UPDATE jobs SET ready_after=? WHERE id=?", &[&until, &job.id])?;
                    q.log(job.who(), "turn.deferred", &json!({"until": p::seconds_json(until), "reason": "circuit_open"}))?;
                    continue;
                }
                if efficiency.quota_guard
                    && let Ok(route) = p::route(Some(&parse(job.bound_route.as_deref())))
                    && let Some((until, kind, reserve)) = quota_hold(q, &job, &route, time)?
                {
                    if pause::after_failure(q, &job, "订阅额度窗口已用尽，暂停后续恢复尝试。")? {
                        continue;
                    }
                    if let Some(ids) = self.failover(q, &job, "quota_exhausted")? {
                        aborts.extend(ids);
                        continue;
                    }
                    let until = until + 60.0;
                    q.run("UPDATE jobs SET ready_after=? WHERE id=?", &[&until, &job.id])?;
                    q.log(job.who(), "turn.deferred", &json!({"until": p::seconds_json(until),
                        "reason": if reserve { "quota_reserve" } else { "quota_window" }, "window": kind}))?;
                    continue;
                }
                if job.auto_root() {
                    if !job.continuous && job.auto_cycles >= 64 {
                        let state = if q.pending_children(job.id)? { "draining" } else { "failed" };
                        q.run("UPDATE jobs SET state=?,result=? WHERE id=?", &[&state, &BUDGET_REASON, &job.id])?;
                        if p::is_terminal(state) { q.withdraw_question(job.who())?; }
                        q.log(job.who(), state, &json!(BUDGET_REASON))?;
                        continue;
                    }
                    q.run("UPDATE tickets SET auto_cycles=auto_cycles+1 WHERE id=?", &[&job.ticket])?;
                }
                q.run("UPDATE questions SET parked=0 WHERE job=? AND state='pending'", &[&job.id])?;
                let mut packet = envelope(q, &job, health.clone())?;
                if crate::integration_queue::enabled(q,job.ticket)? {
                    packet.insert("integration_queue".into(),json!({"enabled":true,"publication":"operator_confirmation"}));
                }
                if job.is_root() && let Some(control) = pause::resumed_control(q, job.ticket)? {
                    packet.insert("pause_control".into(), control);
                }
                if job.is_root() && pause::consolidating(q, job.ticket)? {
                    packet.insert("pause_after_handoff".into(), json!({
                        "consolidation": true,
                        "instruction": "用户已请求暂停。这是最后一次交接轮，请汇总已完成工作、验证证据、未完成事项和下一步，然后结束本轮。平台已禁止新派工，恢复前不会再启动轮次。"
                    }));
                }
                packet.insert("execution_directory".into(),json!(job.cwd));
                packet.insert("isolated_writers".into(),json!(q.exists("SELECT 1 FROM workspace_policy WHERE ticket=? AND isolated=1", &[&job.ticket])?));
                if let Some(w) = crate::workspaces::get(q,job.id)? { packet.insert("workspace".into(),serde_json::to_value(w)?); }
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
                if job.writing { occupied.push(key); }
                occupied.extend(workspace_dependencies.iter().map(|(w,_)|crate::workspaces::scope(&w.path)));
                if !job.is_root() && efficiency.max_collaborators > 0 {
                    *collaborators.entry(job.ticket).or_insert(0) += 1;
                }
                slots.push(Turn {
                    inner: Arc::downgrade(self),
                    job,
                    packet: Mutex::new(packet),
                    reports,
                    menus: Mutex::new(Map::new()),
                    cancel: CancellationToken::new(),
                    timed_out: AtomicBool::new(false),
                    loop_detected: AtomicBool::new(false),
                    tool_follow: AtomicBool::new(false),
                    memos: AtomicUsize::new(0),
                    route: Mutex::new(None),
                    activity: Mutex::new(Tracker::new(time)),
                    preparing: AtomicBool::new(!workspace_dependencies.is_empty()),
                    workspace_dependencies,
                });
            }
            ensure!(self.admission_open(), "调度准入已停止，本批事务已回滚");
            Ok(slots)
        })?;
        for id in aborts {
            if let Some(slot) = sched.active.get(&id) {
                slot.cancel.cancel();
            }
        }
        let mut n = 0;
        for turn in admitted {
            if !self.admission_open() {
                self.interrupt_unstarted(&turn)?;
                continue;
            }
            n += 1;
            let turn = Arc::new(turn);
            let id = turn.job.id;
            let inner = self.clone();
            let t = turn.clone();
            // The task's cleanup waits for the scheduler lock held here, so
            // the slot is always registered before it can be removed.
            let task = tokio::spawn(async move {
                if let Err(e) = inner.execute(&t).await {
                    inner.scheduler_failed("journal_error", format!("无法保存任务状态：{e}"));
                }
                let mut s = lock(&inner.sched);
                s.active.remove(&t.job.id);
            });
            sched.active.insert(
                id,
                Slot {
                    ticket: turn.job.ticket,
                    cancel: turn.cancel.clone(),
                    task: Some(task),
                    turn: turn.clone(),
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
        self.catalog_request(name, None)
    }

    fn catalog_request(self: &Arc<Self>, name: &str, route: Option<Route>) -> CatalogFuture {
        let key = match &route {
            Some(route) => CatalogKey::Selected(name.into(), route.clone()),
            None => CatalogKey::Full(name.into()),
        };
        let mut map = lock(&self.catalogs);
        if let Some(f) = map.get(&key) {
            return f.clone();
        }
        let probe = match self.adapters.get(name) {
            Some(adapter) => match route {
                Some(route) => match adapter.catalog_for_route(route) {
                    Some(probe) => Some(probe),
                    None => {
                        drop(map);
                        return self.catalog(name);
                    }
                },
                None => Some(adapter.catalog()),
            },
            None => None,
        };
        let weak = Arc::downgrade(self);
        let completed_key = key.clone();
        let fut: BoxFuture<'static, Result<Value, String>> = Box::pin(async move {
            let r = match probe {
                Some(probe) => probe.await.map(|c| c.value).map_err(|e| e.to_string()),
                None => Err("执行端不可用".to_string()),
            };
            if let Some(inner) = weak.upgrade() {
                lock(&inner.catalogs).remove(&completed_key);
            }
            r
        });
        let shared = fut.shared();
        map.insert(key, shared.clone());
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
        self.flush_aborts();
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
        if !self.admission_open() {
            self.interrupt_unstarted(turn)?;
            return Ok(());
        }
        let primary = job.primary();
        turn.ensure_current()?;
        // Also covers pre-upgrade queued/retried jobs. Never initialize the
        // not-yet-provisioned isolated path or sweep inactive historical projects.
        let project_dir = PathBuf::from(turn.packet().get("project").and_then(Value::as_str).unwrap_or(&job.cwd));
        let repository = self.prepare_project(project_dir, job.is_root(), turn.cancel.clone()).await?;
        turn.ensure_current()?;
        if repository.initialized || repository.initial_commit.is_some() {
            self.db.tx(|q| {
                if q.live(job.who())? { q.log(job.who(), "project.git", &json!(repository))?; }
                Ok(())
            })?;
        }
        if let Some(w) = self.db.with(|q| crate::workspaces::get(q,job.id))? {
            let w=crate::workspaces::identity(&w)?;
            self.db.tx(|q|crate::workspaces::save(q,&w))?;
            let data = self.data_dir.clone();
            let prerequisites=turn.workspace_dependencies.clone();
            let valid=self.db.with(|q| {
                for (w,version) in &prerequisites {
                    if !q.exists("SELECT 1 FROM jobs WHERE id=? AND turn=? AND state='done'",&[&w.job,version])? { return Ok(false); }
                }
                Ok(true)
            })?;
            if !valid { return Err(TurnError::Wait(now()+1.0)); }
            let ready = tokio::task::spawn_blocking(move || crate::integration_queue::provision_dependencies(&w,&data,&prerequisites)).await
                .map_err(|_| anyhow!("工作区准备异常"))??;
            self.db.tx(|q| {
                for (w,version) in &turn.workspace_dependencies {
                    ensure!(q.exists("SELECT 1 FROM jobs WHERE id=? AND turn=? AND state='done'",&[&w.job,version])?,"前置任务在工作区准备期间已续派，请重新派发当前依赖任务");
                    if let Some(mut prior)=crate::workspaces::get(q,w.job)? {
                        prior.head=crate::workspaces::head(Path::new(&prior.path))?;
                        crate::workspaces::save(q,&prior)?;
                    }
                }
                crate::workspaces::save(q,&ready)
            })?;
            turn.preparing.store(false, Ordering::SeqCst);
            turn.packet().insert("workspace".into(),json!(ready));
        }
        let project = project_key(turn.packet().get("project").and_then(Value::as_str).unwrap_or(&job.cwd));
        let (commands, criteria) = self.db.with(|q| Ok((checks::configured(q, &project)?,
            crate::delivery::auto_criteria(q, job.ticket)?.0)))?;
        let start_fingerprint = if checks::writer(job) && (!commands.is_empty() || criteria) {
            self.check_fingerprint(turn).await?
        } else { None };
        let (route, fresh) = self.execution_route(turn).await?;
        if fresh && let Some(until) = self.opts.runner.admission_blocked_until(&job.agent, &route, now()) {
            return Err(TurnError::Wait(until));
        }
        let efficiency = self.opts.efficiency.read().unwrap_or_else(|e| e.into_inner()).clone();
        if fresh && efficiency.quota_guard {
            let hold = self.db.with(|q| quota_hold(q, job, &route, now()))?;
            if let Some((until, kind, reserve)) = hold {
                return Err(TurnError::Quota { until, kind, reserve });
            }
        }
        let mut guide = guides::base(efficiency.concise_handoffs);
        if turn.packet().get("integration_queue").is_some() {
            guide.push_str("\n本工单启用隔离任务自动验证合入队列。写任务完成后平台自动创建检查点、验证合并候选，失败可退回原会话修复。通过后等待操作者确认才更新原项目。请在结论中区分隔离工作区完成、候选通过与已确认合入；没有 merged 回执时明确尚未合入。\n");
        }
        if primary {
            guide.push('\n');
            guide.push_str(guides::CONTINUOUS);
        } else if job.profile == "auto" && !job.continuous {
            guide.push('\n');
            guide.push_str(guides::LEGACY_AUTO);
        }
        guide.push_str(guides::ECONOMY);
        guide.push_str(guides::KNOWLEDGE);
        if !commands.is_empty() { guide.push_str(guides::CHECKS); }
        let mut packet = turn.packet().clone();
        packet.remove("completion_checks");
        if !commands.is_empty() { packet.insert("completion_checks".into(), json!(commands)); }
        packet.insert("collaboration_preferences".into(), json!({
            "concise_handoffs": efficiency.concise_handoffs,
        }));
        if let Some(budget) = self.db.with(|q| usage_budget(q, &efficiency, now()))? {
            packet.insert("usage_budget".into(), budget);
        }
        if primary && packet.get("autonomy").and_then(|a| a.get("phase")).and_then(Value::as_str) == Some("discover") {
            guide.push_str(guides::DISCOVERY);
        }
        // Adapters mount the bus only on full-access and auto turns.
        if self.opts.bus.is_some() && matches!(job.profile.as_str(), "full" | "auto") {
            guide.push_str(guides::BUS);
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
        // Any report with a failure (retrying or given up) gets the re-route
        // and SA_CANCEL guidance.
        let failing_report = turn.reports.iter().any(|r| r.get("failure").is_some_and(|f| !f.is_null()));
        let unhealthy = packet
            .get("harness_health")
            .and_then(Value::as_object)
            .is_some_and(|h| !h.is_empty());
        if failing_report || unhealthy {
            guide.push_str(guides::REROUTE);
        }
        // Role pack: runs on the enriched envelope, before the snapshot and
        // before any delivery view is derived from it.
        let selection = crate::context_selection::select(&mut packet, job);
        *turn.packet() = packet.clone();
        let manifest = packet.get("files").cloned().unwrap_or(Value::Null);
        let objects = self.data_dir.join("objects");
        let images = tokio::task::spawn_blocking(move || crate::images::from_manifest(&manifest, &objects))
            .await.map_err(|_| anyhow!("图片校验任务异常"))??;
        if !images.is_empty() && !crate::images::supports_native_images(&job.agent) {
            return Err(anyhow!(crate::images::UNSUPPORTED).into());
        }
        // Keep the admission/enrichment snapshot separate from the delivery
        // view. Reports used by commit_reply must retain every known child.
        let snapshot = packet.clone();
        let snapshot_sha256 = digest(&stringify(&context::sort_keys(&Value::Object(snapshot.clone()))));
        let lazy_mcp_schemas = efficiency.lazy_mcp_schemas && p::is_full(&job.profile);
        // Recovery files live outside the project. Restricted harness policies
        // have no confirmed read grant there, so they retain complete prompts.
        let baseline = if efficiency.incremental_context && p::is_full(&job.profile) {
            self.db.with(|q| context::delivery::baseline(q, job.id, job.session.as_deref(), &route))?
                .filter(|baseline| baseline.lazy_mcp_schemas == lazy_mcp_schemas)
        } else {
            None
        };
        let incremental = baseline.as_ref().is_some_and(|baseline| !baseline.needs_recovery);
        let recovery_delivery = baseline.as_ref().is_some_and(|baseline| baseline.needs_recovery);
        let full_context = if baseline.is_some() {
            Some(context::archive(&snapshot, &self.data_dir)?)
        } else { None };
        let full_prompt_bytes = guide.len() + 1 + stringify(&Value::Object(snapshot.clone())).len();
        let delivered_guide = if let Some(baseline) = baseline {
            let (delta, delivered_guide) = if recovery_delivery {
                context::delivery::recovery(&packet, &guide, &baseline, full_context.as_ref().expect("archived continuation"))
            } else {
                context::delivery::delta(&packet, &guide, &baseline)
            };
            packet = delta;
            delivered_guide
        } else {
            guide.clone()
        };
        let deferred_schemas = lazy_mcp_schemas && context::delivery::defer_mcp_schemas(&mut packet, &snapshot);
        if let Some(reference) = full_context {
            packet.insert("full_context".into(), reference);
        } else if deferred_schemas {
            packet.insert("full_context".into(), context::archive(&snapshot, &self.data_dir)?);
        }
        let mut prompt = format!("{delivered_guide}\n{}", stringify(&Value::Object(packet.clone())));
        let compacted = prompt.len() > MAX_PROMPT_BYTES;
        if compacted {
            // Preserve the original recovery target when compacting a lazy
            // view, reserving room for its exact reference and lookup guide.
            let mut full_view = snapshot.clone();
            let reserved = if deferred_schemas {
                context::delivery::defer_mcp_schemas(&mut full_view, &snapshot);
                json!({"full_context":packet["full_context"],"mcp_schema_lookup":full_view["mcp_schema_lookup"]})
            } else {
                json!({})
            };
            let reserve_bytes = if deferred_schemas { stringify(&reserved).len() } else { 0 };
            packet = context::compact(&full_view, &guide, &self.data_dir, MAX_PROMPT_BYTES.saturating_sub(reserve_bytes))?;
            if deferred_schemas {
                packet.insert("full_context".into(), reserved["full_context"].clone());
                packet.insert("mcp_schema_lookup".into(), reserved["mcp_schema_lookup"].clone());
            }
            prompt = format!("{guide}\n{}", stringify(&Value::Object(packet.clone())));
        }
        turn.ensure_current()?;
        let snapshot_seq = self.db.tx(|q| {
            let snapshot_seq = q.log(job.who(), "context.snapshot", &Value::Object(snapshot.clone()))?;
            q.log(job.who(), "input", &Value::Object(packet.clone()))?;
            q.log(job.who(), "running", &json!({
                "guide": if compacted { &guide } else { &delivered_guide },
                "prompt_sha256": digest(&prompt), "snapshot_seq": snapshot_seq,
                "snapshot_sha256": snapshot_sha256, "incremental": incremental && !compacted,
                "recovery_delivery": recovery_delivery && !compacted,
                "full_prompt_bytes": full_prompt_bytes, "sent_prompt_bytes": prompt.len(),
                "saved_bytes": full_prompt_bytes.saturating_sub(prompt.len()),
                "lazy_mcp_schemas": deferred_schemas,
            }))?;
            let mode = if compacted { "compacted" } else if recovery_delivery { "recovery" } else if incremental { "delta" } else { "full" };
            q.log(job.who(), "context.selection", &crate::context_selection::manifest(
                crate::context_selection::role_of(job), &snapshot, &selection, mode))?;
            Ok(snapshot_seq)
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
            images,
            cwd: PathBuf::from(&job.cwd),
            route: route.clone(),
            profile: if job.profile == "legacy" {
                if job.writing { "partial" } else { "readonly" }.into()
            } else {
                job.profile.clone()
            },
            writable: p::is_full(&job.profile) || job.writing,
            session_id: job.session.clone(),
            bus: self.opts.bus.as_ref().and_then(|b| b.grant(job.ticket, job.id, job.turn, &job.agent)),
        };
        // The token is valid only while this turn runs.
        let _bus = BusLease(self.opts.bus.clone(), request.bus.as_ref().map(|g| g.token.clone()));
        let host: Arc<dyn TurnHost> = Arc::new(Host(turn.clone()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let runner = self.opts.runner.clone();
        if !self.admission_open() {
            self.interrupt_unstarted(turn)?;
            return Ok(());
        }
        lock(&turn.activity).set_phase(ActivityPhase::Spawning, now());
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
                    {
                        let at = now();
                        let mut activity = lock(&turn.activity);
                        for ev in &batch {
                            activity.on_event(ev, at);
                        }
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
        lock(&turn.activity).set_phase(ActivityPhase::Closing, now());
        let mut rest = vec![];
        while let Ok(ev) = rx.try_recv() {
            rest.push(ev);
        }
        if !rest.is_empty() {
            self.absorb(turn, rest, &route, &mut stream).await;
        }
        // A journal/control failure may have cancelled the native task. Keep
        // the original internal error instead of its cancellation response.
        if let Some(fatal) = stream.fatal.take() {
            return Err(fatal.into());
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
        // The native runner and its wall-clock select have completed. Checks
        // have independent command deadlines and retain the admission slot.
        let checked = self.completion_gate(turn, &project, start_fingerprint, &outcome.text, &stream.correction_notes).await?;
        // A completed gate is part of the returned reply. Shutdown after this
        // point must preserve finished work; ticket cancellation still uses live.
        self.db.tx(|q| {
            if !q.live(job.who())? { return Ok(()); }
            self.log_transitions(q, job.who())?;
            // These hints were available to the currently running adapter.
            // Expire only our own hints when that turn successfully replies,
            // so an unread correction cannot schedule a paid continuation.
            for seq in &stream.correction_notes {
                q.run(
                    "UPDATE mailbox SET consumed_turn=? WHERE seq=? AND target=? AND consumed_turn IS NULL",
                    &[&job.turn, seq, &job.id],
                )?;
            }
            self.commit_reply(q, turn, &outcome.text, outcome.session_id.as_deref(), &stream.invalid, checked.as_ref())?;
            q.run("DELETE FROM quota_windows WHERE agent=? AND provider=? AND exhausted=1", &[&job.agent, &route.provider])?;
            let same_session = !(incremental || recovery_delivery) || outcome.session_id == job.session;
            context::delivery::acknowledge(q, job.who(), &route, json!({
                "snapshot_seq": snapshot_seq, "snapshot_sha256": snapshot_sha256,
                "guide_sha256": digest(&guide), "complete": !compacted && same_session,
                "lazy_mcp_schemas": lazy_mcp_schemas,
                "full_context": packet.get("full_context"),
                "recovery_delivery": recovery_delivery && !compacted,
            }))
        })?;
        Ok(())
    }

    async fn check_fingerprint(self: &Arc<Self>, turn: &Arc<Turn>) -> Result<Option<String>, TurnError> {
        let owner = self.clone();
        let turn = turn.clone();
        Ok(tokio::task::spawn_blocking(move || {
            crate::workspaces::fingerprint(Path::new(&turn.job.cwd), &||
                turn.aborted() || owner.stopping.load(Ordering::SeqCst))
        }).await.map_err(|_| anyhow!("工作树指纹计算异常"))??)
    }

    async fn completion_gate(
        self: &Arc<Self>, turn: &Arc<Turn>, project: &str, start: Option<String>, text: &str, corrections: &[i64],
    ) -> Result<Option<checks::Outcome>, TurnError> {
        let job = &turn.job;
        let plan = self.db.tx(|q| checks::plan(q, job, project, text,
            turn.tool_follow.load(Ordering::SeqCst), corrections))?;
        let Some(mut plan) = plan else { return Ok(None); };
        let fingerprint = self.check_fingerprint(turn).await?;
        // A bounced check can fail for reasons outside Git or recover without
        // a tree change. Keep checking that claim until its bounce is resolved.
        // With the delivery gate on, required items still need evidence for an
        // unchanged tree; only the project commands are skipped then (§10.2).
        let unchanged = !plan.has_bounce && fingerprint.is_some() && fingerprint == start;
        if unchanged && !(plan.criteria && plan.gate) { return Ok(None); }
        let commands = plan.commands.clone();
        let cwd = PathBuf::from(&job.cwd);
        let environment = tokio::task::spawn_blocking(move || crate::verification::environment(&cwd, &commands))
            .await.map_err(|_| anyhow!("检查环境识别异常"))?;
        plan.bind_environment(&environment);
        let cached = plan.cached(fingerprint.as_deref());
        let mut runs = vec![];
        let mut failure = None;
        if !cached && !unchanged {
            for argv in &plan.commands {
                turn.ensure_current()?;
                self.db.tx(|q| {
                    ensure!(!turn.aborted() && q.live(job.who())?, "任务已中止");
                    q.log(job.who(), "check.started", &json!({"argv":argv}))?;
                    Ok(())
                })?;
                let owner = self.clone();
                let running = turn.clone();
                let command = argv.clone();
                let started = Instant::now();
                let result = tokio::task::spawn_blocking(move || {
                    crate::workspaces::command_with_cancel(Path::new(&running.job.cwd), &command,
                        Duration::from_secs(600), Some(&|| running.aborted() || owner.stopping.load(Ordering::SeqCst)))
                }).await.map_err(|_| anyhow!("完成检查执行异常"))?;
                // The runner reports interruption while owning the process tree.
                // A later shutdown must preserve a command that already finished.
                let (ok, exit, bytes) = match result {
                    Ok(result) => {
                        if result.cancelled { return Err(anyhow!("任务已中止").into()); }
                        (result.ok, result.exit, result.output)
                    }
                    Err(error) => (false, None, error.to_string().into_bytes()),
                };
                let tail = String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(4000)..]).into_owned();
                let seconds = started.elapsed().as_secs_f64();
                self.db.tx(|q| {
                    ensure!(q.live(job.who())?, "任务已中止");
                    q.log(job.who(), "check.result", &json!({"argv":argv,"ok":ok,"exit":exit,"seconds":seconds,"tail":tail}))?;
                    Ok(())
                })?;
                runs.push(json!({"argv":argv,"ok":ok,"exit":exit,"seconds":seconds}));
                if !ok {
                    failure = Some((argv.clone(), exit, tail));
                    break;
                }
            }
        }
        // Operator-adopted delivery criteria run only after the project checks
        // pass, on a snapshot of this job's own work directory.
        let delivery = if failure.is_none() { self.auto_delivery(turn, unchanged).await? } else { None };
        // Unchanged tree with evidence already current: nothing new to record.
        if unchanged && delivery.is_none() { return Ok(None); }
        let mut payload = json!({"ok":true,"cached":cached,"attempt":plan.attempt,"runs":runs,
            "verification_key":plan.checks_sha});
        if unchanged { payload["unchanged"] = json!(true); }
        if let Some(delivery) = delivery {
            payload["delivery"] = delivery.payload;
            failure = delivery.failure;
        }
        payload["ok"] = json!(failure.is_none());
        Ok(Some(checks::Outcome { plan, fingerprint, payload, failure }))
    }

    async fn auto_delivery(
        self: &Arc<Self>, turn: &Arc<Turn>, unchanged: bool,
    ) -> Result<Option<crate::delivery::AutoOutcome>, TurnError> {
        let job = &turn.job;
        let engine = Engine { inner: self.clone() };
        let running = turn.clone();
        let request = crate::delivery::AutoRequest {
            ticket: job.ticket, job: job.id, turn: job.turn, cwd: job.cwd.clone(), unchanged,
        };
        let result = tokio::task::spawn_blocking(move || {
            engine.auto_delivery(&request, &|| running.aborted())
        }).await.map_err(|_| anyhow!("交付证据采集异常"))?;
        if turn.aborted() { return Err(anyhow!("任务已中止").into()); }
        let outcome = result?;
        if let Some(outcome) = &outcome {
            self.db.tx(|q| {
                ensure!(q.live(job.who())?, "任务已中止");
                q.log(job.who(), "delivery.auto", &outcome.payload)?;
                Ok(())
            })?;
        }
        Ok(outcome)
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
        let full_menus = job.is_root() || job.primary();
        if full_menus {
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
        // A collaborator with a complete fixed route needs live validation of
        // that route. Defaults still require full discovery to resolve them.
        let fixed_route = if full_menus {
            None
        } else if !fresh {
            Some(bound.clone())
        } else if p::truthy_value(&selected_execution) {
            Some(selected_execution.clone())
        } else {
            pick(&job.agent, None)
        };
        let selected_probe = fixed_route.as_ref().and_then(|value| p::route(Some(value)).ok());
        let cat = match (memo.remove(&job.agent), selected_probe) {
            (Some(r), _) => r,
            (None, Some(route)) => self.catalog_request(&job.agent, Some(route)).await,
            (None, None) => self.catalog(&job.agent).await,
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
        match p::options(Some(&cat), Some(&route_json(&selected))) {
            Ok(Some(_)) => {}
            Ok(None) => return Err(route_error(crate::resilience::CODE_ROUTE_NOT_CONFIGURED, UNAVAILABLE.into())),
            Err(message) => return Err(route_error(crate::resilience::CODE_ROUTE_NOT_CONFIGURED, message)),
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
        if job.agent == "claude" {
            for (kind, data) in &journal {
                if kind == "native" {
                    self.observe_cache(job.id, data);
                }
            }
        }
        let windows: Vec<_> = journal.iter().filter(|(kind, data)| kind == "native" && quota_frame(data))
            .flat_map(|(_, data)| sa_store::quota::windows_from_native(&job.agent, data)).collect();
        let route_text = stringify(&route_json(route));
        let record = |q: &Q| {
            if !q.live(job.who())? {
                return Ok(false);
            }
            for (kind, data) in &journal {
                let seq = q.log(job.who(), kind, data)?;
                if kind == "native" {
                    context::delivery::record_compaction(q, job.who(), job.admission, seq, data)?;
                }
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
            if !windows.is_empty() {
                observe_windows(q, job.who(), &job.agent, &route.provider, &windows, now())?;
            }
            // Early-persisted native session (spec §9.2 D.1).
            for s in &sessions {
                q.run(
                    "INSERT OR IGNORE INTO native_sessions(job,turn,agent,route,session) VALUES(?,?,?,?,?)",
                    &[&job.id, &job.turn, &job.agent, &route_text, s],
                )?;
            }
            Ok(true)
        };
        // A native session id is what a restart resumes from, so it is forced
        // to disk; a batch of stream frames alone is telemetry.
        let saved = if sessions.is_empty() && windows.is_empty() {
            self.db.telemetry_tx(record)
        } else {
            self.db.tx(record)
        };
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
            if kind == "native" && let Some(signal)=st.detector.observe(&job.agent,&data) {
                let stop=signal==crate::stuck::Signal::Stop;
                let message=if stop {"工具操作与结果持续重复，已停止本轮以避免空转；请调整策略后重试。"} else {"检测到重复操作与相同结果，请改变方法或说明等待的外部条件。"};
                let _=self.db.tx(|q| {
                    q.log(job.who(),if stop {"turn.loop_stopped"} else {"turn.loop_warning"},&json!({"reason":message}))?;
                    if !stop { platform_note(q,job,job.id,message)?; }
                    Ok(())
                });
                if stop {turn.loop_detected.store(true,Ordering::SeqCst);turn.cancel.cancel();}
            }
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
        // External effects and shared knowledge require the authoritative
        // final reply.
        if streaming && (line.starts_with("SA_TOOL ") || line.starts_with("SA_MEMO ") || line.starts_with("SA_REPORT ") || line.starts_with("SA_ASK ")) {
            return;
        }
        if st.fatal.is_some() || turn.aborted() {
            return;
        }
        let job = &turn.job;
        let cleaned = line.strip_suffix('\r').unwrap_or(line);
        let final_control = cleaned.split_once(' ').filter(|(tag, _)| matches!(*tag, "SA_REPORT" | "SA_ASK"));
        let result = if let Some((tag, body)) = final_control {
            p::final_control(tag, body).map(|_| ()).map_err(CmdError::Rejected)
        } else {
            dispatch::command(turn, cleaned, streaming).await
        };
        let Err(err) = result else {
            return;
        };
        let tag = line.split(' ').next().unwrap_or_default().to_string();
        if matches!(err, CmdError::Other(_)) {
            st.fatal = Some(err.into_anyhow());
            turn.cancel.cancel();
            return;
        }
        let saved = if streaming && matches!(err, CmdError::Syntax(_)) {
            self.db
                .with(|q| q.log(job.who(), "protocol.preview_invalid", &json!({"tag": tag})))
                .map(|_| None)
        } else {
            let reason = err.message();
            // Final-only declarations are independent of autonomy controls.
            // Their rejection receipt must not invalidate a valid SA_FINISH.
            if job.primary() && !matches!(tag.as_str(), "SA_REPORT" | "SA_ASK") {
                match st.invalid.iter_mut().find(|(t, _)| *t == tag) {
                    Some(entry) => entry.1 = reason.clone(),
                    None => st.invalid.push((tag.clone(), reason.clone())),
                }
            }
            self.reject_command(turn, cleaned, &tag, &reason, streaming)
        };
        match saved {
            Ok(Some(seq)) => st.correction_notes.push(seq),
            Ok(None) => {}
            Err(error) => {
                st.fatal = Some(error);
                turn.cancel.cancel();
            }
        }
    }

    /// Rejections are durable receipts, independent of subsequent valid
    /// commands. The transaction also makes parent notification idempotent.
    fn reject_command(
        &self,
        turn: &Turn,
        line: &str,
        tag: &str,
        reason: &str,
        streaming: bool,
    ) -> Result<Option<i64>> {
        let job = &turn.job;
        let command_sha256 = digest(line);
        let logical = job.logical();
        let command: Value = line
            .split_once(' ')
            .and_then(|(_, body)| serde_json::from_str(body).ok())
            .unwrap_or(Value::Null);
        let id = command.get("id")
            .filter(|id| p::valid_text(Some(id), 96))
            .cloned()
            .unwrap_or(Value::Null);
        self.db.tx(|q| {
            if !q.live(job.who())? || q.exists(
                "SELECT 1 FROM events WHERE job=? AND kind='protocol.rejected' AND json_extract(payload,'$.logical_turn')=? AND json_extract(payload,'$.command_sha256')=? LIMIT 1",
                &[&job.id, &logical, &command_sha256],
            )? {
                return Ok(None);
            }
            let receipt = json!({
                "tag": tag, "id": id, "logical_turn": logical,
                "command_sha256": command_sha256, "reason": reason, "executed": false,
            });
            q.log(job.who(), "protocol.rejected", &receipt)?;
            if job.primary() {
                q.log(job.who(), "auto.protocol", &json!({"tag": tag, "reason": reason}))?;
            }
            let body = format!("任务 #{} 的 {} 指令被拒绝。该指令未执行，其他工作仍可继续。请依据原因修正控制消息，保留并报告已经完成的业务成果。\n{}",
                job.id, tag, stringify(&receipt));
            if let Some(parent) = job.parent
                && q.exists(
                    "SELECT 1 FROM jobs WHERE id=? AND ticket=? AND state IN ('queued','running','waiting')",
                    &[&parent, &job.ticket],
                )?
            {
                platform_note(q, job, parent, &body)?;
            }
            if streaming && !job.primary() {
                return platform_note(q, job, job.id, &body).map(Some);
            }
            Ok(None)
        })
    }

    /// `commitReply(ctx, result, reports, primary, invalid)` (E:409-485).
    fn commit_reply(
        &self,
        q: &Q,
        turn: &Turn,
        text: &str,
        session: Option<&str>,
        invalid: &[(String, String)],
        checked: Option<&checks::Outcome>,
    ) -> Result<()> {
        let job = &turn.job;
        if !q.live(job.who())? {
            return Ok(());
        }
        let check_failed = checked.is_some_and(checks::Outcome::failed);
        let bounce = checked.is_some_and(checks::Outcome::bounced);
        if let Some(checked) = checked { checked.save(q, job)?; }
        else { q.run("DELETE FROM job_checks WHERE job=?", &[&job.id])?; }
        // A bounced completion claim has no accepted agent outcome yet.
        let outcome = if bounce { None } else { p::last_final_control(text, "SA_REPORT") };
        let ask = p::last_final_control(text, "SA_ASK");
        if let Some(outcome) = &outcome {
            let payload = stringify(outcome);
            q.run("INSERT OR REPLACE INTO job_outcomes(job,turn,payload) VALUES(?,?,?)", &[&job.id, &job.turn, &payload])?;
            q.log(job.who(), "report", outcome)?;
        } else {
            q.run("DELETE FROM job_outcomes WHERE job=?", &[&job.id])?;
        }
        let reports = &turn.reports;
        let seen: HashMap<i64, &Value> = reports
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_i64).map(|id| (id, r)))
            .collect();
        let children = q.all(
            "SELECT j.id,j.state,j.result,f.replacement FROM jobs j LEFT JOIN job_failovers f ON f.original=j.id
      WHERE j.parent=? AND j.id>?",
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
        if job.auto_root() && !follow && ask.is_none() && !check_failed {
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
        // A child replaced by automatic failover counts through its sibling.
        if state == "done"
            && !job.auto_root()
            && children.iter().any(|c| {
                matches!(str_of(c, "state"), "partial" | "failed" | "cancelled")
                    && c.get("replacement").is_none_or(Value::is_null)
                    && !outcome.as_ref().and_then(|o| o.get("resolved")).and_then(Value::as_array)
                        .is_some_and(|rows| rows.iter().any(|row| row["job"] == c["id"]))
            })
        {
            state = "partial";
        }
        // A question takes the completion slot before the reply can become
        // terminal, including when an unresolved child would make it partial:
        // otherwise the question would be withdrawn before the person saw it.
        let parked = ask.is_some() && matches!(state, "done" | "partial");
        if parked { state = "waiting"; }
        if state == "done" && !job.auto_root()
            && outcome.as_ref().is_some_and(|o| matches!(o["status"].as_str(), Some("partial" | "blocked")))
        {
            state = "partial";
        }
        let redirect = job.profile == "auto" && follow;
        if redirect {
            state = "queued";
        }
        // advance may have parked a continuous primary on a platform question.
        let platform_parked = q.exists("SELECT 1 FROM questions WHERE job=? AND turn=? AND state='pending' AND origin='platform' AND parked=1", &[&job.id, &job.turn])?;
        if !platform_parked { q.commit_question(job.who(), ask.as_ref(), parked && !redirect)?; }
        if check_failed {
            let checked = checked.expect("failed check outcome");
            if bounce {
                state = if redirect { "queued" } else { "waiting" };
                platform_note(q, job, job.id, &checked.note())?;
                q.log(job.who(), "check.bounced", &json!({"attempt":checked.plan.attempt}))?;
            } else if job.auto_root() && !follow {
                let argv = &checked.failure.as_ref().expect("failed command").0;
                q.platform_question(job.who(), "completion-checks", &format!("完成检查连续 3 次未通过：{}。要怎么继续？", argv.join(" ")), &["继续修复", "停止"])?;
                state = "waiting";
            } else if state == "done" { state = "partial"; }
        }
        if job.is_root() {
            knowledge::root_progress(q, job, state, text)?;
        }
        q.run(
            "UPDATE jobs SET result=?,session=?,state=?,dispatch_turn=NULL,turn=turn+? WHERE id=?",
            &[&text, &session, &state, &(redirect as i64), &job.id],
        )?;
        if p::is_terminal(state) { q.withdraw_question(job.who())?; }
        q.log(
            job.who(),
            "reply",
            &json!({"text": text, "sha256": digest(text), "session": session, "state": state}),
        )?;
        let ids:Vec<i64>=turn.packet().get("scm_feedback").and_then(Value::as_array).into_iter().flatten().filter_map(|v|v["id"].as_i64()).collect();
        crate::scm::acknowledge(q,job.id,&ids,job.turn)?;
        pause::after_reply(q, job, outstanding)?;
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
        if job.is_root() && pause::status(q, job.ticket)? != PauseStatus::Running {
            let error = match err {
                TurnError::Adapter(error) => json!({"class": class_name(error.class), "code": error.code, "http_status": error.http_status}),
                _ => json!({}),
            };
            q.log(who, "turn.pause_error", &json!({"reason": reason, "error": error, "timed_out": timed_out}))?;
        }
        if pause::after_failure(q, job, &reason)? {
            return Ok(());
        }
        if turn.loop_detected.load(Ordering::SeqCst) {
            return self.give_up(q,job,"工具操作与结果持续重复，已停止空转；请调整方法后重试。");
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
            if let Some(ids) = self.failover(q, job, "circuit_open")? {
                lock(&self.pending_aborts).extend(ids);
                return Ok(());
            }
            return self.requeue(q, job, *until, None, "turn.route_wait", json!({
                "until": p::seconds_json(*until),
                "class": "route_unavailable",
                "route": route.as_ref().map(route_json),
            }));
        }
        if let TurnError::Quota { until, kind, reserve } = err {
            if let Some(ids) = self.failover(q, job, "quota_exhausted")? {
                lock(&self.pending_aborts).extend(ids);
                return Ok(());
            }
            return self.requeue(q, job, *until + 60.0, None, "turn.quota_wait", json!({
                "until": p::seconds_json(*until + 60.0), "window": kind,
                "reason": if *reserve { "quota_reserve" } else { "quota_window" },
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
                reasoning_effort: None,
            });
            let quota = if e.quota_exhausted() {
                Some(record_quota_failure(q, job, &route, e, now())?)
            } else { None };
            let mut ledger = ledger(q, job.id)?;
            let rotation = self.opts.rotation.candidates(&job.agent);
            let decision = self.opts.runner.decide(&mut ledger, &job.agent, &route, e, &rotation, now());
            self.log_transitions(q, who)?;
            // Optional automatic failover of read-only children (spec §9.2 F):
            // an unavailable route, or a breaker that is now open, moves the
            // work to a sibling on another harness instead of waiting. The
            // operator's provider rotation on the same harness goes first.
            let rotating = matches!(decision, Some(Recovery::Rotate { .. }));
            let open = matches!(decision, Some(Recovery::WaitForRoute { .. }))
                || self
                    .opts
                    .runner
                    .health()
                    .iter()
                    .any(|(a, r, h)| *a == job.agent && r.provider == route.provider
                        && r.model == route.model && r.mode == route.mode && *h == Health::Open);
            let reason = if rotating {
                None
            } else if e.quota_exhausted()
                && self.opts.efficiency.read().unwrap_or_else(|e| e.into_inner()).quota_guard {
                Some("quota_exhausted")
            } else if e.class == ErrorClass::RouteUnavailable {
                Some("route_unavailable")
            } else if open {
                Some("circuit_open")
            } else {
                None
            };
            if let Some(reason) = reason
                && let Some(ids) = self.failover(q, job, reason)?
            {
                lock(&self.pending_aborts).extend(ids);
                return Ok(());
            }
            if let Some(decision) = decision {
                return self.recover(q, job, &route, e, decision, ledger.failures, quota.as_ref());
            }
        }
        if let TurnError::Adapter(e) = err && e.native_retry_exhausted() {
            return self.give_up(q, job, &reason);
        }
        if auto_root && (timed_out || TRANSIENT.is_match(&reason)) {
            let attempts = q.int(
                "SELECT count(*) AS n FROM events WHERE job=? AND kind='auto.retry' AND seq>
        coalesce((SELECT max(seq) FROM events WHERE job=? AND kind IN ('reply','user.follow')),0)",
                &[&job.id, &job.id],
            )?;
            if (job.continuous && pause::status(q, job.ticket)? == PauseStatus::Running) || attempts < 2 {
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
        if p::is_terminal(state) { q.withdraw_question(job.who())?; }
        q.log(job.who(), state, &json!(reason))?;
        Ok(())
    }

    /// Optional automatic failover of a read-only child (spec §9.2 F), for
    /// tickets with `ticket_failover`. Creates a sibling with the same body
    /// on the first other harness, in `AGENTS` order, that the parent's
    /// latest `delegate_models` lists as available (any configured harness
    /// when the parent has no delegate menu) and whose route is not
    /// circuit-open, skipping harnesses already tried in this failover
    /// chain. Logs `send {…, failover_of, reason}` on the parent and cancels
    /// the original and its subtree. Writers and roots never fail over.
    /// Returns the cancelled job ids, or `None` when nothing was done.
    fn failover(&self, q: &Q, job: &JobCtx, reason: &str) -> Result<Option<Vec<i64>>> {
        let Some(parent) = job.parent else { return Ok(None) };
        // This setting can change while the native turn is running. Read
        // the current authorization in the same transaction as the move,
        // rather than taking a snapshot at admission.
        if job.writing || !q.exists("SELECT 1 FROM ticket_failover WHERE ticket=?", &[&job.ticket])? {
            return Ok(None);
        }
        let Some(original) = q.one("SELECT * FROM jobs WHERE id=?", &[&job.id])? else {
            return Ok(None);
        };
        if p::is_terminal(str_of(&original, "state")) || int_of(&original, "writing") != 0 {
            return Ok(None);
        }
        let parent_row = q
            .one("SELECT * FROM jobs WHERE id=? AND ticket=?", &[&parent, &job.ticket])?
            .filter(|r| matches!(str_of(r, "state"), "queued" | "running" | "waiting"));
        let Some(parent_row) = parent_row else { return Ok(None) };
        let mut tried = vec![job.agent.clone()];
        let mut cursor = job.id;
        while let Some(prev) = q.one(
            "SELECT f.original,j.agent FROM job_failovers f JOIN jobs j ON j.id=f.original WHERE f.replacement=?",
            &[&cursor],
        )? {
            tried.push(str_of(&prev, "agent").to_string());
            cursor = int_of(&prev, "original");
        }
        let menus = context::delivery::full_input(q, parent, i64::MAX)?
            .and_then(|r| parse(Some(str_of(&r, "payload"))).get("delegate_models").cloned())
            .and_then(|d| match d {
                Value::Object(m) if !m.is_empty() => Some(m),
                _ => None,
            });
        let health = self.opts.runner.health();
        let manifest = q
            .one("SELECT manifest FROM job_files WHERE job=?", &[&job.id])?
            .map(|m| parse(Some(str_of(&m, "manifest"))))
            .unwrap_or_else(|| json!([]));
        let needs_images = manifest.as_array().is_some_and(|files| {
            files.iter().any(|file| file["name"].as_str().is_some_and(crate::images::is_image_name))
        });
        let blocked = |agent: &str, route: Option<&Route>| {
            health.iter().any(|(a, r, h)| {
                a == agent && matches!(h, Health::Open | Health::HalfOpen)
                    && route.is_none_or(|x| x.provider == r.provider && x.model == r.model && x.mode == r.mode)
            })
        };
        let target = AGENTS.into_iter().find(|agent| {
            if tried.iter().any(|t| t == agent)
                || !self.adapters.contains_key(*agent)
                || (needs_images && !crate::images::supports_native_images(agent))
            {
                return false;
            }
            let Some(menus) = &menus else { return !blocked(agent, None) };
            let Some(menu) = menus.get(*agent) else { return false };
            if menu.get("available") == Some(&Value::Bool(false)) {
                return false;
            }
            let model = menu
                .get("models")
                .and_then(|m| m.get(0))
                .or_else(|| menu.get("default_model"))
                .cloned()
                .unwrap_or(Value::Null);
            let route = json!({"provider": menu["provider"], "model": model, "mode": menu["mode"]});
            p::route(Some(&route)).is_ok_and(|r| !blocked(agent, Some(&r)))
        });
        let Some(target) = target else { return Ok(None) };
        let msgkey = format!("{}#failover:{}", str_of(&original, "msgkey"), job.id);
        let role = str_of(&original, "assigned_role").to_string();
        let sender = original.get("sender").and_then(Value::as_str);
        let source_turn = original.get("source_turn").and_then(Value::as_i64);
        let narrowed = original.get("permission_override").and_then(Value::as_str);
        let sibling = q.insert(
            "INSERT INTO jobs(ticket,parent,sender,agent,body,writing,depth,source_turn,
      msgkey,state,role,assigned_role,route_override,permission_override) VALUES(?,?,?,?,?,0,?,?,?,'queued',?,?,NULL,?)",
            &[
                &job.ticket,
                &parent,
                &sender,
                &target,
                &job.body,
                &job.depth,
                &source_turn,
                &msgkey,
                &role,
                &role,
                &narrowed,
            ],
        )?;
        q.run(
            "INSERT INTO job_files(job,manifest) SELECT ?,manifest FROM job_files WHERE job=?",
            &[&sibling, &job.id],
        )?;
        q.log(
            who_of(&parent_row),
            "send",
            &json!({
                "to": target,
                "child": sibling,
                "body": job.body,
                "sha256": digest(&job.body),
                "write": false,
                "files": manifest,
                "role": role,
                "selection": Value::Null,
                "permission_profile": job.profile,
                "failover_of": job.id,
                "reason": reason,
            }),
        )?;
        q.run(
            "INSERT INTO job_failovers(original,replacement) VALUES(?,?)",
            &[&job.id, &sibling],
        )?;
        q.run("INSERT INTO job_deps(job,needs) SELECT ?,needs FROM job_deps WHERE job=?", &[&sibling, &job.id])?;
        let note = format!("执行端 {} 不可用，已自动改派给 {target}（任务 #{sibling}）", job.agent);
        Ok(Some(q.cancel_subtree(job.id, &note)?))
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

    #[allow(clippy::too_many_arguments)]
    fn recover(
        &self,
        q: &Q,
        job: &JobCtx,
        route: &Route,
        err: &AdapterError,
        decision: Recovery,
        failures: u32,
        quota: Option<&sa_store::quota::Window>,
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
                    "reason": err.message,
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
                    "error_message": err.message,
                    "from": route_json(route),
                    "to": route_json(&to),
                    "reason": class_name(reason),
                    "attempt": attempt,
                    "after_ms": delay.as_millis() as u64,
                }))?;
                notice(q, delay.as_secs_f64())
            }
            Recovery::WaitForRoute { until_secs } => self.requeue(q, job, until_secs, None, "turn.route_wait", json!({
                "reason": err.message,
                "until": p::seconds_json(until_secs),
                "class": class_name(err.class),
                "route": route_json(route),
            })),
            Recovery::GiveUp { class, attempts } => {
                let guard = self.opts.efficiency.read().unwrap_or_else(|e| e.into_inner()).quota_guard;
                if guard && err.quota_exhausted() && let Some(window) = quota {
                    let until = window.resets_at.unwrap_or(at + 1800.0) + 60.0;
                    if until - at > QUOTA_WAIT_MAX {
                        q.log(job.who(), "turn.gave_up", &json!({"class": "billing", "attempts": attempts,
                            "code": err.code, "quota_until": p::seconds_json(until)}))?;
                        return self.give_up(q, job, &format!("订阅额度窗口已用尽，预计 {} 重置；等待时间超过 6 小时，请在重置后继续或改用其他供应商",
                            classify::utc_minute(window.resets_at.unwrap_or(until) as i64)));
                    }
                    let waits = q.int("SELECT count(*) FROM events WHERE job=? AND kind='turn.quota_wait' AND seq>
                        coalesce((SELECT max(seq) FROM events WHERE job=? AND kind IN ('reply','user.follow','retry')),0)", &[&job.id, &job.id])?;
                    if waits < 12 {
                        // The first refused turn can establish a native session too.
                        if let Some(session) = err.session_id.as_deref() {
                            q.run("UPDATE jobs SET session=? WHERE id=?", &[&session, &job.id])?;
                        }
                        self.requeue(q, job, until, None, "turn.quota_wait", json!({
                            "until": p::seconds_json(until), "window": window.kind, "reason": "quota_window",
                            "class": "billing", "route": route_json(route), "code": err.code,
                        }))?;
                        if waits == 0 {
                            notify_parent_as(q, job, json!("quota_window"), 1, until)?;
                        }
                        return Ok(());
                    }
                }
                // A continuous project keeps retrying transient failures,
                // bounded per hour (spec §9.2 A).
                if job.primary() && classify::is_retryable(class) && !err.native_retry_exhausted()
                    && pause::status(q, job.ticket)? == PauseStatus::Running {
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

/// The longest the scheduler holds or parks work for a usage window. Beyond
/// it (a weekly limit, typically) the work runs and, if refused, fails with
/// the reset time instead of sitting queued for days.
const QUOTA_WAIT_MAX: f64 = 6.0 * 3600.0;

/// Most frames contain no account-level observation; avoid parsing them twice.
fn quota_frame(data: &Value) -> bool {
    data.get("rate_limit_info").is_some()
        || data.get("type").and_then(Value::as_str) == Some("rate_limit_event")
        || data.get("method").and_then(Value::as_str).is_some_and(|m| m.starts_with("account/rateLimits/"))
}

fn observe_windows(q: &Q, who: Who, agent: &str, provider: &str, windows: &[sa_store::quota::Window], at: f64) -> Result<()> {
    let mut changed = false;
    for window in windows {
        changed |= sa_store::quota::upsert(q.conn(), agent, provider, window, at)?;
    }
    if changed {
        q.log(who, "quota.window", &json!({"agent": agent, "provider": provider, "windows": windows}))?;
    }
    Ok(())
}

enum QuotaStatus {
    Exhausted { until: f64, kind: String },
    Reserve { until: f64, kind: String },
    Clear,
}

/// Admission reads the same transaction that claims a collaborator slot.
fn quota_status(q: &Q, agent: &str, provider: &str, at: f64, latest_reset: Option<f64>) -> Result<QuotaStatus> {
    let rows = q.all("SELECT kind,resets_at,observed_at,exhausted,used FROM quota_windows
        WHERE agent=? AND provider=? AND (resets_at>? OR (resets_at IS NULL AND observed_at>=?))
        AND (? IS NULL OR coalesce(resets_at,observed_at+1800)<=?)
        ORDER BY exhausted DESC,coalesce(resets_at,observed_at+1800) DESC,kind",
        &[&agent, &provider, &at, &(at - 21600.0), &latest_reset, &latest_reset])?;
    let mut reserve = QuotaStatus::Clear;
    for row in rows {
        let reset = row.get("resets_at").and_then(Value::as_f64);
        let kind = str_of(&row, "kind").to_string();
        if int_of(&row, "exhausted") != 0 {
            let until = reset.unwrap_or_else(|| row.get("observed_at").and_then(Value::as_f64).unwrap_or(at) + 1800.0);
            // An observation without a reset authorizes a probe after 30 minutes.
            // Keeping it exhausted beyond that time would requeue to the past.
            if until > at { return Ok(QuotaStatus::Exhausted { until, kind }); }
            continue;
        }
        if matches!(reserve, QuotaStatus::Clear)
            && row.get("used").and_then(Value::as_f64).is_some_and(|used| used >= 0.90)
            && let Some(until) = reset {
            reserve = QuotaStatus::Reserve { until, kind };
        }
    }
    Ok(reserve)
}

fn quota_hold(q: &Q, job: &JobCtx, route: &Route, at: f64) -> Result<Option<(f64, String, bool)>> {
    // Filter before selecting a window so a distant weekly reset cannot
    // hide a shorter window that still requires an admission hold.
    match quota_status(q, &job.agent, &route.provider, at, Some(at + QUOTA_WAIT_MAX))? {
        QuotaStatus::Exhausted { until, kind } => Ok(Some((until, kind, false))),
        QuotaStatus::Reserve { until, kind } if !job.is_root() => {
            let root = q.one("SELECT j.agent,r.route FROM jobs j JOIN native_routes r ON r.job=j.id
                WHERE j.ticket=? AND j.parent IS NULL", &[&job.ticket])?;
            let shared = root.as_ref().is_some_and(|root| str_of(root, "agent") == job.agent
                && p::route(Some(&parse(root.get("route").and_then(Value::as_str))))
                    .is_ok_and(|root_route| root_route.provider == route.provider));
            Ok(shared.then_some((until, kind, true)))
        }
        _ => Ok(None),
    }
}

fn record_quota_failure(q: &Q, job: &JobCtx, route: &Route, err: &AdapterError, at: f64) -> Result<sa_store::quota::Window> {
    let mut kind = err.code.as_deref().and_then(|code| code.strip_prefix("quota:")).unwrap_or("window").to_string();
    let reset = if let Some(delay) = err.retry_after {
        at + delay.as_secs_f64()
    } else if let Some(row) = q.one("SELECT kind,resets_at FROM quota_windows WHERE agent=? AND provider=?
        AND resets_at>? AND (used>=0.95 OR exhausted=1) ORDER BY resets_at LIMIT 1", &[&job.agent, &route.provider, &at])? {
        kind = str_of(&row, "kind").to_string();
        row.get("resets_at").and_then(Value::as_f64).unwrap_or(at + 1800.0)
    } else {
        // A probe time is not a provider-reported reset for a named window.
        kind = "unknown".into();
        at + 1800.0
    };
    let previous = q.one("SELECT used,minutes FROM quota_windows WHERE agent=? AND provider=? AND kind=?",
        &[&job.agent, &route.provider, &kind])?;
    let window = sa_store::quota::Window {
        kind,
        used: previous.as_ref().and_then(|row| row.get("used")).and_then(Value::as_f64),
        resets_at: Some(reset),
        exhausted: true,
        minutes: previous.as_ref().and_then(|row| row.get("minutes")).and_then(Value::as_i64),
    };
    observe_windows(q, job.who(), &job.agent, &route.provider, std::slice::from_ref(&window), at)?;
    Ok(window)
}

fn usage_budget(q: &Q, settings: &sa_store::efficiency::Settings, at: f64) -> Result<Option<Value>> {
    let mut budget = Map::new();
    if settings.max_collaborators > 0 {
        budget.insert("max_parallel_collaborators".into(), json!(settings.max_collaborators));
    }
    if settings.quota_guard {
        let mut windows = vec![];
        for observed in sa_store::quota::load(q.conn(), at)? {
            let window = observed.window;
            if !window.exhausted && !window.used.is_some_and(|used| used >= 0.5) {
                continue;
            }
            // "reserve" is announced only where admission actually holds work.
            let soon = window.resets_at.is_some_and(|reset| reset - at <= QUOTA_WAIT_MAX);
            let state = if window.exhausted { "exhausted" }
                else if soon && window.used.is_some_and(|used| used >= 0.90) { "reserve" } else { "ok" };
            let mut item = json!({"agent": observed.agent, "provider": observed.provider, "state": state});
            if let Some(minutes) = window.minutes { item["window_minutes"] = json!(minutes); }
            if let Some(used) = window.used { item["used_percent"] = json!(((used * 20.0).round() * 5.0) as i64); }
            if let Some(reset) = window.resets_at { item["resets_in_minutes"] = json!((((reset - at) / 300.0).round() * 5.0) as i64); }
            windows.push(item);
        }
        if !windows.is_empty() { budget.insert("windows".into(), Value::Array(windows)); }
    }
    Ok((!budget.is_empty()).then_some(Value::Object(budget)))
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

fn cancel_ticket(q: &Q, ticket: i64) -> Result<()> {
    q.root(ticket)?;
    q.run("UPDATE approvals SET decision='expired' WHERE ticket=? AND decision='pending'", &[&ticket])?;
    for job in q.all(&format!("SELECT * FROM jobs WHERE ticket=? AND state NOT IN {END_SQL}"), &[&ticket])? {
        q.run("UPDATE jobs SET state='cancelled' WHERE id=?", &[&int_of(&job, "id")])?;
        q.withdraw_question(who_of(&job))?;
        q.log(who_of(&job), "cancelled", &json!("用户停止了工单"))?;
    }
    Ok(())
}

/// Early wake of the parent after a child's first failed attempt (spec §9.2
/// F). `source=NULL` keeps it outside the 128-note budget.
fn platform_note(q: &Q, job: &JobCtx, target: i64, body: &str) -> Result<i64> {
    let seq=q.log(job.who(),"note",&json!({"job":target,"sender":"platform","body":body,"sha256":digest(body)}))?;
    q.run("INSERT INTO mailbox(seq,target,source,source_turn,msgkey) VALUES(?,?,NULL,NULL,NULL)",&[&seq,&target])?;
    Ok(seq)
}

fn notify_parent(q: &Q, job: &JobCtx, class: ErrorClass, attempt: u32, until: f64) -> Result<()> {
    notify_parent_as(q, job, class_name(class), attempt, until)
}

fn notify_parent_as(q: &Q, job: &JobCtx, class: Value, attempt: u32, until: f64) -> Result<()> {
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
        "class": class,
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
/// errors, expiring correction hints and the first internal failure.
#[derive(Default)]
struct Stream {
    pending: String,
    invalid: Vec<(String, String)>,
    fatal: Option<anyhow::Error>,
    correction_notes: Vec<i64>,
    detector: crate::stuck::Detector,
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
    // Live activity shows the wait; the guard ends it on every exit path.
    struct Waiting<'a>(&'a Turn);
    impl Drop for Waiting<'_> {
        fn drop(&mut self) {
            lock(&self.0.activity).approval(false, now());
        }
    }
    lock(&turn.activity).approval(true, now());
    let _waiting = Waiting(&turn);
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

/// When the Claude lead's prompt cache was last read or written, for how long
/// it stays warm, and how many keep-alives this wait has used.
#[derive(Clone, Copy, Debug)]
struct CacheClock {
    last: f64,
    ttl: f64,
    pings: u32,
    running: bool,
}

/// Keep-alives per wait. One refresh reads the prefix (0.1x input price);
/// an expired cache is rewritten (1.25x for 5 minutes, 2x for 1 hour), so
/// ten refreshes stay well below one rewrite of a 1-hour cache.
const KEEPALIVE_CAP: u32 = 10;
const KEEPALIVE_PROMPT: &str = "Super Agent prompt-cache keep-alive. Reply with the single word OK. Do not use any tools.";

impl Inner {
    /// Track main-loop Claude requests: `message_start.usage` says whether
    /// the request wrote 1-hour or 5-minute cache entries.
    fn observe_cache(&self, job: i64, data: &Value) {
        if data["type"] != "stream_event" || data["event"]["type"] != "message_start" || !data["parent_tool_use_id"].is_null() {
            return;
        }
        let usage = &data["event"]["message"]["usage"];
        let one_hour = usage["cache_creation"]["ephemeral_1h_input_tokens"].as_u64().unwrap_or(0) > 0;
        let five_min = usage["cache_creation"]["ephemeral_5m_input_tokens"].as_u64().unwrap_or(0) > 0;
        let mut clocks = lock(&self.cache_clocks);
        let clock = clocks.entry(job).or_insert(CacheClock { last: 0.0, ttl: 0.0, pings: 0, running: false });
        clock.last = now();
        clock.pings = 0;
        if one_hour {
            clock.ttl = 3600.0;
        } else if five_min {
            clock.ttl = 300.0;
        }
    }

    /// Start keep-alives for waiting Claude jobs whose cache is about to
    /// expire. Called from the scheduler timer; never blocks it.
    fn keepalive_pass(self: &Arc<Self>) {
        if self.scheduler_database_busy.load(Ordering::SeqCst) {
            return;
        }
        if self.stopping.load(Ordering::SeqCst)
            || self.scheduler_state.load(Ordering::SeqCst) != SCHEDULER_RUNNING
        {
            return;
        }
        if !self.opts.efficiency.read().unwrap_or_else(|e| e.into_inner()).claude_keepalive {
            return;
        }
        let at = now();
        let due: Vec<i64> = lock(&self.cache_clocks)
            .iter()
            .filter(|(_, c)| {
                let margin = if c.ttl >= 3600.0 { 300.0 } else { 60.0 };
                !c.running && c.ttl > 0.0 && c.pings < KEEPALIVE_CAP && at - c.last >= c.ttl - margin
            })
            .map(|(job, _)| *job)
            .collect();
        for job in due {
            let Some(adapter) = self.adapters.get("claude").cloned() else { return };
            let request = match self.db.with(|q| keepalive_request(q, job)) {
                Ok(Some(request)) => request,
                // Not waiting any more (or not eligible): forget the clock.
                _ => {
                    lock(&self.cache_clocks).remove(&job);
                    continue;
                }
            };
            if let Some(c) = lock(&self.cache_clocks).get_mut(&job) {
                c.running = true;
            }
            let inner = self.clone();
            tokio::spawn(async move { inner.keepalive(adapter, request).await });
        }
    }

    async fn keepalive(self: Arc<Self>, adapter: Arc<dyn Adapter>, mut request: RunRequest) {
        if self.stopping.load(Ordering::SeqCst)
            || self.scheduler_state.load(Ordering::SeqCst) != SCHEDULER_RUNNING
        {
            return;
        }
        let job = request.job;
        let who = Who { ticket: request.ticket, id: job, turn: request.turn };
        // The bus contributes tool definitions; the fork must list the same tools.
        request.bus = self.opts.bus.as_ref().and_then(|b| b.grant(request.ticket, job, request.turn, "claude"));
        let _bus = BusLease(self.opts.bus.clone(), request.bus.as_ref().map(|g| g.token.clone()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let started = std::time::Instant::now();
        let run = adapter.keepalive(request, Arc::new(AdviceHost), tx, cancel.clone());
        let usage = tokio::spawn(async move {
            let (mut read, mut write) = (0u64, 0u64);
            while let Some(ev) = rx.recv().await {
                if let AdapterEvent::Journal { kind, data } = ev
                    && kind == "native"
                    && data["type"] == "stream_event"
                    && data["event"]["type"] == "message_start"
                    && data["parent_tool_use_id"].is_null()
                {
                    let u = &data["event"]["message"]["usage"];
                    read += u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                    write += u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                }
            }
            (read, write)
        });
        let outcome = match run {
            None => Err("执行端不支持缓存保温".to_string()),
            Some(run) => tokio::select! {
                r = run => r.map(|_| ()).map_err(|e| e.message),
                () = tokio::time::sleep(Duration::from_secs(180)) => { cancel.cancel(); Err("保温请求超时".into()) }
            },
        };
        let (read, write) = usage.await.unwrap_or((0, 0));
        let seconds = (started.elapsed().as_secs_f64() * 10.0).round() / 10.0;
        let payload = match &outcome {
            Ok(()) => json!({"ok": true, "cache_read": read, "cache_write": write, "seconds": seconds}),
            Err(e) => json!({"ok": false, "error": e, "cache_read": read, "cache_write": write, "seconds": seconds}),
        };
        let _ = self.db.with(|q| q.log(who, "cache.keepalive", &payload));
        let mut clocks = lock(&self.cache_clocks);
        if let Some(c) = clocks.get_mut(&job) {
            c.running = false;
            c.pings += 1;
            if outcome.is_ok() && read > 0 {
                c.last = now();
            } else {
                // A failed or cold refresh will not keep anything warm: stop.
                c.pings = KEEPALIVE_CAP;
            }
        }
    }
}

/// The request a keep-alive must send: the job's own session, route, working
/// directory and permission, so the prefix equals its next real turn's.
fn keepalive_request(q: &Q, job: i64) -> Result<Option<RunRequest>> {
    let Some(row) = q.one(
        "SELECT j.*,t.cwd AS ticket_cwd,coalesce(j.permission_override,t.permission_profile) AS profile,r.route AS bound
         FROM jobs j JOIN tickets t ON t.id=j.ticket LEFT JOIN native_routes r ON r.job=j.id
         WHERE j.id=? AND j.agent='claude' AND j.state='waiting' AND coalesce(j.session,'')!=''
         AND NOT EXISTS(SELECT 1 FROM ticket_pause p WHERE p.ticket=j.ticket)",
        &[&job],
    )?
    else {
        return Ok(None);
    };
    let profile = str_of(&row, "profile").to_string();
    if !matches!(profile.as_str(), "full" | "auto") {
        return Ok(None);
    }
    let Ok(route) = p::route(Some(&parse(row.get("bound").and_then(Value::as_str)))) else {
        return Ok(None);
    };
    let cwd = if let Some(w) = crate::workspaces::get(q, job)? {
        w.path
    } else if let Some(d) = q.one("SELECT cwd FROM job_directories WHERE job=?", &[&job])? {
        str_of(&d, "cwd").to_string()
    } else {
        str_of(&row, "ticket_cwd").to_string()
    };
    let writing = int_of(&row, "writing") != 0;
    Ok(Some(RunRequest {
        ticket: int_of(&row, "ticket"),
        job,
        turn: int_of(&row, "turn"),
        agent: "claude".into(),
        prompt: KEEPALIVE_PROMPT.into(),
        images: vec![],
        cwd: PathBuf::from(cwd),
        route,
        writable: p::is_full(&profile) || writing,
        profile,
        session_id: Some(str_of(&row, "session").to_string()),
        bus: None,
    }))
}

/// Advisor turns run read-only: no approvals, no notes.
struct AdviceHost;

impl TurnHost for AdviceHost {
    fn approve(&self, _action: Value) -> BoxFuture<'static, bool> {
        Box::pin(async { false })
    }
    fn pending_notes(&self) -> Vec<Value> {
        vec![]
    }
    fn accept_notes(&self, _seqs: &[i64]) {}
}

/// Upper bound of one advisor turn; bus clients wait at most this long.
const ADVICE_LIMIT: Duration = Duration::from_secs(14 * 60);

#[cfg(test)]
#[path = "engine/advice_usage_tests.rs"]
mod advice_usage_tests;

impl Engine {
    /// The advisor a ticket would consult now (override, default or lead).
    pub fn advisor_for(&self, ticket: i64) -> Result<crate::advice::Advisor> {
        self.inner.db.with(|q| crate::advice::choose(q, ticket))
    }

    /// Stored advisor of a scope: a ticket, or the global default (`None`).
    pub fn advisor_setting(&self, ticket: Option<i64>) -> Result<Option<(String, Route)>> {
        self.inner.db.with(|q| crate::advice::get(q, ticket))
    }

    /// Store (or clear with `None`) the advisor of a scope.
    pub fn set_advisor(&self, ticket: Option<i64>, choice: Option<(&str, &Route)>) -> Result<()> {
        if let Some((agent, _)) = choice {
            ensure!(self.inner.adapters.contains_key(agent), "未知的顾问执行端：{agent}");
        }
        self.inner.db.tx(|q| crate::advice::set(q, ticket, choice))
    }

    /// One read-only advisor turn for a running task. Logged on the caller's
    /// job as `advice.started` / `advice.result`.
    pub async fn consult(&self, ticket: i64, job: i64, turn: i64, brief: &str, cancel: CancellationToken) -> Result<Value> {
        self.consult_limited(ticket, job, turn, brief, cancel, ADVICE_LIMIT)
            .await
    }

    async fn consult_limited(
        &self,
        ticket: i64,
        job: i64,
        turn: i64,
        brief: &str,
        cancel: CancellationToken,
        limit: Duration,
    ) -> Result<Value> {
        let inner = &self.inner;
        let cancel = cancel.child_token();
        ensure!(!brief.trim().is_empty() && brief.len() <= crate::advice::MAX_BRIEF, "简报为空或超过 {} 字节", crate::advice::MAX_BRIEF);
        let who = Who { ticket, id: job, turn };
        let (advisor, cwd, requirements) = inner.db.tx(|q| {
            ensure!(q.live(who)?, "本轮已结束");
            let cwd = q.one("SELECT cwd FROM tickets WHERE id=?", &[&ticket])?.ok_or_else(|| anyhow!("工单不存在"))?;
            Ok((crate::advice::choose(q, ticket)?, str_of(&cwd, "cwd").to_string(), crate::context_selection::advisor_section(q, ticket)?))
        })?;
        let adapter = inner.adapters.get(&advisor.agent).cloned().ok_or_else(|| anyhow!("顾问执行端不可用：{}", advisor.agent))?;
        if inner.opts.efficiency.read().unwrap_or_else(|e| e.into_inner()).quota_guard
            && let QuotaStatus::Exhausted { until, .. } =
                inner.db.with(|q| quota_status(q, &advisor.agent, &advisor.route.provider, now(), None))?
        {
            bail!("顾问所在供应商的订阅额度窗口已用尽，预计 {} 后恢复；请自行判断或向上级求助", classify::utc_minute(until as i64));
        }
        let label = json!({"agent": advisor.agent, "provider": advisor.route.provider, "model": advisor.route.model,
            "reasoning_effort": advisor.route.reasoning_effort, "source": advisor.source});
        // Journal sequence is durable and unique across concurrent callers and restarts.
        let (section, sources, omitted) = match requirements {
            Some((text, sources, omitted)) => (Some(text), sources, omitted),
            None => (None, vec![], 0),
        };
        let started_seq = inner.db.with(|q| {
            let seq = q.log(who, "advice.started", &json!({"schema_version":1, "advisor": label, "brief_bytes": brief.len(), "brief_sha256": digest(brief)}))?;
            q.log(who, "context.selection", &crate::context_selection::advisor_manifest(sources, omitted, brief.len()))?;
            Ok(seq)
        })?;
        let attempt_id = format!("advice:{started_seq}");
        let prompt = match &section {
            Some(section) => format!("{}\n\n{section}\n\n{brief}", crate::advice::GUIDE),
            None => format!("{}\n\n{brief}", crate::advice::GUIDE),
        };
        let request = RunRequest {
            ticket,
            job,
            turn,
            agent: advisor.agent.clone(),
            prompt,
            images: vec![],
            cwd: PathBuf::from(cwd),
            route: advisor.route.clone(),
            profile: "readonly".into(),
            writable: false,
            session_id: None,
            bus: None,
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let record_usage = |event| -> Result<()> {
            if let AdapterEvent::Journal { kind, data } = event
                && kind == "native"
                && let Some(sample) = sa_store::usage::advisor_usage_sample(&advisor.agent, &data)
            {
                inner.db.with(|q| q.log(who, "usage.observed", &json!({
                    "schema_version":1, "role":"advisor", "attempt_id":attempt_id,
                    "agent":advisor.agent, "sample":sample
                })))?;
            }
            Ok(())
        };
        let mut usage_lost = false;
        let started = std::time::Instant::now();
        let outcome = {
            let run = adapter.run(request, Arc::new(AdviceHost), tx, cancel.clone());
            let timeout = tokio::time::sleep(limit);
            tokio::pin!(run, timeout);
            loop {
                tokio::select! {
                    r = &mut run => break r.map_err(|e| anyhow!(e.message)),
                    event = rx.recv(), if !rx.is_closed() || !rx.is_empty() => {
                        if let Some(event) = event {
                            usage_lost |= record_usage(event).is_err();
                        }
                    }
                    () = &mut timeout => {
                        cancel.cancel();
                        break Err(anyhow!("顾问超过 {} 分钟未完成", ADVICE_LIMIT.as_secs() / 60));
                    }
                    () = cancel.cancelled() => break Err(anyhow!("顾问已取消")),
                }
            }
        };
        // Completion can win select while a final usage frame is already queued.
        // Preserve those samples on success, failure, cancellation and timeout.
        rx.close();
        while let Ok(event) = rx.try_recv() {
            usage_lost |= record_usage(event).is_err();
        }
        if outcome.is_err() {
            cancel.cancel();
        }
        let seconds = (started.elapsed().as_secs_f64() * 10.0).round() / 10.0;
        let logged = match &outcome {
            Ok(o) => json!({"schema_version":1, "attempt_id":attempt_id, "usage_complete":!usage_lost, "state": "done", "seconds": seconds, "advisor": label, "advice": o.text}),
            Err(e) => json!({"schema_version":1, "attempt_id":attempt_id, "usage_complete":!usage_lost, "state": "error", "seconds": seconds, "advisor": label, "error": e.to_string()}),
        };
        // Unlike optional metering, durable delivery of the advice is required.
        // Do not report success when its final receipt cannot be persisted;
        // the existing SQLite busy timeout applies, and we never re-run the
        // model merely to retry a failed journal write.
        if let Err(error) = inner.db.with(|q| q.log(who, "advice.result", &logged)) {
            cancel.cancel();
            return Err(error);
        }
        let o = outcome?;
        Ok(json!({"attempt_id":attempt_id, "usage_complete":!usage_lost, "advisor": label, "seconds": seconds, "advice": o.text}))
    }
}

impl crate::services::Consultant for Engine {
    fn consult(&self, ticket: i64, job: i64, turn: i64, brief: String, cancel: CancellationToken) -> BoxFuture<'static, Result<Value>> {
        let engine = self.clone();
        Box::pin(async move { engine.consult(ticket, job, turn, &brief, cancel).await })
    }
}

/// Revokes a turn's bus token on every exit path.
struct BusLease(Option<Arc<dyn crate::services::Bus>>, Option<String>);

impl Drop for BusLease {
    fn drop(&mut self) {
        if let (Some(bus), Some(token)) = (&self.0, &self.1) {
            bus.revoke(token);
        }
    }
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
            // A bus `reply` reaches its asker through `ask` (or `inbox` after a
            // timeout); steering it too would deliver the answer twice.
            .filter(|n| !n["id"].as_str().is_some_and(|id| id.starts_with("bus-r-")))
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

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use crate::adapter::{Catalog, EventSink, RunOutcome};

    struct ProbeAdapter {
        selected: bool,
        calls: AtomicUsize,
    }

    impl Adapter for ProbeAdapter {
        fn agent(&self) -> &'static str {
            "antigravity"
        }

        fn catalog(&self) -> BoxFuture<'static, Result<Catalog>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(Catalog {
                    value: json!({"full":true}),
                })
            })
        }

        fn catalog_for_route(&self, route: Route) -> Option<BoxFuture<'static, Result<Catalog>>> {
            if !self.selected {
                return None;
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            Some(Box::pin(async move {
                Ok(Catalog {
                    value: route_json(&route),
                })
            }))
        }

        fn run(
            &self,
            _: RunRequest,
            _: Arc<dyn TurnHost>,
            _: EventSink,
            _: CancellationToken,
        ) -> BoxFuture<'static, Result<RunOutcome, AdapterError>> {
            panic!("catalog test must not execute models")
        }
    }

    #[tokio::test]
    async fn selected_catalog_shares_inflight_and_preserves_full_fallback() {
        for selected in [false, true] {
            let data = tempfile::tempdir().unwrap();
            let adapter = Arc::new(ProbeAdapter {
                selected,
                calls: AtomicUsize::new(0),
            });
            let engine =
                Engine::open(data.path(), vec![adapter.clone()], EngineOptions::default()).unwrap();
            let route = Route {
                provider: "google".into(),
                model: "model-a".into(),
                mode: "cli".into(),
                reasoning_effort: Some("low".into()),
            };
            let first = engine
                .inner
                .catalog_request("antigravity", Some(route.clone()));
            let second = engine
                .inner
                .catalog_request("antigravity", Some(route.clone()));
            let mut other = route.clone();
            other.reasoning_effort = Some("high".into());
            let third = engine.inner.catalog_request("antigravity", Some(other));
            assert_eq!(
                adapter.calls.load(Ordering::SeqCst),
                if selected { 2 } else { 1 }
            );
            let (first, second, third) = tokio::join!(first, second, third);
            assert_eq!(first.as_ref().unwrap(), second.as_ref().unwrap());
            if selected {
                assert_ne!(first.unwrap(), third.unwrap());
            }
            engine
                .inner
                .catalog_request("antigravity", Some(route))
                .await
                .unwrap();
            assert_eq!(
                adapter.calls.load(Ordering::SeqCst),
                if selected { 3 } else { 2 }
            );
            engine.close().await;
        }
    }
}

#[cfg(test)]
mod keepalive_tests {
    use super::*;
    use crate::adapter::{Catalog, EventSink, RunOutcome};

    /// Fake Claude that records keep-alive requests and reports a cache read.
    #[derive(Default)]
    struct WarmClaude {
        seen: Mutex<Vec<RunRequest>>,
    }

    impl Adapter for WarmClaude {
        fn agent(&self) -> &'static str {
            "claude"
        }
        fn catalog(&self) -> BoxFuture<'static, Result<Catalog>> {
            Box::pin(async { Ok(Catalog { value: json!({}) }) })
        }
        fn run(&self, _: RunRequest, _: Arc<dyn TurnHost>, _: EventSink, _: CancellationToken) -> BoxFuture<'static, Result<RunOutcome, AdapterError>> {
            Box::pin(async { Err(AdapterError::new(ErrorClass::Other, "unused")) })
        }
        fn keepalive(&self, request: RunRequest, _: Arc<dyn TurnHost>, events: EventSink, _: CancellationToken) -> Option<BoxFuture<'static, Result<RunOutcome, AdapterError>>> {
            lock(&self.seen).push(request);
            Some(Box::pin(async move {
                let _ = events.send(AdapterEvent::Journal {
                    kind: "native".into(),
                    data: json!({"type":"stream_event","parent_tool_use_id":null,"event":{"type":"message_start","message":{"usage":{"input_tokens":3,"cache_read_input_tokens":350000,"cache_creation_input_tokens":12}}}}),
                });
                Ok(RunOutcome { text: "OK".into(), session_id: None })
            }))
        }
    }

    fn engine(dir: &Path) -> (Engine, Arc<WarmClaude>) {
        let claude = Arc::new(WarmClaude::default());
        let opts = EngineOptions { tick: Duration::from_secs(3600), ..Default::default() };
        let engine = Engine::open(dir, vec![claude.clone() as Arc<dyn Adapter>], opts).unwrap();
        engine.inner.scheduler_state.store(SCHEDULER_RUNNING, Ordering::SeqCst);
        engine
            .db()
            .with(|q| {
                q.conn().execute_batch(
                    "INSERT INTO tickets(id,goal,cwd,writes,lead,created,permission_profile) VALUES(1,'g','C:/p',0,'claude',0,'full');
                     INSERT INTO jobs(id,ticket,parent,agent,body,writing,depth,turn,state,session) VALUES(1,1,NULL,'claude','b',0,0,4,'waiting','sess-1');
                     INSERT INTO native_routes VALUES(1,'{\"provider\":\"p\",\"model\":\"claude-opus-5-5\",\"mode\":\"cli\"}');",
                )?;
                Ok(())
            })
            .unwrap();
        (engine, claude)
    }

    fn start_event(one_hour: u64, five_min: u64) -> Value {
        json!({"type":"stream_event","parent_tool_use_id":null,"event":{"type":"message_start","message":{"usage":{
            "cache_creation":{"ephemeral_1h_input_tokens":one_hour,"ephemeral_5m_input_tokens":five_min}}}}})
    }

    fn age(engine: &Engine, job: i64, seconds: f64) {
        lock(&engine.inner.cache_clocks).get_mut(&job).unwrap().last = now() - seconds;
    }

    async fn settle(engine: &Engine) {
        for _ in 0..200 {
            if lock(&engine.inner.cache_clocks).values().all(|c| !c.running) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("keep-alive did not finish");
    }

    #[tokio::test]
    async fn refreshes_a_waiting_lead_before_its_cache_expires() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, claude) = engine(dir.path());
        engine.inner.observe_cache(1, &start_event(900, 0));
        assert_eq!(lock(&engine.inner.cache_clocks)[&1].ttl, 3600.0);
        age(&engine, 1, 3000.0);
        engine.inner.keepalive_pass();
        settle(&engine).await;
        assert!(lock(&claude.seen).is_empty(), "not due before the 55-minute margin");
        age(&engine, 1, 3350.0);
        engine.inner.keepalive_pass();
        settle(&engine).await;
        let seen = lock(&claude.seen).clone();
        assert_eq!(seen.len(), 1);
        let r = &seen[0];
        assert_eq!((r.session_id.as_deref(), r.profile.as_str(), r.turn), (Some("sess-1"), "full", 4));
        assert_eq!(r.route.model, "claude-opus-5-5");
        assert_eq!(r.cwd, PathBuf::from("C:/p"));
        assert_eq!(r.prompt, KEEPALIVE_PROMPT);
        let clock = lock(&engine.inner.cache_clocks)[&1];
        assert_eq!(clock.pings, 1);
        assert!(now() - clock.last < 5.0, "a warm read restarts the clock");
        let logged = engine.db().with(|q| q.one("SELECT payload FROM events WHERE kind='cache.keepalive'", &[])).unwrap().unwrap();
        assert!(str_of(&logged, "payload").contains("\"cache_read\":350000"));
        // A real request resets the count; five-minute caches use a 4-minute margin.
        engine.inner.observe_cache(1, &start_event(0, 50));
        let clock = lock(&engine.inner.cache_clocks)[&1];
        assert_eq!((clock.pings, clock.ttl), (0, 300.0));
    }

    #[tokio::test]
    async fn skips_paused_read_only_capped_and_disabled_waits() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, claude) = engine(dir.path());
        let due = |engine: &Engine| {
            engine.inner.observe_cache(1, &start_event(10, 0));
            age(engine, 1, 3500.0);
            engine.inner.keepalive_pass();
        };
        engine.inner.scheduler_state.store(SCHEDULER_FAILED, Ordering::SeqCst);
        due(&engine);
        settle(&engine).await;
        assert!(lock(&claude.seen).is_empty(), "failed scheduler must not keep a cache warm");
        engine.inner.scheduler_state.store(SCHEDULER_RUNNING, Ordering::SeqCst);
        engine.inner.opts.efficiency.write().unwrap().claude_keepalive = false;
        due(&engine);
        settle(&engine).await;
        engine.inner.opts.efficiency.write().unwrap().claude_keepalive = true;
        lock(&engine.inner.cache_clocks).get_mut(&1).unwrap().pings = KEEPALIVE_CAP;
        engine.inner.keepalive_pass();
        settle(&engine).await;
        assert!(lock(&claude.seen).is_empty(), "disabled and capped waits are not refreshed");
        engine.db().with(|q| q.run("INSERT INTO ticket_pause(ticket,state) VALUES(1,'paused')", &[]).map(|_| ())).unwrap();
        due(&engine);
        settle(&engine).await;
        assert!(!lock(&engine.inner.cache_clocks).contains_key(&1), "a paused ticket forgets the clock");
        engine.db().with(|q| q.run("DELETE FROM ticket_pause", &[]).map(|_| ())).unwrap();
        engine.db().with(|q| q.run("UPDATE tickets SET permission_profile='readonly'", &[]).map(|_| ())).unwrap();
        due(&engine);
        settle(&engine).await;
        engine.db().with(|q| q.run("UPDATE tickets SET permission_profile='full'", &[]).map(|_| ())).unwrap();
        engine.db().with(|q| q.run("UPDATE jobs SET state='running'", &[]).map(|_| ())).unwrap();
        due(&engine);
        settle(&engine).await;
        assert!(lock(&claude.seen).is_empty(), "read-only and running jobs are not refreshed");
    }
}

#[cfg(test)]
#[path = "engine/scheduler_tests.rs"]
mod scheduler_tests;
