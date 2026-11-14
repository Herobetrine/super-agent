mod common;

use common::*;
use sa_engine::adapter::BusGrant;
use sa_engine::services::Bus;
use std::sync::{Arc, Mutex};

/// Records grants and revocations; tokens are `t<job>`.
#[derive(Default)]
struct FakeBus {
    log: Mutex<Vec<String>>,
}

impl Bus for FakeBus {
    fn grant(&self, ticket: i64, job: i64, turn: i64, agent: &str) -> Option<BusGrant> {
        self.log.lock().unwrap().push(format!("grant {ticket}/{job}/{turn}/{agent}"));
        Some(BusGrant { url: "http://127.0.0.1:1/mcp".into(), token: format!("t{job}") })
    }
    fn revoke(&self, token: &str) {
        self.log.lock().unwrap().push(format!("revoke {token}"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turns_get_a_bus_grant_and_full_profiles_get_the_guide() {
    let bus = Arc::new(FakeBus::default());
    let mut opts = quiet();
    opts.bus = Some(bus.clone());
    let fx = Fx::with(script(|_| async { Ok("done".into()) }), t1_catalog(), opts);
    let full = fx.begin(fx.params().profile("full")).await;
    fx.finish(full, "done").await;
    let readonly = fx.create(fx.params().profile("readonly")).await;
    fx.finish(readonly, "done").await;
    let calls = fx.shared.calls(|c| Some((c.req.profile.clone(), c.req.bus.clone(), c.req.prompt.clone())));
    let (_, grant, prompt) = calls.iter().find(|c| c.0 == "full").unwrap();
    assert_eq!(grant.as_ref().unwrap().token, format!("t{}", fx.engine.root(full).unwrap()["id"]));
    assert!(prompt.contains("sa_bus MCP tools"), "full-access turns carry the bus guide");
    let (_, _, prompt) = calls.iter().find(|c| c.0 == "readonly").unwrap();
    assert!(!prompt.contains("sa_bus MCP tools"), "adapters do not mount the bus on read-only turns");
    let log = bus.log.lock().unwrap().clone();
    let grants = log.iter().filter(|l| l.starts_with("grant")).count();
    let revokes = log.iter().filter(|l| l.starts_with("revoke")).count();
    assert_eq!(grants, revokes, "every turn's token is revoked when the turn ends: {log:?}");
    fx.engine.close().await;
}
