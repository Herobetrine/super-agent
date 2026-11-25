mod common;

use common::*;
use futures::future::BoxFuture;
use sa_engine::{Engine, adapter::*};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

struct Slow {
    inner: Arc<dyn Adapter>,
    local: bool,
    delay: Duration,
    gate: Gate,
    full: AtomicUsize,
    selected: AtomicUsize,
    revision: AtomicUsize,
    fail: AtomicBool,
}

impl Slow {
    fn new(fx: &Fx, agent: &str, local: bool, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            inner: adapters(&fx.shared)
                .into_iter()
                .find(|a| a.agent() == agent)
                .unwrap(),
            local,
            delay,
            gate: Gate::new(),
            full: 0.into(),
            selected: 0.into(),
            revision: 0.into(),
            fail: false.into(),
        })
    }
}

impl Adapter for Slow {
    fn agent(&self) -> &'static str {
        self.inner.agent()
    }

    fn local_catalog(&self) -> Value {
        if !self.local {
            return Value::Null;
        }
        let mut value = t1_catalog();
        value["providers"][0]["models"] = json!([{"id":"default"}]);
        value
    }
    fn catalog_revision(&self) -> String {
        self.revision.load(SeqCst).to_string()
    }
    fn cli_info(&self) -> BoxFuture<'static, Value> {
        Box::pin(async { json!({"version":"1.2.3","compatibility":"unverified"}) })
    }
    fn catalog(&self) -> BoxFuture<'static, anyhow::Result<Catalog>> {
        self.full.fetch_add(1, SeqCst);
        let (delay, gate, fail) = (self.delay, self.gate.clone(), self.fail.load(SeqCst));
        let generation = self.revision.load(SeqCst);
        Box::pin(async move {
            gate.wait().await;
            tokio::time::sleep(delay).await;
            anyhow::ensure!(!fail, "injected catalog failure");
            let mut value = t1_catalog();
            value["generation"] = json!(generation);
            Ok(Catalog { value })
        })
    }
    fn catalog_for_route(&self, _: Route) -> Option<BoxFuture<'static, anyhow::Result<Catalog>>> {
        self.selected.fetch_add(1, SeqCst);
        Some(Box::pin(async {
            let mut value = t1_catalog();
            value["providers"][0]["models"] = json!([{"id":"default"},{"id":"new-model"}]);
            Ok(Catalog { value })
        }))
    }
    fn run(
        &self,
        req: RunRequest,
        host: Arc<dyn TurnHost>,
        events: EventSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<RunOutcome, AdapterError>> {
        self.inner.run(req, host, events, cancel)
    }
}

fn fixture() -> Fx {
    Fx::new(script(|_| async { Ok("done".into()) }), t1_catalog())
}

async fn reopen(fx: &mut Fx, agents: &[Arc<Slow>]) {
    fx.engine.close().await;
    let adapters = agents
        .iter()
        .map(|a| a.clone() as Arc<dyn Adapter>)
        .collect();
    fx.engine = Engine::open(&fx.data, adapters, quiet()).unwrap();
}

async fn packet(fx: &Fx, ticket: i64) -> Value {
    let find = || {
        fx.shared
            .calls(|c| (c.req.ticket == ticket).then(|| c.packet.clone()))
            .pop()
    };
    tokio::time::timeout(Duration::from_secs(1), async {
        fx.engine.start().unwrap();
        fx.until(|| find().is_some()).await;
        find().unwrap()
    })
    .await
    .expect("lead did not reach its packet within one second")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_admissions_share_slow_refresh_then_restart_uses_persisted_menu() {
    let mut fx = fixture();
    let lead = Slow::new(&fx, "codex", true, Duration::from_secs(5));
    reopen(&mut fx, std::slice::from_ref(&lead)).await;
    lead.gate.open();
    for _ in 0..3 {
        let id = fx.create(fx.params()).await;
        let menu = packet(&fx, id).await["delegate_models"]["codex"].clone();
        assert_eq!(menu["partial"], true);
        assert_eq!(menu["models"], json!(["default"]));
        fx.finish(id, "done").await;
    }
    fx.until(|| fx.int("SELECT count(*) FROM catalog_cache WHERE agent='codex'") == 1)
        .await;
    assert_eq!(lead.full.load(SeqCst), 1);
    for restart in [false, true] {
        if restart {
            reopen(&mut fx, std::slice::from_ref(&lead)).await;
        }
        let id = fx.create(fx.params()).await;
        let menu = packet(&fx, id).await["delegate_models"]["codex"].clone();
        assert!(menu.get("partial").is_none());
        assert_eq!(menu["models"], json!(["default", "review"]));
        fx.finish(id, "done").await;
        assert_eq!(lead.full.load(SeqCst), 1);
    }
    let row = fx
        .all("SELECT fingerprint,value FROM catalog_cache WHERE agent='codex'")
        .pop()
        .unwrap();
    let value: Value = serde_json::from_str(row["value"].as_str().unwrap()).unwrap();
    assert_eq!(value["cli"]["version"], "1.2.3");
    assert!(row["fingerprint"].as_str().unwrap().contains("1.2.3"));
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_default_waits_for_only_its_own_shared_discovery() {
    let mut fx = fixture();
    let lead = Slow::new(&fx, "codex", false, Duration::ZERO);
    let peer = Slow::new(&fx, "claude", false, Duration::from_secs(5));
    reopen(&mut fx, &[lead.clone(), peer.clone()]).await;
    let id = fx.begin(fx.params()).await;
    fx.until(|| lead.full.load(SeqCst) == 1 && peer.full.load(SeqCst) == 1)
        .await;
    assert_eq!(fx.shared.count(|_| true), 0);
    lead.gate.open();
    let result = packet(&fx, id).await;
    assert_eq!(result["delegate_models"]["claude"]["partial"], true);
    assert_eq!(lead.full.load(SeqCst), 1);
    assert_eq!(peer.full.load(SeqCst), 1);
    fx.finish(id, "done").await;
    peer.gate.open();
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn desktop_and_three_cached_admissions_share_one_slow_refresh() {
    let mut fx = fixture();
    let original = Slow::new(&fx, "codex", true, Duration::ZERO);
    original.gate.open();
    reopen(&mut fx, std::slice::from_ref(&original)).await;
    fx.engine.request_catalog("codex", true).await.unwrap();
    let slow = Slow::new(&fx, "codex", true, Duration::from_secs(5));
    slow.revision.store(1, SeqCst);
    reopen(&mut fx, std::slice::from_ref(&slow)).await;
    let initial = tokio::time::timeout(
        Duration::from_secs(1),
        fx.engine.request_catalog("codex", false),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(initial["generation"], 0);
    let engine = fx.engine.clone();
    let forced = tokio::spawn(async move { engine.request_catalog("codex", true).await });
    for _ in 0..3 {
        let id = fx.create(fx.params()).await;
        assert_eq!(
            packet(&fx, id).await["delegate_models"]["codex"]["models"],
            json!(["default", "review"])
        );
        fx.finish(id, "done").await;
    }
    assert!(!forced.is_finished());
    assert_eq!(slow.full.load(SeqCst), 1);
    slow.gate.open();
    assert_eq!(forced.await.unwrap().unwrap()["generation"], 1);
    assert_eq!(slow.full.load(SeqCst), 1);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changed_revision_failed_refresh_preserves_menu_and_saved_entry() {
    let mut fx = fixture();
    let lead = Slow::new(&fx, "codex", true, Duration::ZERO);
    reopen(&mut fx, std::slice::from_ref(&lead)).await;
    lead.gate.open();
    fx.engine.request_catalog("codex", true).await.unwrap();
    let before = fx.all("SELECT * FROM catalog_cache");
    lead.fail.store(true, SeqCst);
    lead.revision.store(1, SeqCst);
    let id = fx.create(fx.params()).await;
    let result = packet(&fx, id).await;
    assert_eq!(
        result["delegate_models"]["codex"]["models"],
        json!(["default", "review"])
    );
    fx.until(|| lead.full.load(SeqCst) >= 2).await;
    assert!(fx.engine.request_catalog("codex", true).await.is_err());
    fx.finish(id, "done").await;
    let id = fx.create(fx.params()).await;
    assert_eq!(
        packet(&fx, id).await["delegate_models"],
        result["delegate_models"]
    );
    assert_eq!(fx.all("SELECT * FROM catalog_cache"), before);
    fx.finish(id, "done").await;
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_menu_miss_uses_one_targeted_probe_before_running() {
    let mut fx = fixture();
    let lead = Slow::new(&fx, "codex", true, Duration::ZERO);
    reopen(&mut fx, std::slice::from_ref(&lead)).await;
    lead.gate.open();
    fx.engine.request_catalog("codex", true).await.unwrap();
    fx.engine
        .db()
        .with(|q| {
            q.run("UPDATE catalog_cache SET fetched_at=0", &[])?;
            Ok(())
        })
        .unwrap();
    let fresh = Slow::new(&fx, "codex", true, Duration::ZERO);
    reopen(&mut fx, std::slice::from_ref(&fresh)).await;
    let mut params = fx.params();
    params.selections =
        Some(json!({"codex":{"provider":"test-local","model":"new-model","mode":"cli"}}));
    let id = fx.create(params).await;
    assert_eq!(
        fresh.selected.load(SeqCst),
        1,
        "creation checks the miss once"
    );
    packet(&fx, id).await;
    assert_eq!(
        fresh.selected.load(SeqCst),
        2,
        "admission checks the miss once"
    );
    assert_eq!(
        fx.shared.calls(|c| Some(c.req.route.model.clone())),
        ["new-model"]
    );
    fx.until(|| fresh.full.load(SeqCst) == 1).await;
    fresh.gate.open();
    fx.finish(id, "done").await;
    fx.engine.close().await;
}
