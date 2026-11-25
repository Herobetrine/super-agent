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
        let model = if self.revision.load(SeqCst) == 0 {
            "default"
        } else {
            "review"
        };
        value["default"]["model"] = json!(model);
        value["providers"][0]["models"] = json!([{"id":model}]);
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
        let catalog = self.inner.catalog();
        Box::pin(async move {
            gate.wait().await;
            tokio::time::sleep(delay).await;
            anyhow::ensure!(!fail, "injected catalog failure");
            let mut value = catalog.await?.value;
            value["generation"] = json!(generation);
            if generation > 0 && value["default"]["model"] == "default" {
                value["default"]["model"] = json!("review");
            }
            Ok(Catalog { value })
        })
    }
    fn catalog_for_route(&self, _: Route) -> Option<BoxFuture<'static, anyhow::Result<Catalog>>> {
        self.selected.fetch_add(1, SeqCst);
        let catalog = self.inner.catalog();
        Some(Box::pin(async move {
            let mut catalog = catalog.await?;
            catalog.value["providers"][0]["models"]
                .as_array_mut()
                .unwrap()
                .push(json!({"id":"new-model"}));
            Ok(catalog)
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
async fn new_ticket_binds_changed_local_default_while_existing_ticket_keeps_binding() {
    let mut fx = fixture();
    let original = Slow::new(&fx, "codex", true, Duration::ZERO);
    original.gate.open();
    reopen(&mut fx, std::slice::from_ref(&original)).await;
    fx.engine.request_catalog("codex", true).await.unwrap();
    let old = fx.begin(fx.params()).await;
    fx.finish(old, "done").await;
    let slow = Slow::new(&fx, "codex", true, Duration::ZERO);
    slow.revision.store(1, SeqCst);
    reopen(&mut fx, std::slice::from_ref(&slow)).await;
    let new = fx.create(fx.params()).await;
    packet(&fx, new).await;
    fx.finish(new, "done").await;
    fx.engine.follow(old, "continue", false).unwrap();
    fx.finish(old, "done").await;
    assert_eq!(
        fx.shared.calls(|c| Some(c.req.route.model.clone())),
        ["default", "review", "default"]
    );
    let row = fx
        .all(&format!(
            "SELECT r.route FROM native_routes r JOIN jobs j ON r.job=j.id WHERE j.ticket={new}"
        ))
        .pop()
        .unwrap();
    let binding: Value = serde_json::from_str(row["route"].as_str().unwrap()).unwrap();
    assert_eq!(binding["model"], "review");
    assert_eq!(slow.full.load(SeqCst), 1);
    slow.gate.open();
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_unknown_default_waits_for_current_binding() {
    let mut fx = fixture();
    let old = Slow::new(&fx, "codex", false, Duration::ZERO);
    old.gate.open();
    reopen(&mut fx, std::slice::from_ref(&old)).await;
    fx.engine.request_catalog("codex", true).await.unwrap();

    let current = Slow::new(&fx, "codex", false, Duration::ZERO);
    current.revision.store(1, SeqCst);
    reopen(&mut fx, std::slice::from_ref(&current)).await;
    let ticket = fx.begin(fx.params()).await;
    fx.until(|| current.full.load(SeqCst) == 1).await;
    assert_eq!(fx.int("SELECT count(*) FROM native_routes"), 0);
    current.gate.open();
    fx.finish(ticket, "done").await;
    assert_eq!(
        fx.shared.calls(|c| Some(c.req.route.model.clone())),
        ["review"]
    );
    assert_eq!(current.full.load(SeqCst), 1);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refresh_merges_only_failed_unchanged_sources_and_persists_the_menu() {
    let mut old = t1_catalog();
    old["providers"] = json!([
        {"id":"a","models":[{"id":"old-a"}]},
        {"id":"b","models":[{"id":"old-b"}]},
        {"id":"removed","models":[{"id":"gone"}]},
        {"id":"edited","sourceIdentity":"endpoint-1","models":[{"id":"private"}]}
    ]);
    let mut fx = Fx::new(script(|_| async { Ok("done".into()) }), old.clone());
    fx.engine.request_catalog("codex", true).await.unwrap();
    let mut live = old;
    live["default"] = json!({"provider":"b","model":"new-b","mode":"cli"});
    live["providers"] = json!([
        {"id":"a","catalogError":true,"available":false,"models":[]},
        {"id":"b","models":[{"id":"new-b"}]},
        {"id":"edited","sourceIdentity":"endpoint-2","catalogError":true,"models":[]}
    ]);
    fx.shared.set_catalog(catalog_of(live));
    for _ in 0..2 {
        let merged = fx.engine.request_catalog("codex", true).await.unwrap();
        let sources = merged["providers"].as_array().unwrap();
        assert_eq!(sources.len(), 3);
        assert_eq!(sources[0]["models"], json!([{"id":"old-a"}]));
        assert_eq!(sources[0]["available"], true);
        let pick_a = json!({"provider":"a","mode":"cli","model":"old-a"});
        let menu_a = sa_engine::protocol::options(Some(&merged), Some(&pick_a))
            .unwrap()
            .unwrap();
        assert_eq!(menu_a["models"], json!(["old-a"]));
        assert_eq!(sources[1]["models"], json!([{"id":"new-b"}]));
        assert_eq!(sources[2]["models"], json!([]));
    }
    fx.restart(quiet()).await;
    let id = fx.create(fx.params()).await;
    assert_eq!(
        packet(&fx, id).await["delegate_models"]["codex"]["models"],
        json!(["new-b"])
    );
    fx.finish(id, "done").await;
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_empty_default_recovers_on_next_ticket_without_manual_refresh() {
    let mut fx = fixture();
    let lead = Slow::new(&fx, "codex", false, Duration::ZERO);
    reopen(&mut fx, std::slice::from_ref(&lead)).await;
    lead.gate.open();
    let mut incomplete = t1_catalog();
    incomplete["default"]["model"] = json!("");
    incomplete["providers"][0]["models"] = json!([]);
    fx.shared.set_catalog(catalog_of(incomplete));
    let cached = fx.engine.request_catalog("codex", true).await.unwrap();
    assert_eq!(cached["default"]["model"], "");

    fx.shared.set_catalog(catalog_of(t1_catalog()));
    let id = fx.begin(fx.params()).await;
    fx.finish(id, "done").await;
    assert_eq!(
        fx.shared.calls(|c| Some(c.req.route.model.clone())),
        ["default"]
    );
    assert_eq!(lead.full.load(SeqCst), 2);
    assert_eq!(lead.selected.load(SeqCst), 0);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn creation_revalidates_removed_custom_provider_after_revision_change() {
    let mut fx = fixture();
    let lead = Slow::new(&fx, "codex", true, Duration::ZERO);
    reopen(&mut fx, std::slice::from_ref(&lead)).await;
    lead.gate.open();
    let mut catalog = t1_catalog();
    catalog["providers"].as_array_mut().unwrap().push(json!({
        "id":"custom-removed","models":[{"id":"saved-model"}]
    }));
    fx.shared.set_catalog(catalog_of(catalog));
    fx.engine.request_catalog("codex", true).await.unwrap();
    let params = || {
        let mut params = fx.params();
        params.selections = Some(json!({"codex":{
            "provider":"custom-removed","model":"saved-model","mode":"cli"
        }}));
        params
    };
    fx.create(params()).await;
    assert_eq!(lead.selected.load(SeqCst), 0);

    fx.shared.set_catalog(catalog_of(t1_catalog()));
    lead.revision.store(1, SeqCst);
    let error = fx.engine.create(params()).await.unwrap_err().to_string();
    assert!(
        ["codex", "custom-removed", "saved-model"]
            .iter()
            .all(|part| error.contains(*part)),
        "{error}"
    );
    assert_eq!(lead.selected.load(SeqCst), 1);
    assert_eq!(lead.full.load(SeqCst), 1);
    assert_eq!(fx.int("SELECT count(*) FROM tickets"), 1);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn creation_distinguishes_unreadable_list_from_explicitly_disabled_route() {
    let fx = fixture();
    let mut partial = t1_catalog();
    partial["providers"][0]["catalogError"] = json!(true);
    fx.shared.set_catalog(catalog_of(partial));
    for (provider, model, accepted) in [
        ("test-local", "unknown", true),
        ("test-local", "disabled", false),
        ("removed", "unknown", false),
    ] {
        let mut params = fx.params();
        params.selections = Some(json!({"codex":{"provider":provider,"model":model,"mode":"cli"}}));
        assert_eq!(
            fx.engine.create(params).await.is_ok(),
            accepted,
            "{provider}/{model}"
        );
    }
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failing_endpoint_keeps_current_saved_models_selectable_at_creation() {
    let mut fx = fixture();
    let lead = Slow::new(&fx, "codex", true, Duration::ZERO);
    reopen(&mut fx, std::slice::from_ref(&lead)).await;
    lead.gate.open();
    let mut catalog = t1_catalog();
    catalog["providers"][0]["sourceIdentity"] = json!("custom-endpoint");
    catalog["providers"][0]["models"] =
        json!([{"id":"old"},{"id":"shared","reasoningEfforts":["high"]}]);
    fx.shared.set_catalog(catalog_of(catalog.clone()));
    fx.engine.request_catalog("codex", true).await.unwrap();

    lead.revision.store(1, SeqCst);
    let models = json!([{"id":"new"},{"id":"shared","reasoningEfforts":[]}]);
    catalog["providers"][0]["models"] = models;
    catalog["providers"][0]["catalogError"] = json!(true);
    catalog["providers"][0]["defaultModel"] = json!("new");
    catalog["default"]["model"] = json!("new");
    fx.shared.set_catalog(catalog_of(catalog));
    let merged = fx.engine.request_catalog("codex", true).await.unwrap();
    assert_eq!(
        merged["providers"][0]["models"],
        json!([{"id":"new"},{"id":"shared","reasoningEfforts":[]},{"id":"old"}])
    );
    assert_eq!(merged["providers"][0]["defaultModel"], "new");
    assert_eq!(merged["default"]["model"], "new");
    let selection = json!({"provider":"test-local","model":"new","mode":"cli"});
    let menu = sa_engine::protocol::options(Some(&merged), Some(&selection))
        .unwrap()
        .unwrap();
    assert_eq!(menu["models"], json!(["new", "shared", "old"]));
    let mut params = fx.params();
    params.selections = Some(json!({"codex":selection}));
    fx.create(params).await;
    assert_eq!(lead.selected.load(SeqCst), 0);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failover_uses_completed_refresh_for_its_only_provisional_alternative() {
    let release_child = Gate::new();
    let gate = release_child.clone();
    let mut fx = Fx::new(
        script(move |c| {
            let gate = gate.clone();
            async move {
                if c.depth() == 0 && c.turn() == 1 {
                    return Ok(send(json!({})));
                }
                if c.depth() > 0 && c.name == "deepseek" {
                    gate.wait().await;
                    let mut error = AdapterError::new(
                        ErrorClass::RouteUnavailable,
                        "fixture route unavailable",
                    );
                    error.code = Some(sa_engine::resilience::CODE_ROUTE_NOT_CONFIGURED.into());
                    return Err(error);
                }
                Ok("done".into())
            }
        }),
        t1_catalog(),
    );
    let lead = Slow::new(&fx, "codex", true, Duration::ZERO);
    let alternative = Slow::new(&fx, "claude", false, Duration::ZERO);
    let child = Slow::new(&fx, "deepseek", true, Duration::ZERO);
    lead.gate.open();
    child.gate.open();
    reopen(&mut fx, &[lead, alternative.clone(), child]).await;
    // Codex can run its locally configured default, but its published menu
    // confirmed it unavailable for delegation. It comes first in failover order.
    let mut unavailable = t1_catalog();
    unavailable["providers"][0]["available"] = json!(false);
    fx.shared.set_catalog(catalog_of(unavailable));
    fx.engine.request_catalog("codex", true).await.unwrap();
    fx.shared.set_catalog(catalog_of(t1_catalog()));
    let ticket = fx.begin(fx.params().failover(true)).await;
    fx.until(|| fx.shared.count(|c| c.name == "deepseek") == 1)
        .await;
    let menus = fx.shared.calls(|c| {
        (c.req.ticket == ticket && c.packet["depth"] == 0)
            .then(|| c.packet["delegate_models"].clone())
    });
    assert_eq!(menus.len(), 1);
    assert_eq!(menus[0]["codex"]["available"], false);
    assert!(menus[0]["codex"].get("partial").is_none());
    assert_eq!(menus[0]["claude"]["partial"], true);
    assert_eq!(menus[0]["claude"]["available"], false);

    let (cached, refresh) = fx.engine.catalog_updates("claude", false);
    assert!(cached.is_none());
    alternative.gate.open();
    refresh.unwrap().await.unwrap();
    release_child.open();
    fx.finish(ticket, "done").await;
    let replacements = fx.all(
        "SELECT j.agent,j.state,j.result FROM job_failovers f JOIN jobs j ON j.id=f.replacement",
    );
    assert_eq!(replacements.len(), 1);
    assert_eq!(replacements[0]["agent"], "claude");
    assert_eq!(replacements[0]["state"], "done");
    assert_eq!(replacements[0]["result"], "done");
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
