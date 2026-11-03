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
use std::sync::{Arc, mpsc};
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

pub struct Host {
    tx: Option<tokio::sync::mpsc::UnboundedSender<Action>>,
    pub rx: mpsc::Receiver<Outcome>,
    engine: Engine,
    thread: Option<JoinHandle<()>>,
    runtime: tokio::runtime::Handle,
    adapters: Arc<HashMap<String, Arc<dyn Adapter>>>,
    catalog_tx: mpsc::Sender<CatalogReply>,
    pub catalogs: mpsc::Receiver<CatalogReply>,
    notify: Notify,
}

impl Host {
    /// Claim `dir`, open the journal and start scheduling. `notify` is called
    /// after every outcome (the UI repaints).
    pub fn start(dir: PathBuf, notify: impl Fn() + Send + Sync + 'static) -> Result<Self> {
        Self::start_with(dir, || sa_adapters::all(Sys::from_process()), notify)
    }

    /// [`start`](Self::start) with explicit harness adapters (tests).
    pub fn start_with(
        dir: PathBuf,
        adapters: impl FnOnce() -> Vec<Arc<dyn Adapter>> + Send + 'static,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        type Ready = (Engine, tokio::runtime::Handle, Vec<Arc<dyn Adapter>>);
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
                    let adapters = adapters();
                    let engine = match open_engine(&dir, adapters.clone()) {
                        Ok(e) => e,
                        Err(e) => {
                            let _ = ready_tx.send(Err(format!("{e:#}")));
                            return;
                        }
                    };
                    let handle = tokio::runtime::Handle::current();
                    let _ = ready_tx.send(Ok((engine.clone(), handle, adapters)));
                    while let Some(action) = rx.recv().await {
                        let kind = action.kind();
                        // Create awaits attachment snapshots; run every action in
                        // order so the UI sees outcomes in submission order.
                        let result = execute(&engine, action).await.map_err(|e| format!("{e:#}"));
                        let _ = out_tx.send(Outcome { kind, result });
                        wake();
                    }
                    engine.close().await;
                    // `_owner` drops here: the lock is released after the
                    // journal is closed.
                });
            })
            .context("无法启动调度线程")?;
        match ready_rx.recv() {
            Ok(Ok((engine, runtime, adapters))) => {
                let (catalog_tx, catalogs) = mpsc::channel();
                Ok(Self {
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

fn open_engine(dir: &Path, adapters: Vec<Arc<dyn Adapter>>) -> Result<Engine> {
    let engine = Engine::open(dir, adapters, EngineOptions::default())?;
    engine
        .db()
        .with(|q| Ok(q.conn().execute_batch(sa_store::STUDIO_PLAN)?))?;
    engine.start()?;
    Ok(engine)
}

/// `Studio.create` checks (ST:299-317) before `Engine::create`.
async fn create(
    engine: &Engine,
    goal: String,
    cwd: Option<PathBuf>,
    lead: String,
    profile: String,
    files: Vec<String>,
    selections: Option<Value>,
) -> Result<i64> {
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
    engine.create(params).await
}

async fn execute(engine: &Engine, action: Action) -> Result<Option<i64>> {
    match action {
        Action::Create {
            goal,
            cwd,
            lead,
            profile,
            files,
            selections,
        } => Ok(Some(create(engine, goal, cwd, lead, profile, files, selections).await?)),
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

    pub(crate) fn fakes() -> Vec<Arc<dyn Adapter>> {
        sa_engine::adapter::AGENTS
            .iter()
            .map(|a| Arc::new(Echo(a)) as Arc<dyn Adapter>)
            .collect()
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
