//! Scheduler host: owns `desktop.lock`, a tokio runtime and the engine, and
//! executes operator actions off the UI thread (the Rust replacement for
//! `runtime/broker.cjs` plus the write half of `runtime/studio.cjs`).
//!
//! When another workbench (Rust or Electron) already holds the journal,
//! [`Host::start`] fails with [`owner::BUSY`] and the window stays a
//! read-only observer.

use anyhow::{Context, Result, anyhow, bail};
use sa_adapters::sys::Sys;
use sa_engine::db::who_of;
use sa_engine::{CreateParams, Engine, EngineOptions, owner};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use sa_engine::adapter::{AGENTS, Adapter};
use std::collections::HashMap;
use sa_adapters::Registry;
use sa_engine::adapter::Route;
use sa_engine::runner::RotationSource;
use sa_store::providers::{self, CustomProvider, Rotation};
use std::sync::{Arc, RwLock, mpsc};
use std::thread::JoinHandle;

/// One operator action. Mirrors the broker's `engineOps` and the studio
/// plan operations.
#[derive(Clone, Debug)]
pub enum Action {
    Create {
        goal: String,
        /// `None` creates a fresh temporary project like `studio.create`.
        cwd: Option<PathBuf>,
        lead: String,
        profile: String,
        files: Vec<String>,
        /// Per-agent routes; `None` follows `route_defaults`.
        selections: Option<Value>,
        /// `{skills, servers}` ids; `None` follows the extension defaults.
        extensions: Option<Value>,
    },
    Follow {
        ticket: i64,
        body: String,
        resume: bool,
    },
    Note {
        ticket: i64,
        job: i64,
        body: String,
    },
    Cancel {
        ticket: i64,
    },
    Retry {
        job: i64,
    },
    Decide {
        id: i64,
        allow: bool,
    },
    PlanAdd {
        ticket: i64,
        body: String,
    },
    PlanToggle {
        ticket: i64,
        id: i64,
    },
    /// `saveDefaults` (ST:234-238): routes for new conversations.
    SaveDefaults {
        selections: Value,
    },
    /// `saveRoutes` (ST:239-290): routes of a stopped ticket.
    SaveRoutes {
        ticket: i64,
        selections: Value,
    },
    /// Operator entry in the project knowledge base.
    KnowledgeAdd {
        cwd: String,
        kind: String,
        body: String,
        ticket: Option<i64>,
    },
    KnowledgeArchive {
        id: i64,
        archived: bool,
    },
    /// Add or update an operator-managed provider (empty id adds).
    ProviderSave {
        provider: CustomProvider,
    },
    ProviderRemove {
        id: String,
    },
    RotationSave {
        rotation: Rotation,
    },
    /// Sidebar metadata; `None` fields stay unchanged, an empty title clears it.
    TicketMeta {
        ticket: i64,
        title: Option<String>,
        pinned: Option<bool>,
        archived: Option<bool>,
    },
}

impl Action {
    pub fn kind(&self) -> ActionKind {
        match self {
            Action::Create { .. } => ActionKind::Create,
            Action::Follow { .. } => ActionKind::Follow,
            Action::Note { .. } => ActionKind::Note,
            Action::Cancel { .. } => ActionKind::Cancel,
            Action::Retry { .. } => ActionKind::Retry,
            Action::Decide { allow, .. } => ActionKind::Decide { allow: *allow },
            Action::PlanAdd { .. } | Action::PlanToggle { .. } => ActionKind::Plan,
            Action::SaveDefaults { .. } | Action::SaveRoutes { .. } => ActionKind::Routes,
            Action::KnowledgeAdd { .. } | Action::KnowledgeArchive { .. } => ActionKind::Knowledge,
            Action::ProviderSave { .. } | Action::ProviderRemove { .. } | Action::RotationSave { .. } => {
                ActionKind::Providers
            }
            Action::TicketMeta { .. } => ActionKind::Meta,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    Create,
    Follow,
    Note,
    Cancel,
    Retry,
    Decide { allow: bool },
    Plan,
    Routes,
    Knowledge,
    Providers,
    Meta,
}

/// Result of one action; `value` is the new ticket id for `Create`.
#[derive(Debug)]
pub struct Outcome {
    pub kind: ActionKind,
    pub result: Result<Option<i64>, String>,
}

/// A harness catalog probe answered off the action queue.
#[derive(Debug)]
pub struct CatalogReply {
    pub agent: String,
    pub result: Result<Value, String>,
}

type Notify = Arc<dyn Fn() + Send + Sync>;

/// Rotation candidates from the operator's saved order (empty when off).
/// The host refreshes the cache after every provider or rotation edit.
pub struct DbRotation(pub Arc<RwLock<Rotation>>);

impl RotationSource for DbRotation {
    fn candidates(&self, agent: &str) -> Vec<Route> {
        let r = self.0.read().unwrap_or_else(|p| p.into_inner());
        if !r.enabled {
            return vec![];
        }
        r.order
            .get(agent)
            .into_iter()
            .flatten()
            .map(|x| Route {
                provider: x.provider.clone(),
                model: x.model.clone(),
                mode: x.mode.clone(),
            })
            .collect()
    }
}

/// Provider registry and rotation shared by the adapters, the engine and the UI.
#[derive(Clone)]
pub struct Shared {
    pub registry: Registry,
    pub rotation: Arc<RwLock<Rotation>>,
}

impl Shared {
    fn new() -> Self {
        Self {
            registry: sa_adapters::registry(vec![]),
            rotation: Arc::default(),
        }
    }

    /// Reload both caches from the journal.
    fn reload(&self, engine: &Engine) -> Result<()> {
        let (list, rotation) = engine.db().with(|q| {
            providers::ensure_schema(q.conn())?;
            Ok((providers::list(q.conn())?, providers::rotation(q.conn())?))
        })?;
        *self.registry.write().unwrap_or_else(|p| p.into_inner()) = list;
        *self.rotation.write().unwrap_or_else(|p| p.into_inner()) = rotation;
        Ok(())
    }
}

pub struct Host {
    tx: Option<tokio::sync::mpsc::UnboundedSender<Action>>,
    pub rx: mpsc::Receiver<Outcome>,
    engine: Engine,
    thread: Option<JoinHandle<()>>,
    runtime: tokio::runtime::Handle,
    adapters: Arc<HashMap<String, Arc<dyn Adapter>>>,
    catalog_tx: mpsc::Sender<CatalogReply>,
    pub catalogs: mpsc::Receiver<CatalogReply>,
    probe_tx: mpsc::Sender<Result<Vec<String>, String>>,
    /// Answers of [`Host::probe_models`].
    pub probes: mpsc::Receiver<Result<Vec<String>, String>>,
    shared: Shared,
    ext: sa_ext::ProjectExtensions,
    notify: Notify,
}

impl Host {
    /// Claim `dir`, open the journal and start scheduling. `notify` is called
    /// after every outcome (the UI repaints).
    pub fn start(dir: PathBuf, notify: impl Fn() + Send + Sync + 'static) -> Result<Self> {
        Self::start_with(dir, |shared| sa_adapters::all_with(Sys::from_process(), shared.registry.clone()), notify)
    }

    /// [`start`](Self::start) with explicit harness adapters (tests).
    pub fn start_with(
        dir: PathBuf,
        adapters: impl FnOnce(&Shared) -> Vec<Arc<dyn Adapter>> + Send + 'static,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        let shared = Shared::new();
        let for_thread = shared.clone();
        type Ready = (Engine, tokio::runtime::Handle, Vec<Arc<dyn Adapter>>, sa_ext::ProjectExtensions);
        let notify: Notify = Arc::new(notify);
        let wake = notify.clone();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<Ready, String>>();
        let (out_tx, out_rx) = mpsc::channel();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Action>();
        let thread = std::thread::Builder::new()
            .name("sa-desktop-scheduler".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(4)
                    .thread_name("sa-engine")
                    .enable_all()
                    .build()
                {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("调度运行时启动失败：{e}")));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let _owner = match owner::claim(&dir) {
                        Ok(o) => o,
                        Err(e) => {
                            let _ = ready_tx.send(Err(e.to_string()));
                            return;
                        }
                    };
                    let adapters = adapters(&for_thread);
                    let ext = match sa_ext::ProjectExtensions::open(&dir) {
                        Ok(x) => x,
                        Err(e) => {
                            let _ = ready_tx.send(Err(format!("MCP 与技能服务无法打开：{e:#}")));
                            return;
                        }
                    };
                    let engine = match open_engine(&dir, adapters.clone(), &for_thread, &ext) {
                        Ok(e) => e,
                        Err(e) => {
                            let _ = ready_tx.send(Err(format!("{e:#}")));
                            return;
                        }
                    };
                    let handle = tokio::runtime::Handle::current();
                    let _ = ready_tx.send(Ok((engine.clone(), handle, adapters, ext.clone())));
                    while let Some(action) = rx.recv().await {
                        let kind = action.kind();
                        // Create awaits attachment snapshots; run every action in
                        // order so the UI sees outcomes in submission order.
                        let result = execute(&engine, &for_thread, action).await.map_err(|e| format!("{e:#}"));
                        let _ = out_tx.send(Outcome { kind, result });
                        wake();
                    }
                    engine.close().await;
                    ext.close();
                    // `_owner` drops here: the lock is released after the
                    // journal is closed.
                });
            })
            .context("无法启动调度线程")?;
        match ready_rx.recv() {
            Ok(Ok((engine, runtime, adapters, ext))) => {
                let (catalog_tx, catalogs) = mpsc::channel();
                let (probe_tx, probes) = mpsc::channel();
                Ok(Self {
                    probe_tx,
                    probes,
                    shared,
                    ext,
                    tx: Some(tx),
                    rx: out_rx,
                    engine,
                    thread: Some(thread),
                    runtime,
                    adapters: Arc::new(adapters.into_iter().map(|a| (a.agent().to_string(), a)).collect()),
                    catalog_tx,
                    catalogs,
                    notify,
                })
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(anyhow!(e))
            }
            Err(_) => {
                let _ = thread.join();
                bail!("调度线程意外退出")
            }
        }
    }

    pub fn submit(&self, action: Action) -> bool {
        self.tx.as_ref().is_some_and(|tx| tx.send(action).is_ok())
    }

    /// Probe one harness catalog concurrently with the action queue (probes
    /// take seconds); the answer arrives on [`catalogs`](Self::catalogs).
    pub fn request_catalog(&self, agent: &str) {
        if self.tx.is_none() {
            return;
        }
        let adapter = self.adapters.get(agent).cloned();
        let (tx, notify, agent) = (self.catalog_tx.clone(), self.notify.clone(), agent.to_string());
        self.runtime.spawn(async move {
            let result = match adapter {
                Some(a) => a.catalog().await.map(|c| c.value).map_err(|e| format!("{e:#}")),
                None => Err("未知执行器".into()),
            };
            let _ = tx.send(CatalogReply { agent, result });
            notify();
        });
    }

    /// Fetch the model list of an unsaved provider (`/v1/models`).
    pub fn probe_models(&self, provider: CustomProvider) {
        if self.tx.is_none() {
            return;
        }
        let (tx, notify) = (self.probe_tx.clone(), self.notify.clone());
        self.runtime.spawn(async move {
            let cancel = tokio_util::sync::CancellationToken::new();
            let r = sa_adapters::probe_models(&provider, &Sys::from_process(), &cancel)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(r);
            notify();
        });
    }

    /// The MCP and skill service (management calls run off the UI thread).
    pub fn ext(&self) -> sa_ext::ProjectExtensions {
        self.ext.clone()
    }

    pub fn runtime(&self) -> tokio::runtime::Handle {
        self.runtime.clone()
    }

    pub fn providers(&self) -> Vec<CustomProvider> {
        self.shared.registry.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn rotation(&self) -> Rotation {
        self.shared.rotation.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Saved routes for new conversations (`route_defaults`).
    pub fn defaults(&self) -> Value {
        self.engine
            .db()
            .with(|q| q.one("SELECT routes FROM route_defaults WHERE singleton=1", &[]))
            .ok()
            .flatten()
            .and_then(|r| r.get("routes").and_then(Value::as_str).and_then(|s| serde_json::from_str(s).ok()))
            .unwrap_or_else(|| json!({}))
    }

    /// Unrecoverable scheduler fault (scheduling stopped).
    pub fn fault(&self) -> Option<String> {
        self.engine.fault()
    }

    #[cfg(test)]
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn active_count(&self) -> usize {
        self.engine.active_count()
    }

    /// Stop scheduling, abort running turns and release the journal.
    pub fn shutdown(&mut self) {
        self.tx.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn open_engine(
    dir: &Path,
    adapters: Vec<Arc<dyn Adapter>>,
    shared: &Shared,
    ext: &sa_ext::ProjectExtensions,
) -> Result<Engine> {
    let opts = EngineOptions {
        rotation: Arc::new(DbRotation(shared.rotation.clone())),
        extensions: Some(Arc::new(ext.clone())),
        ..Default::default()
    };
    let engine = Engine::open(dir, adapters, opts)?;
    engine
        .db()
        .with(|q| {
            q.conn().execute_batch(sa_store::STUDIO_PLAN)?;
            Ok(q.conn().execute_batch(sa_store::TICKET_META)?)
        })?;
    // Providers and rotation must be loaded before the first admission.
    shared.reload(&engine)?;
    engine.start()?;
    Ok(engine)
}

/// `Studio.create` checks (ST:299-317) before `Engine::create`.
async fn create(engine: &Engine, action: Action) -> Result<i64> {
    let Action::Create {
        goal,
        cwd,
        lead,
        profile,
        files,
        selections,
        extensions,
    } = action
    else {
        bail!("不是新建操作");
    };
    if goal.trim().is_empty() {
        bail!("请输入目标");
    }
    if goal.encode_utf16().count() > 200_000 {
        bail!("文本长度或格式无效");
    }
    if !sa_engine::adapter::AGENTS.contains(&lead.as_str()) {
        bail!("请选择主控");
    }
    if !sa_engine::protocol::PROFILES.contains(&profile.as_str()) {
        bail!("无效权限");
    }
    let cwd = match cwd {
        Some(c) => {
            if !c.is_dir() {
                bail!("项目目录不存在");
            }
            c
        }
        None => tempfile::Builder::new()
            .prefix("super-agent-project-")
            .tempdir()?
            .keep(),
    };
    let mut params = CreateParams::new(goal, cwd).lead(&lead).profile(&profile);
    params.files = files;
    params.selections = selections.map(parse_routes).transpose()?;
    params.extensions = extensions;
    engine.create(params).await
}

async fn execute(engine: &Engine, shared: &Shared, action: Action) -> Result<Option<i64>> {
    match action {
        a @ Action::Create { .. } => Ok(Some(create(engine, a).await?)),
        Action::Follow { ticket, body, resume } => {
            engine.follow(ticket, &body, resume)?;
            Ok(None)
        }
        Action::Note { ticket, job, body } => {
            engine.note(ticket, job, &body)?;
            Ok(None)
        }
        Action::Cancel { ticket } => {
            engine.cancel(ticket)?;
            Ok(None)
        }
        Action::Retry { job } => {
            engine.retry(job)?;
            Ok(None)
        }
        Action::Decide { id, allow } => {
            engine.decide(id, allow)?;
            Ok(None)
        }
        Action::PlanAdd { ticket, body } => {
            let body = body.trim().to_string();
            if body.is_empty() || body.encode_utf16().count() > 2000 {
                bail!("计划项不能为空");
            }
            engine.db().tx(|q| {
                let root = q.root(ticket)?;
                let id = q.insert("INSERT INTO studio_plan(ticket,body) VALUES(?,?)", &[&ticket, &body])?;
                q.log(who_of(&root), "plan.add", &json!({"id": id}))?;
                Ok(())
            })?;
            Ok(None)
        }
        Action::PlanToggle { ticket, id } => {
            engine.db().tx(|q| {
                let root = q.root(ticket)?;
                let row = q
                    .one(
                        "UPDATE studio_plan SET done=1-done WHERE ticket=? AND id=? RETURNING done",
                        &[&ticket, &id],
                    )?
                    .ok_or_else(|| anyhow!("计划项不存在"))?;
                let done = row.get("done").and_then(Value::as_i64) == Some(1);
                let kind = if done { "plan.done" } else { "plan.reopen" };
                q.log(who_of(&root), kind, &json!({"id": id}))?;
                Ok(())
            })?;
            Ok(None)
        }
        Action::SaveDefaults { selections } => {
            let routes = parse_routes(selections)?.to_string();
            engine.db().with(|q| {
                q.run("INSERT OR REPLACE INTO route_defaults(singleton,routes) VALUES(1,?)", &[&routes])
            })?;
            Ok(None)
        }
        Action::SaveRoutes { ticket, selections } => {
            save_routes(engine, ticket, parse_routes(selections)?)?;
            Ok(None)
        }
        Action::KnowledgeAdd { cwd, kind, body, ticket } => {
            Ok(Some(engine.add_knowledge(&cwd, &kind, &body, ticket)?))
        }
        Action::KnowledgeArchive { id, archived } => {
            engine.archive_knowledge(id, archived)?;
            Ok(None)
        }
        Action::ProviderSave { provider } => {
            engine.db().with(|q| providers::save(q.conn(), &provider))?;
            shared.reload(engine)?;
            Ok(None)
        }
        Action::ProviderRemove { id } => {
            engine.db().with(|q| providers::remove(q.conn(), &id))?;
            shared.reload(engine)?;
            Ok(None)
        }
        Action::TicketMeta {
            ticket,
            title,
            pinned,
            archived,
        } => {
            let title = title.map(|t| t.trim().chars().take(200).collect::<String>());
            engine.db().tx(|q| {
                if !q.exists("SELECT 1 FROM tickets WHERE id=?", &[&ticket])? {
                    bail!("工单不存在");
                }
                q.run("INSERT OR IGNORE INTO studio_ticket_meta(ticket) VALUES(?)", &[&ticket])?;
                if let Some(t) = &title {
                    let t: Option<&str> = (!t.is_empty()).then_some(t.as_str());
                    q.run("UPDATE studio_ticket_meta SET title=? WHERE ticket=?", &[&t, &ticket])?;
                }
                if let Some(p) = pinned {
                    q.run("UPDATE studio_ticket_meta SET pinned=? WHERE ticket=?", &[&(p as i64), &ticket])?;
                }
                if let Some(a) = archived {
                    q.run("UPDATE studio_ticket_meta SET archived=? WHERE ticket=?", &[&(a as i64), &ticket])?;
                }
                Ok(())
            })?;
            Ok(None)
        }
        Action::RotationSave { rotation } => {
            engine.db().with(|q| providers::save_rotation(q.conn(), &rotation))?;
            shared.reload(engine)?;
            Ok(None)
        }
    }
}

/// `parseRoutes` (ST:57-70): `{agent: {mode, model, provider}}`.
pub fn parse_routes(routes: Value) -> Result<Value> {
    let Some(map) = routes.as_object() else { bail!("路由格式有误") };
    for (agent, row) in map {
        let keys: Option<Vec<&str>> = row.as_object().map(|r| {
            let mut k: Vec<&str> = r.keys().map(String::as_str).collect();
            k.sort_unstable();
            k
        });
        if !AGENTS.contains(&agent.as_str()) || keys.as_deref() != Some(&["mode", "model", "provider"][..]) {
            bail!("路由字段无效");
        }
        let text = |k: &str| {
            row[k]
                .as_str()
                .is_some_and(|v| !v.is_empty() && v.encode_utf16().count() <= 1000)
        };
        if !matches!(row["mode"].as_str(), Some("cli" | "desktop")) || !text("provider") || !text("model") {
            bail!("模型或供应商无效");
        }
    }
    Ok(routes)
}

/// `saveRoutes` (ST:239-290): every job must be terminal; changed agents
/// lose their bound route and native session.
fn save_routes(engine: &Engine, ticket: i64, selections: Value) -> Result<()> {
    engine.db().tx(|q| {
        let row = q
            .one("SELECT agent_routes FROM tickets WHERE id=?", &[&ticket])?
            .ok_or_else(|| anyhow!("工单不存在"))?;
        if q.exists(
            "SELECT 1 FROM jobs WHERE ticket=? AND state NOT IN ('cancelled','failed','partial','done')",
            &[&ticket],
        )? {
            bail!("请先停止或完成工单");
        }
        let old: Value = serde_json::from_str(row["agent_routes"].as_str().unwrap_or("{}"))?;
        let changed: Vec<&str> = AGENTS
            .into_iter()
            .filter(|a| {
                ["provider", "model", "mode"]
                    .iter()
                    .any(|f| old.get(*a).and_then(|r| r.get(f)) != selections.get(*a).and_then(|r| r.get(f)))
            })
            .collect();
        for agent in &changed {
            q.run(
                "DELETE FROM native_routes WHERE job IN (SELECT id FROM jobs WHERE ticket=? AND agent=?)",
                &[&ticket, agent],
            )?;
            q.run(
                "UPDATE jobs SET session=NULL,route_override=NULL WHERE ticket=? AND agent=?",
                &[&ticket, agent],
            )?;
        }
        q.run("UPDATE tickets SET agent_routes=? WHERE id=?", &[&selections.to_string(), &ticket])?;
        if !changed.is_empty() {
            let root = q.root(ticket)?;
            q.log(
                who_of(&root),
                "routes.changed",
                &json!({"old": old, "new": selections, "new_native_session": changed}),
            )?;
        }
        Ok(())
    })
}

/// Relative project paths for attachment picks; files outside `project`
/// are rejected (the engine snapshot re-checks containment).
pub fn relative_files(project: &Path, picked: &[PathBuf]) -> Result<Vec<String>> {
    let root = sa_engine::services::realpath(project)?;
    let mut out = Vec::new();
    for p in picked {
        let real = sa_engine::services::realpath(p)?;
        let rel = real
            .strip_prefix(&root)
            .map_err(|_| anyhow!("附件位于项目外：{}", p.display()))?;
        out.push(rel.to_string_lossy().replace('\\', "/"));
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use sa_engine::adapter::{AdapterError, Catalog, EventSink, RunOutcome, RunRequest, TurnHost};
    use std::future::Future;
    use std::pin::Pin;
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;

    type Boxed<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

    /// Answers every turn with `done <job>`; on an `ask` ticket it first
    /// requests one approval and waits for the operator.
    pub(crate) struct Echo(pub &'static str);

    impl Adapter for Echo {
        fn agent(&self) -> &'static str {
            self.0
        }
        fn catalog(&self) -> Boxed<anyhow::Result<Catalog>> {
            let route = json!({"provider": "test-local", "model": "default", "mode": "cli"});
            Box::pin(async move {
                Ok(Catalog {
                    value: json!({
                        "default": route,
                        "legacy_defaults": {"user": route},
                        "modes": [{"id": "cli", "available": true}],
                        "providers": [{"id": "test-local", "models": [{"id": "default"}]}]
                    }),
                })
            })
        }
        fn run(
            &self,
            req: RunRequest,
            host: Arc<dyn TurnHost>,
            _events: EventSink,
            _cancel: CancellationToken,
        ) -> Boxed<Result<RunOutcome, AdapterError>> {
            Box::pin(async move {
                let approved = if req.profile == "ask" {
                    if host.approve(json!({"tool": "fixture-write"})).await { " approved" } else { " denied" }
                } else {
                    ""
                };
                Ok(RunOutcome {
                    text: format!("done {}{approved}", req.job),
                    session_id: Some(format!("s-{}", req.job)),
                })
            })
        }
    }

    pub(crate) fn fakes(_: &Shared) -> Vec<Arc<dyn Adapter>> {
        sa_engine::adapter::AGENTS
            .iter()
            .map(|a| Arc::new(Echo(a)) as Arc<dyn Adapter>)
            .collect()
    }

    /// `Echo` whose catalog accepts every route bound in a legacy journal.
    struct Permissive(&'static str, Vec<Value>);

    impl Adapter for Permissive {
        fn agent(&self) -> &'static str {
            self.0
        }
        fn catalog(&self) -> Boxed<anyhow::Result<Catalog>> {
            let mut providers: Vec<Value> = vec![json!({"id": "test-local", "models": [{"id": "default"}]})];
            for r in &self.1 {
                let (p, m) = (r["provider"].as_str().unwrap_or_default(), r["model"].as_str().unwrap_or_default());
                providers.push(json!({"id": p, "models": [{"id": m}]}));
            }
            let route = json!({"provider": "test-local", "model": "default", "mode": "cli"});
            Box::pin(async move {
                Ok(Catalog {
                    value: json!({
                        "default": route,
                        "legacy_defaults": {"user": route},
                        "modes": [{"id": "cli", "available": true}],
                        "providers": providers
                    }),
                })
            })
        }
        fn run(
            &self,
            req: RunRequest,
            host: Arc<dyn TurnHost>,
            events: EventSink,
            cancel: CancellationToken,
        ) -> Boxed<Result<RunOutcome, AdapterError>> {
            Echo(self.0).run(req, host, events, cancel)
        }
    }

    /// `SA_LEGACY_COPY=<dir>`: a COPY of a journal written by the Node
    /// engine (never the live one). Opens it with the Rust scheduler, checks
    /// recovery, and follows up every finished non-continuous ticket.
    #[test]
    fn legacy_journal_copy_opens_and_resumes() {
        let Ok(dir) = std::env::var("SA_LEGACY_COPY") else {
            eprintln!("set SA_LEGACY_COPY to a copied journal directory");
            return;
        };
        let dir = PathBuf::from(dir);
        assert_ne!(
            sa_engine::services::realpath(&dir).ok(),
            sa_engine::services::realpath(&sa_store::default_data_dir()).ok(),
            "refusing to run against the live journal"
        );
        let routes: Vec<Value> = {
            let db = rusqlite::Connection::open(dir.join(sa_store::DB_FILE)).unwrap();
            let mut out = vec![];
            let mut add = |text: String| {
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    if v.get("provider").is_some() {
                        out.push(v);
                    } else if let Some(m) = v.as_object() {
                        out.extend(m.values().cloned());
                    }
                }
            };
            for sql in [
                "SELECT route FROM native_routes",
                "SELECT agent_routes FROM tickets",
                "SELECT routes FROM route_defaults",
            ] {
                let mut st = db.prepare(sql).unwrap();
                for r in st.query_map([], |r| r.get::<_, String>(0)).unwrap() {
                    add(r.unwrap());
                }
            }
            out
        };
        let adapters = move |_: &Shared| {
            AGENTS
                .iter()
                .map(|a| Arc::new(Permissive(a, routes.clone())) as Arc<dyn Adapter>)
                .collect()
        };
        let host = Host::start_with(dir.clone(), adapters, || {}).unwrap();
        assert!(host.fault().is_none(), "{:?}", host.fault());
        let roots = host
            .engine
            .db()
            .with(|q| {
                q.all(
                    "SELECT t.id,t.permission_profile p,j.state FROM tickets t JOIN jobs j ON j.ticket=t.id AND j.parent IS NULL",
                    &[],
                )
            })
            .unwrap();
        eprintln!("legacy roots: {roots:?}");
        let running = host
            .engine
            .db()
            .with(|q| q.int("SELECT count(*) FROM jobs WHERE state='running'", &[]))
            .unwrap();
        assert_eq!(running, 0, "recovery left running jobs");
        let mut followed = 0;
        for r in &roots {
            let ticket = r["id"].as_i64().unwrap();
            if r["state"] != json!("done") || r["p"] == json!("auto") {
                continue;
            }
            let replies = |h: &Host| kinds(h, ticket).iter().filter(|k| *k == "reply").count();
            let before = replies(&host);
            host.submit(Action::Follow {
                ticket,
                body: "核对迁移后的续跑".into(),
                resume: false,
            });
            assert!(outcome(&host).result.is_ok());
            let end = Instant::now() + Duration::from_secs(30);
            while !(root_state(&host, ticket) == "done" && replies(&host) > before) {
                assert!(Instant::now() < end, "ticket {ticket} stuck: {:?}", host.engine.root(ticket));
                std::thread::sleep(Duration::from_millis(50));
            }
            let reply = host
                .engine
                .db()
                .with(|q| q.one("SELECT result FROM jobs WHERE ticket=? AND parent IS NULL", &[&ticket]))
                .unwrap()
                .unwrap();
            eprintln!("ticket {ticket}: {}", reply["result"]);
            followed += 1;
        }
        assert!(host.fault().is_none(), "{:?}", host.fault());
        eprintln!("followed {followed} legacy tickets");
        // The read model still opens the migrated journal.
        drop(host);
        let mut store = sa_store::Store::open_read_only(&dir).unwrap();
        for r in &roots {
            store.snapshot(r["id"].as_i64().unwrap(), 0).unwrap();
        }
    }

    /// Drive a live ticket until its root is terminal, approving every
    /// request (live tests only run in temporary projects; DeepSeek Harness
    /// asks for the whole turn's workspace scope on `ask` tickets).
    fn drive_live(host: &Host, ticket: i64, limit: Duration) -> serde_json::Map<String, Value> {
        let end = Instant::now() + limit;
        loop {
            let pending = host
                .engine
                .db()
                .with(|q| q.all("SELECT id,action FROM approvals WHERE ticket=? AND decision='pending'", &[&ticket]))
                .unwrap();
            for a in pending {
                let action = a["action"].as_str().unwrap_or_default();
                eprintln!("approval {}: {}", a["id"], action.chars().take(160).collect::<String>());
                let _ = host.engine.decide(a["id"].as_i64().unwrap(), true);
            }
            let root = host.engine.root(ticket).unwrap();
            if matches!(
                root["state"].as_str(),
                Some("done" | "failed" | "partial" | "cancelled" | "interrupted")
            ) {
                return root;
            }
            if Instant::now() >= end {
                eprintln!("live ticket timed out");
                return root;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// The `sa-ext` fixture MCP server built next to this test binary.
    fn fixture_mcp() -> Option<PathBuf> {
        let deps = std::env::current_exe().ok()?.parent()?.to_path_buf();
        let exe = deps.parent()?.join(format!("sa-ext-fixture-mcp{}", std::env::consts::EXE_SUFFIX));
        exe.is_file().then_some(exe)
    }

    /// Billable: `SA_LIVE_EXT=1`. A real DeepSeek lead calls a granted MCP
    /// tool (with operator approval) and records a shared memory entry.
    #[test]
    fn live_mcp_tool_call_and_project_memory() {
        if std::env::var("SA_LIVE_EXT").as_deref() != Ok("1") {
            eprintln!("set SA_LIVE_EXT=1 to run a billable MCP + knowledge turn");
            return;
        }
        let exe = fixture_mcp().expect("build the workspace first: sa-ext-fixture-mcp");
        let data = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let secret_file = data.path().join("mcp-secret");
        let secret = format!("mcp-{:x}", std::process::id() as u64 * 104_729 + 7);
        std::fs::write(&secret_file, &secret).unwrap();
        let host = Host::start(data.path().to_path_buf(), || {}).unwrap();
        let ext = host.ext();
        let server = ext
            .save_server(&json!({"name": "proof", "command": exe, "args": [secret_file], "enabled": true}))
            .unwrap();
        let tools = host.runtime().block_on(ext.probe(&server)).unwrap();
        eprintln!("probed tools: {tools}");
        ext.set_defaults(&json!({"skills": [], "servers": [server]}), None).unwrap();

        let memo = format!("构建口令 {secret} 已核对");
        let goal = format!(
            "Call the MCP tool `challenge` (no arguments) of the MCP server listed in project_extensions.mcp, \
             using an SA_TOOL line whose server field is that entry's `server` value. \
             After its receipt arrives, reply with only the exact secret text it returned, and on a separate line \
             record this project memory exactly: SA_MEMO {{\"kind\":\"memory\",\"text\":\"{memo}\"}} . Do not modify files."
        );
        host.submit(Action::Create {
            goal,
            cwd: Some(project.path().to_path_buf()),
            lead: "deepseek".into(),
            profile: "ask".into(),
            files: vec![],
            selections: None,
            extensions: None,
        });
        let ticket = outcome(&host).result.unwrap().unwrap();
        let root = drive_live(&host, ticket, Duration::from_secs(420));
        let k = kinds(&host, ticket);
        eprintln!("state={} result={:?} kinds={k:?}", root["state"], root["result"]);
        for e in host
            .engine
            .db()
            .with(|q| {
                q.all(
                    "SELECT seq,turn,kind,substr(payload,1,300) p FROM events WHERE ticket=? AND kind NOT IN ('native','delta') ORDER BY seq",
                    &[&ticket],
                )
            })
            .unwrap()
        {
            eprintln!("  #{} t{} {} {}", e["seq"], e["turn"], e["kind"].as_str().unwrap_or_default(), e["p"].as_str().unwrap_or_default());
        }
        assert_eq!(root["state"], json!("done"));
        assert!(k.iter().any(|x| x == "mcp.result"), "no MCP receipt");
        let receipt = host
            .engine
            .db()
            .with(|q| q.one("SELECT state FROM studio_tool_receipts WHERE job=(SELECT id FROM jobs WHERE ticket=? AND parent IS NULL)", &[&ticket]))
            .unwrap();
        eprintln!("receipt {receipt:?}");
        assert!(root["result"].as_str().unwrap_or_default().contains(&secret), "secret not returned");
        let entries = host
            .engine
            .knowledge(&sa_engine::services::realpath(project.path()).unwrap().display().to_string())
            .unwrap();
        eprintln!("knowledge: {entries:?}");
        assert!(entries.iter().any(|e| e.kind == "memory" && e.body == memo), "SA_MEMO not recorded");
        assert!(entries.iter().any(|e| e.kind == "progress"), "no automatic progress entry");
    }

    /// Billable: `SA_LIVE_PROVIDER=<agent>,<base_url>,<KEY_ENV>,<model>`. One
    /// read-only turn through an operator-added custom provider.
    #[test]
    fn live_custom_provider_turn() {
        let Ok(spec) = std::env::var("SA_LIVE_PROVIDER") else {
            eprintln!("set SA_LIVE_PROVIDER=agent,base_url,KEY_ENV,model to run a billable custom-provider turn");
            return;
        };
        let parts: Vec<&str> = spec.split(',').collect();
        let [agent, base, key_env, model] = parts[..] else { panic!("bad SA_LIVE_PROVIDER") };
        let data = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let nonce = format!("cp-{:x}", std::process::id() as u64 * 7_919 + 3);
        std::fs::write(project.path().join("challenge.txt"), &nonce).unwrap();
        let host = Host::start(data.path().to_path_buf(), || {}).unwrap();
        let provider = CustomProvider {
            id: String::new(),
            agent: agent.into(),
            name: "live-test".into(),
            base_url: base.into(),
            key_env: key_env.into(),
            models: vec![model.into()],
            enabled: true,
        };
        host.probe_models(provider.clone());
        match host.probes.recv_timeout(Duration::from_secs(30)).unwrap() {
            Ok(models) => eprintln!("probe: {} models, listed={}", models.len(), models.iter().any(|m| m == model)),
            Err(e) => eprintln!("probe failed: {e}"),
        }
        host.submit(Action::ProviderSave { provider });
        assert!(outcome(&host).result.is_ok());
        let saved = host.providers().remove(0);
        let route = json!({"provider": saved.route_id(), "model": model, "mode": "cli"});
        host.submit(Action::Create {
            goal: "Read challenge.txt in this project. Return only the exact text inside it. Do not modify any files.".into(),
            cwd: Some(project.path().to_path_buf()),
            lead: agent.into(),
            profile: "readonly".into(),
            files: vec![],
            selections: Some(json!({agent: route})),
            extensions: None,
        });
        let ticket = outcome(&host).result.unwrap().unwrap();
        let root = drive_live(&host, ticket, Duration::from_secs(300));
        let bound = host
            .engine
            .db()
            .with(|q| q.one("SELECT route FROM native_routes WHERE job=?", &[&root["id"].as_i64().unwrap()]))
            .unwrap();
        eprintln!("state={} result={:?} route={bound:?}", root["state"], root["result"]);
        assert_eq!(root["state"], json!("done"));
        // Weaker models may wrap the answer; the file content must be there.
        assert!(root["result"].as_str().unwrap_or_default().contains(&nonce), "challenge not read");
        assert!(bound.unwrap()["route"].as_str().unwrap().contains(&saved.route_id()));
    }

    /// Billable: `SA_LIVE_ROTATION=<agent>,<base_url>,<KEY_ENV>,<model>`. The
    /// first provider carries a deliberately invalid key; with rotation on,
    /// the turn must move to the second (valid) provider and finish.
    #[test]
    fn live_rotation_after_auth_failure() {
        let Ok(spec) = std::env::var("SA_LIVE_ROTATION") else {
            eprintln!("set SA_LIVE_ROTATION=agent,base_url,KEY_ENV,model to run a billable rotation turn");
            return;
        };
        let parts: Vec<&str> = spec.split(',').collect();
        let [agent, base, key_env, model] = parts[..] else { panic!("bad SA_LIVE_ROTATION") };
        // SAFETY: set before any adapter thread reads the environment.
        unsafe { std::env::set_var("SA_ROTATION_INVALID_KEY", "sk-invalid-for-rotation-test") };
        let data = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let nonce = format!("rot-{:x}", std::process::id() as u64 * 6_007 + 1);
        std::fs::write(project.path().join("challenge.txt"), &nonce).unwrap();
        let host = Host::start(data.path().to_path_buf(), || {}).unwrap();
        for (name, env) in [("broken", "SA_ROTATION_INVALID_KEY"), ("backup", key_env)] {
            host.submit(Action::ProviderSave {
                provider: CustomProvider {
                    id: String::new(),
                    agent: agent.into(),
                    name: name.into(),
                    base_url: base.into(),
                    key_env: env.into(),
                    models: vec![model.into()],
                    enabled: true,
                },
            });
            assert!(outcome(&host).result.is_ok());
        }
        let list = host.providers();
        let id = |n: &str| list.iter().find(|p| p.name == n).unwrap().route_id();
        let (broken, backup) = (id("broken"), id("backup"));
        let r = |p: &str| sa_store::providers::RouteRef {
            provider: p.into(),
            model: model.into(),
            mode: "cli".into(),
        };
        host.submit(Action::RotationSave {
            rotation: Rotation {
                enabled: true,
                order: [(agent.to_string(), vec![r(&broken), r(&backup)])].into(),
            },
        });
        assert!(outcome(&host).result.is_ok());
        host.submit(Action::Create {
            goal: "Read challenge.txt in this project. Return only the exact text inside it. Do not modify any files.".into(),
            cwd: Some(project.path().to_path_buf()),
            lead: agent.into(),
            profile: "readonly".into(),
            files: vec![],
            selections: Some(json!({agent: {"provider": broken, "model": model, "mode": "cli"}})),
            extensions: None,
        });
        let ticket = outcome(&host).result.unwrap().unwrap();
        let root = drive_live(&host, ticket, Duration::from_secs(300));
        let rotated = host
            .engine
            .db()
            .with(|q| q.all("SELECT payload FROM events WHERE ticket=? AND kind IN ('route.rotated','turn.gave_up','turn.retry_scheduled')", &[&ticket]))
            .unwrap();
        eprintln!("state={} result={:?} resilience={rotated:?}", root["state"], root["result"]);
        assert_eq!(root["state"], json!("done"));
        assert!(root["result"].as_str().unwrap_or_default().contains(&nonce));
        assert!(
            rotated.iter().any(|e| e["payload"].as_str().unwrap_or_default().contains(&backup)),
            "no rotation to the backup provider"
        );
    }

    fn outcome(host: &Host) -> Outcome {
        host.rx.recv_timeout(Duration::from_secs(10)).expect("outcome")
    }

    fn until(cond: impl Fn() -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out");
    }

    fn root_state(host: &Host, ticket: i64) -> String {
        host.engine
            .root(ticket)
            .ok()
            .and_then(|r| r.get("state").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default()
    }

    fn kinds(host: &Host, ticket: i64) -> Vec<String> {
        host.engine
            .db()
            .with(|q| q.all("SELECT kind FROM events WHERE ticket=? ORDER BY seq", &[&ticket]))
            .unwrap()
            .into_iter()
            .map(|r| r["kind"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    #[test]
    fn host_owns_the_journal_and_runs_operator_actions_in_order() {
        let data = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("a.txt"), b"\x00\xff\r\n").unwrap();
        let mut host = Host::start_with(data.path().to_path_buf(), fakes, || {}).unwrap();

        // A second workbench on the same journal is refused.
        let busy = Host::start_with(data.path().to_path_buf(), fakes, || {}).err().unwrap();
        assert_eq!(busy.to_string(), owner::BUSY);

        // Attachments outside the project are rejected before submission.
        let outside = data.path().join("x.txt");
        std::fs::write(&outside, b"x").unwrap();
        assert!(relative_files(project.path(), &[outside]).is_err());
        let files = relative_files(project.path(), &[project.path().join("a.txt")]).unwrap();
        assert_eq!(files, vec!["a.txt".to_string()]);

        assert!(host.submit(Action::Create {
            goal: "fixture".into(),
            cwd: Some(project.path().to_path_buf()),
            lead: "deepseek".into(),
            profile: "readonly".into(),
            files,
            selections: None,
            extensions: None,
        }));
        let o = outcome(&host);
        assert_eq!(o.kind, ActionKind::Create);
        let ticket = o.result.unwrap().unwrap();
        until(|| root_state(&host, ticket) == "done");

        host.submit(Action::Follow {
            ticket,
            body: "继续".into(),
            resume: false,
        });
        assert!(outcome(&host).result.is_ok());
        until(|| root_state(&host, ticket) == "done" && kinds(&host, ticket).iter().filter(|k| *k == "reply").count() == 2);

        // Markdown transcript: goal, follow-up and both replies, controls removed.
        let md = data.path().join("t.md");
        crate::ops::export_markdown(data.path(), ticket, &md).unwrap();
        let text = std::fs::read_to_string(&md).unwrap();
        assert!(text.starts_with("# fixture
"), "{text}");
        assert!(text.contains("## 你 · 追加指令") && text.contains("继续"), "{text}");
        assert_eq!(text.matches("· 主控 ·").count(), 2, "{text}");
        assert!(crate::ops::export_markdown(data.path(), ticket, &md).is_err(), "never overwrites");

        host.submit(Action::PlanAdd {
            ticket,
            body: "  核对附件  ".into(),
        });
        assert!(outcome(&host).result.is_ok());
        let plan = host
            .engine
            .db()
            .with(|q| q.one("SELECT id,body FROM studio_plan WHERE ticket=?", &[&ticket]))
            .unwrap()
            .unwrap();
        assert_eq!(plan["body"], json!("核对附件"));
        let id = plan["id"].as_i64().unwrap();
        host.submit(Action::PlanToggle { ticket, id });
        assert!(outcome(&host).result.is_ok());
        host.submit(Action::PlanToggle { ticket, id: id + 99 });
        assert_eq!(outcome(&host).result.unwrap_err(), "计划项不存在");

        // Invalid actions come back as errors without stopping the host.
        host.submit(Action::Retry { job: 9999 });
        assert!(outcome(&host).result.is_err());
        host.submit(Action::Decide { id: 9999, allow: true });
        assert_eq!(outcome(&host).result.unwrap_err(), "审批已失效");
        host.submit(Action::Create {
            goal: "  ".into(),
            cwd: None,
            lead: "deepseek".into(),
            profile: "readonly".into(),
            files: vec![],
            selections: None,
            extensions: None,
        });
        assert_eq!(outcome(&host).result.unwrap_err(), "请输入目标");

        host.submit(Action::Cancel { ticket });
        assert!(outcome(&host).result.is_ok());
        let k = kinds(&host, ticket);
        for want in ["user", "files", "user.follow", "plan.add", "plan.done"] {
            assert!(k.iter().any(|x| x == want), "missing {want} in {k:?}");
        }
        assert!(host.fault().is_none());

        // Shutdown releases the lock for the next workbench.
        host.shutdown();
        assert!(!owner::is_claimed(data.path()));
        let again = Host::start_with(data.path().to_path_buf(), fakes, || {}).unwrap();
        drop(again);
    }
}
