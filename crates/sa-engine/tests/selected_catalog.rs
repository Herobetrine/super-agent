mod common;

use common::*;
use futures::future::BoxFuture;
use sa_engine::Engine;
use sa_engine::adapter::{
    Adapter, AdapterError, Catalog, EventSink, Route, RunOutcome, RunRequest, TurnHost,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

type ProbeLog = Arc<Mutex<Vec<(String, Option<Route>)>>>;

struct Counted {
    inner: Arc<dyn Adapter>,
    probes: ProbeLog,
    reject_selected: bool,
}

impl Adapter for Counted {
    fn agent(&self) -> &'static str {
        self.inner.agent()
    }

    fn catalog(&self) -> BoxFuture<'static, anyhow::Result<Catalog>> {
        self.probes
            .lock()
            .unwrap()
            .push((self.agent().into(), None));
        self.inner.catalog()
    }

    fn catalog_for_route(
        &self,
        route: Route,
    ) -> Option<BoxFuture<'static, anyhow::Result<Catalog>>> {
        self.probes
            .lock()
            .unwrap()
            .push((self.agent().into(), Some(route)));
        let reject = self.reject_selected;
        let future = self.inner.catalog();
        Some(Box::pin(async move {
            let mut catalog = future.await?;
            if reject {
                catalog.value["providers"][0]["models"] = json!([]);
            }
            Ok(catalog)
        }))
    }

    fn run(
        &self,
        request: RunRequest,
        host: Arc<dyn TurnHost>,
        events: EventSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<RunOutcome, AdapterError>> {
        self.inner.run(request, host, events, cancel)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admission_uses_selected_catalog_only_for_complete_collaborator_routes() {
    for (explicit, reject) in [(true, false), (false, false), (true, true)] {
        let mut fx = Fx::new(
            script(move |c| async move {
                if c.depth() == 0 && c.turn() == 1 {
                    return Ok(send(if explicit {
                        json!({"model":"review"})
                    } else {
                        json!({})
                    }));
                }
                Ok("verified".into())
            }),
            t1_catalog(),
        );
        fx.engine.close().await;
        let probes = Arc::new(Mutex::new(Vec::new()));
        let counted = adapters(&fx.shared)
            .into_iter()
            .map(|inner| {
                // Only the collaborator's route is rejected; ticket creation
                // verifies the lead's explicit codex route against its catalog.
                let reject_selected = reject && inner.agent() == "deepseek";
                Arc::new(Counted {
                    inner,
                    probes: probes.clone(),
                    reject_selected,
                }) as Arc<dyn Adapter>
            })
            .collect();
        fx.engine = Engine::open(&fx.data, counted, quiet()).unwrap();
        let mut params = fx.params();
        params.selections =
            Some(json!({"codex":{"provider":"test-local", "model":"default", "mode":"cli"}}));
        let ticket = fx.begin(params).await;
        fx.finish(ticket, if reject { "partial" } else { "done" })
            .await;
        let probes = probes.lock().unwrap();
        let created: Vec<_> = probes
            .iter()
            .filter(|(name, route)| name == "codex" && route.is_some())
            .collect();
        assert_eq!(created.len(), 1, "creation verifies the explicit route once");
        let selected: Vec<_> = probes
            .iter()
            .filter(|(name, _)| name != "codex")
            .filter_map(|(name, route)| route.as_ref().map(|r| (name, r)))
            .collect();
        assert_eq!(selected.len(), usize::from(explicit));
        if explicit {
            assert_eq!(selected[0].0, "deepseek");
            assert_eq!(selected[0].1.model, "review");
        }
        let root_turns = fx.shared.count(|c| c.packet["depth"] == 0);
        for agent in sa_engine::adapter::AGENTS {
            let full_count = probes
                .iter()
                .filter(|(name, route)| name == agent && route.is_none())
                .count();
            assert_eq!(
                full_count,
                root_turns + usize::from(agent == "deepseek" && !explicit)
            );
        }
        assert_eq!(
            fx.shared.count(|c| c.packet["depth"] == 1),
            usize::from(!reject)
        );
        if reject {
            assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='route.requested' AND job IN (SELECT id FROM jobs WHERE parent IS NOT NULL)"), 0);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bound_collaborator_route_is_revalidated_on_its_next_turn() {
    let mut fx = Fx::new(
        script(|c| async move {
            if c.depth() == 0 && c.turn() == 1 {
                return Ok(send(json!({"model":"review"})));
            }
            if c.depth() == 1 && c.turn() == 1 {
                return Ok(send(json!({"to":"claude"})));
            }
            Ok("verified".into())
        }),
        t1_catalog(),
    );
    fx.engine.close().await;
    let probes = Arc::new(Mutex::new(Vec::new()));
    let counted = adapters(&fx.shared)
        .into_iter()
        .map(|inner| {
            Arc::new(Counted {
                inner,
                probes: probes.clone(),
                reject_selected: false,
            }) as Arc<dyn Adapter>
        })
        .collect();
    fx.engine = Engine::open(&fx.data, counted, quiet()).unwrap();
    let ticket = fx.begin(fx.params()).await;
    fx.finish(ticket, "done").await;
    assert_eq!(fx.shared.count(|c| c.packet["depth"] == 1), 2);
    let probes = probes.lock().unwrap();
    let selected: Vec<_> = probes
        .iter()
        .filter(|(name, route)| name == "deepseek" && route.is_some())
        .collect();
    assert_eq!(selected.len(), 2);
    assert!(
        selected
            .iter()
            .all(|(_, route)| route.as_ref().unwrap().model == "review")
    );
}

/// A ticket-configured route missing from one live probe (a gateway timing out)
/// is retried with a fresh probe instead of failing permanently.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn configured_route_missing_from_one_probe_is_retried() {
    struct Flaky {
        inner: Arc<dyn Adapter>,
        misses: Arc<Mutex<u32>>,
    }
    impl Adapter for Flaky {
        fn agent(&self) -> &'static str {
            self.inner.agent()
        }
        fn catalog(&self) -> BoxFuture<'static, anyhow::Result<Catalog>> {
            self.inner.catalog()
        }
        fn catalog_for_route(&self, _route: Route) -> Option<BoxFuture<'static, anyhow::Result<Catalog>>> {
            let miss = {
                let mut left = self.misses.lock().unwrap();
                let miss = self.agent() == "deepseek" && *left > 0;
                if miss { *left -= 1; }
                miss
            };
            let future = self.inner.catalog();
            Some(Box::pin(async move {
                let mut catalog = future.await?;
                if miss {
                    catalog.value["providers"][0]["models"] = json!([]);
                }
                Ok(catalog)
            }))
        }
        fn run(&self, request: RunRequest, host: Arc<dyn TurnHost>, events: EventSink, cancel: CancellationToken)
            -> BoxFuture<'static, Result<RunOutcome, AdapterError>> {
            self.inner.run(request, host, events, cancel)
        }
    }
    let mut fx = Fx::new(
        script(|c| async move {
            if c.depth() == 0 && c.turn() == 1 {
                return Ok(send(json!({})));
            }
            Ok("verified".into())
        }),
        t1_catalog(),
    );
    fx.engine.close().await;
    let misses = Arc::new(Mutex::new(0));
    let flaky = adapters(&fx.shared)
        .into_iter()
        .map(|inner| Arc::new(Flaky { inner, misses: misses.clone() }) as Arc<dyn Adapter>)
        .collect();
    fx.engine = Engine::open(&fx.data, flaky, quiet()).unwrap();
    let mut params = fx.params();
    params.selections = Some(json!({"deepseek":{"provider":"test-local", "model":"review", "mode":"cli"}}));
    // Creation verifies the route; the collaborator's first probe then misses it.
    let ticket = fx.create(params).await;
    *misses.lock().unwrap() = 1;
    fx.engine.start().unwrap();
    fx.until(|| fx.int("SELECT count(*) FROM events WHERE kind='turn.retry_scheduled'") == 1).await;
    let _ = ticket;
    assert_eq!(*misses.lock().unwrap(), 0, "the missing probe happened");
    let retry = fx.all("SELECT payload FROM events WHERE kind='turn.retry_scheduled'");
    assert!(retry[0]["payload"].as_str().unwrap().contains("route:probe_failed"), "{retry:?}");
    assert_eq!(fx.int("SELECT count(*) FROM jobs WHERE state='failed'"), 0);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='turn.gave_up'"), 0);
    fx.engine.close().await;
}
