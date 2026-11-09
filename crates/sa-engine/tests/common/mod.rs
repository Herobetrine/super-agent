//! Fake harness adapters and helpers for engine tests (the `fixture`,
//! `until` and `gate` helpers of `tests-node/engine*.test.cjs`).

#![allow(dead_code)]

use futures::FutureExt;
use futures::future::BoxFuture;
use sa_engine::adapter::{
    AGENTS, Adapter, AdapterError, AdapterEvent, Catalog, EventSink, RunOutcome, RunRequest,
    TurnHost,
};
use sa_engine::{CreateParams, Engine, EngineOptions};
use serde_json::{Map, Value, json};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Everything one native turn sees.
#[derive(Clone)]
pub struct Ctx {
    pub name: String,
    pub packet: Value,
    pub req: RunRequest,
    pub host: Arc<dyn TurnHost>,
    pub events: EventSink,
    pub cancel: CancellationToken,
}

impl Ctx {
    pub fn emit(&self, kind: &str, data: Value) {
        let _ = self.events.send(AdapterEvent::Journal {
            kind: kind.into(),
            data,
        });
    }
    pub fn depth(&self) -> i64 {
        self.packet["depth"].as_i64().unwrap_or_default()
    }
    pub fn turn(&self) -> i64 {
        self.packet["turn"].as_i64().unwrap_or_default()
    }
    pub fn job(&self) -> i64 {
        self.packet["job"].as_i64().unwrap_or_default()
    }
    pub fn phase(&self) -> Option<&str> {
        self.packet["autonomy"]["phase"].as_str()
    }
    /// The tool-evidence frame of `engine-autonomy.test.cjs`.
    pub fn prove(&self) {
        self.emit(
            "native",
            json!({"type": "tool_result", "callId": "inspect", "status": "completed", "result": "checked fixture"}),
        );
    }
}

pub type Script =
    Arc<dyn Fn(Ctx) -> BoxFuture<'static, Result<String, AdapterError>> + Send + Sync>;
pub type CatalogFn = Arc<dyn Fn() -> BoxFuture<'static, anyhow::Result<Value>> + Send + Sync>;

pub fn script<F, Fut>(f: F) -> Script
where
    F: Fn(Ctx) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<String, AdapterError>> + Send + 'static,
{
    Arc::new(move |c| f(c).boxed())
}

pub fn catalog_of(value: Value) -> CatalogFn {
    Arc::new(move || {
        let v = value.clone();
        async move { Ok(v) }.boxed()
    })
}

/// `catalog()` of `engine.test.cjs`.
pub fn t1_catalog() -> Value {
    let route = json!({"provider": "test-local", "model": "default", "mode": "cli"});
    json!({
        "default": route,
        "legacy_defaults": {"user": route},
        "modes": [{"id": "cli", "available": true}],
        "providers": [
            {"id": "test-local", "models": [{"id": "default"}, {"id": "review"}, {"id": "disabled", "available": false}]},
            {"id": "other", "models": [{"id": "foreign"}]}
        ]
    })
}

/// `catalog()` of `engine-autonomy.test.cjs`.
pub fn t2_catalog() -> Value {
    json!({
        "default": {"provider": "source", "mode": "cli", "model": "base"},
        "modes": [{"id": "cli", "available": true}],
        "providers": [
            {"id": "source", "models": [{"id": "base"}, {"id": "second"}, {"id": "removed", "available": false}]},
            {"id": "foreign", "models": [{"id": "elsewhere"}]}
        ]
    })
}

pub struct Call {
    pub name: String,
    pub packet: Value,
    pub req: RunRequest,
}

pub struct Shared {
    pub script: Mutex<Script>,
    pub catalog: Mutex<CatalogFn>,
    pub calls: Mutex<Vec<Call>>,
    pub panics: Mutex<Vec<String>>,
}

impl Shared {
    pub fn calls<T>(&self, f: impl Fn(&Call) -> Option<T>) -> Vec<T> {
        self.calls.lock().unwrap().iter().filter_map(f).collect()
    }
    pub fn count(&self, f: impl Fn(&Call) -> bool) -> usize {
        self.calls.lock().unwrap().iter().filter(|c| f(c)).count()
    }
    pub fn set_catalog(&self, c: CatalogFn) {
        *self.catalog.lock().unwrap() = c;
    }
}

struct Fake {
    name: &'static str,
    shared: Arc<Shared>,
}

impl Adapter for Fake {
    fn agent(&self) -> &'static str {
        self.name
    }
    fn catalog(&self) -> BoxFuture<'static, anyhow::Result<Catalog>> {
        let f = self.shared.catalog.lock().unwrap().clone();
        async move { Ok(Catalog { value: f().await? }) }.boxed()
    }
    fn run(
        &self,
        req: RunRequest,
        host: Arc<dyn TurnHost>,
        events: EventSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<RunOutcome, AdapterError>> {
        let packet: Value = serde_json::from_str(req.prompt.rsplit('\n').next().unwrap_or("null"))
            .expect("packet JSON");
        let job = packet["job"].clone();
        self.shared.calls.lock().unwrap().push(Call {
            name: self.name.into(),
            packet: packet.clone(),
            req: req.clone(),
        });
        let script = self.shared.script.lock().unwrap().clone();
        let shared = self.shared.clone();
        let ctx = Ctx {
            name: self.name.into(),
            packet,
            req,
            host,
            events,
            cancel,
        };
        async move {
            match AssertUnwindSafe(script(ctx)).catch_unwind().await {
                Ok(Ok(text)) => Ok(RunOutcome {
                    text,
                    session_id: Some(format!("session-{job}")),
                }),
                Ok(Err(e)) => Err(e),
                Err(p) => {
                    let msg = p
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_else(|| "panic".into());
                    shared.panics.lock().unwrap().push(msg.clone());
                    Err(AdapterError::new(
                        sa_engine::adapter::ErrorClass::Protocol,
                        msg,
                    ))
                }
            }
        }
        .boxed()
    }
}

pub fn adapters(shared: &Arc<Shared>) -> Vec<Arc<dyn Adapter>> {
    AGENTS
        .iter()
        .map(|name| {
            Arc::new(Fake {
                name,
                shared: shared.clone(),
            }) as Arc<dyn Adapter>
        })
        .collect()
}

/// Tests drive admission with `step()`; a long tick keeps the background
/// timer from racing synchronous assertions.
pub fn quiet() -> EngineOptions {
    EngineOptions {
        tick: Duration::from_secs(3600),
        ..Default::default()
    }
}

pub struct Fx {
    pub engine: Engine,
    pub dir: TempDir,
    pub project: PathBuf,
    pub data: PathBuf,
    pub shared: Arc<Shared>,
}

impl Fx {
    pub fn new(s: Script, catalog: Value) -> Self {
        Self::with(s, catalog, quiet())
    }

    pub fn with(s: Script, catalog: Value, opts: EngineOptions) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("superagent-rust-test-")
            .tempdir()
            .unwrap();
        let project = dir.path().join("project");
        let data = dir.path().join("state");
        std::fs::create_dir_all(&project).unwrap();
        let shared = Arc::new(Shared {
            script: Mutex::new(s),
            catalog: Mutex::new(catalog_of(catalog)),
            calls: Mutex::new(vec![]),
            panics: Mutex::new(vec![]),
        });
        let engine = Engine::open(&data, adapters(&shared), opts).unwrap();
        Self {
            engine,
            dir,
            project,
            data,
            shared,
        }
    }

    /// Close and reopen the same journal (a restart).
    pub async fn restart(&mut self, opts: EngineOptions) {
        self.engine.close().await;
        self.engine = Engine::open(&self.data, adapters(&self.shared), opts).unwrap();
    }

    pub async fn create(&self, params: CreateParams) -> i64 {
        self.engine.create(params).await.unwrap()
    }

    /// `begin(f, fields)`: lead codex by default, then `start()`.
    pub async fn begin(&self, params: CreateParams) -> i64 {
        let id = self.create(params).await;
        self.engine.start().unwrap();
        id
    }

    pub fn params(&self) -> CreateParams {
        CreateParams::new("fixture", &self.project).lead("codex")
    }

    pub async fn until(&self, cond: impl Fn() -> bool) {
        until(&self.engine, &self.shared, Duration::from_secs(15), cond).await
    }

    pub async fn finish(&self, ticket: i64, state: &str) {
        self.until(|| root_state(&self.engine, ticket) == state)
            .await
    }

    pub fn all(&self, sql: &str) -> Vec<Map<String, Value>> {
        all(&self.engine, sql)
    }

    pub fn int(&self, sql: &str) -> i64 {
        self.engine.db().with(|q| q.int(sql, &[])).unwrap()
    }
}

pub async fn until(engine: &Engine, shared: &Shared, limit: Duration, cond: impl Fn() -> bool) {
    let end = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < end {
        if cond() {
            return;
        }
        if let Some(p) = shared.panics.lock().unwrap().first() {
            panic!("adapter assertion failed: {p}");
        }
        if let Some(f) = engine.fault() {
            panic!("engine fault: {f}");
        }
        engine.step();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!(
        "timed out {:?}",
        all(engine, "SELECT id,turn,state,result FROM jobs")
    );
}

pub fn all(engine: &Engine, sql: &str) -> Vec<Map<String, Value>> {
    engine.db().with(|q| q.all(sql, &[])).unwrap()
}

pub fn root_state(engine: &Engine, ticket: i64) -> String {
    engine
        .root(ticket)
        .map(|r| r["state"].as_str().unwrap_or_default().to_string())
        .unwrap_or_default()
}

pub fn wire(tag: &str, v: Value) -> String {
    format!("{tag} {}", serde_json::to_string(&v).unwrap())
}

/// `send(x)` of `engine.test.cjs`.
pub fn send(extra: Value) -> String {
    let mut base =
        json!({"id": "peer", "to": "deepseek", "body": "核对  \r\n\t🧭", "write": false});
    if let (Value::Object(b), Value::Object(e)) = (&mut base, extra) {
        for (k, v) in e {
            b.insert(k, v);
        }
    }
    wire("SA_SEND", base)
}

pub fn sha(text: impl AsRef<[u8]>) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(text.as_ref()))
}

/// A one-shot latch that many tasks can await.
#[derive(Clone)]
pub struct Gate(watch::Sender<bool>, watch::Receiver<bool>);

impl Gate {
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(false);
        Self(tx, rx)
    }
    pub fn open(&self) {
        let _ = self.0.send(true);
    }
    pub fn is_open(&self) -> bool {
        *self.1.borrow()
    }
    pub async fn wait(&self) {
        let mut rx = self.1.clone();
        let _ = rx.wait_for(|v| *v).await;
    }
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

pub fn payloads(fx: &Fx, sql: &str) -> Vec<Value> {
    fx.all(sql)
        .iter()
        .map(|r| serde_json::from_str(r["payload"].as_str().unwrap()).unwrap())
        .collect()
}

pub fn project_file(project: &Path, name: &str, bytes: &[u8]) {
    std::fs::write(project.join(name), bytes).unwrap();
}
