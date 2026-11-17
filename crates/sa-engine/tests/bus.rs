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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn advisor_turns_run_read_only_on_the_lead_route_or_the_chosen_one() {
    use sa_engine::adapter::Route;
    use tokio_util::sync::CancellationToken;
    let fx = Fx::new(
        script(|c| async move {
            if c.req.prompt.starts_with("You are the advisor") {
                return Ok(format!("advice from {} on {}", c.name, c.req.route.model));
            }
            Ok("done".into())
        }),
        t1_catalog(),
    );
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "done").await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    // A running worker of the ticket asks for advice.
    let worker = fx
        .engine
        .db()
        .with(|q| {
            q.run(
                "INSERT INTO jobs(ticket,parent,agent,body,writing,depth,turn,state) VALUES(?,?,'deepseek','w',0,1,1,'running')",
                &[&ticket, &root],
            )?;
            Ok(q.conn().last_insert_rowid())
        })
        .unwrap();
    let brief = "Goal: fix the importer. Tried X, Y fails. Which approach?";
    let advice = fx.engine.consult(ticket, worker, 1, brief, CancellationToken::new()).await.unwrap();
    assert_eq!(advice["advice"], "advice from codex on default");
    assert_eq!(advice["advisor"]["source"], "lead");
    let call = fx.shared.calls(|c| c.req.prompt.starts_with("You are the advisor").then(|| c.req.clone())).pop().unwrap();
    assert_eq!(call.profile, "readonly");
    assert!(!call.writable && call.bus.is_none() && call.session_id.is_none());
    assert!(call.prompt.ends_with(brief));
    // The operator's choice wins over the lead route.
    let route = Route { provider: "test-local".into(), model: "review".into(), mode: "cli".into(), reasoning_effort: None };
    fx.engine.set_advisor(None, Some(("claude", &route))).unwrap();
    let advice = fx.engine.consult(ticket, worker, 1, brief, CancellationToken::new()).await.unwrap();
    assert_eq!(advice["advice"], "advice from claude on review");
    assert_eq!(advice["advisor"]["source"], "global");
    fx.engine.set_advisor(Some(ticket), Some(("deepseek", &route))).unwrap();
    assert_eq!(fx.engine.advisor_for(ticket).unwrap().agent, "deepseek");
    fx.engine.set_advisor(Some(ticket), None).unwrap();
    assert_eq!(fx.engine.advisor_for(ticket).unwrap().source, "global");
    assert!(fx.engine.set_advisor(None, Some(("nobody", &route))).is_err());
    // Both consultations are on the caller's job; an ended turn cannot ask.
    let events = fx.engine.db().with(|q| q.int("SELECT count(*) FROM events WHERE job=? AND kind='advice.result'", &[&worker])).unwrap();
    assert_eq!(events, 2);
    assert!(fx.engine.consult(ticket, worker, 2, brief, CancellationToken::new()).await.is_err());
    assert!(fx.engine.consult(ticket, worker, 1, " ", CancellationToken::new()).await.is_err());
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn advisor_quota_guard_checks_selected_provider_before_start_and_reads_live_setting() {
    use sa_engine::adapter::Route;
    use sa_store::quota::Window;
    use tokio_util::sync::CancellationToken;

    let mut opts = quiet();
    let efficiency = opts.efficiency.clone();
    efficiency.write().unwrap().quota_guard = true;
    let bus = Arc::new(FakeBus::default());
    opts.bus = Some(bus.clone());
    let fx = Fx::with(
        script(|c| async move {
            if c.req.prompt.starts_with(sa_engine::advice::GUIDE) {
                return Ok(format!("advice from {} on {}", c.name, c.req.route.provider));
            }
            Ok("done".into())
        }),
        t1_catalog(),
        opts,
    );
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "done").await;
    fx.until(|| {
        let log = bus.log.lock().unwrap();
        !log.is_empty() && log.iter().filter(|v| v.starts_with("grant")).count()
            == log.iter().filter(|v| v.starts_with("revoke")).count()
    }).await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    let advisor_route = Route {
        provider: "other".into(), model: "foreign".into(), mode: "cli".into(), reasoning_effort: None,
    };
    fx.engine.set_advisor(Some(ticket), Some(("claude", &advisor_route))).unwrap();
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
    // Add the window after the scheduled root finishes, isolating consultation admission.
    let worker = fx.engine.db().with(|q| {
        q.run(
            "INSERT INTO jobs(ticket,parent,agent,body,writing,depth,turn,state) VALUES(?,?,'deepseek','w',0,1,1,'running')",
            &[&ticket, &root],
        )?;
        let worker = q.conn().last_insert_rowid();
        let caller_route = serde_json::json!({"provider":"test-local","model":"default","mode":"cli"}).to_string();
        q.run("INSERT INTO native_routes(job,route) VALUES(?,?)", &[&worker, &caller_route])?;
        sa_store::quota::upsert(q.conn(), "claude", "other", &Window {
            kind: "five_hour".into(), used: Some(1.0), resets_at: Some(at + 3600.0),
            exhausted: true, minutes: Some(300),
        }, at)?;
        Ok(worker)
    }).unwrap();
    let stored = fx.all("SELECT * FROM quota_windows");
    let bus_before = bus.log.lock().unwrap().clone();
    let brief = "Which importer recovery should the worker use?";
    let advisor_calls = || fx.shared.count(|c| c.req.prompt.starts_with(sa_engine::advice::GUIDE));
    let advice_events = |kind: &str| fx.engine.db().with(|q| q.int(
        "SELECT count(*) FROM events WHERE job=? AND kind=?", &[&worker, &kind],
    )).unwrap();

    let error = fx.engine.consult(ticket, worker, 1, brief, CancellationToken::new()).await.unwrap_err();
    assert!(error.to_string().starts_with("顾问所在供应商的订阅额度窗口已用尽"));
    assert_eq!(advisor_calls(), 0);
    assert_eq!(advice_events("advice.started"), 0);
    assert_eq!(advice_events("advice.result"), 0);
    assert_eq!(fx.all("SELECT * FROM quota_windows"), stored);

    // The same engine observes the operator's updated setting without clearing quota.
    efficiency.write().unwrap().quota_guard = false;
    let advice = fx.engine.consult(ticket, worker, 1, brief, CancellationToken::new()).await.unwrap();
    assert_eq!(advice["advice"], "advice from claude on other");
    assert_eq!(advice["advisor"]["source"], "ticket");
    assert_eq!(advice["advisor"]["agent"], "claude");
    assert_eq!(advice["advisor"]["provider"], "other");
    assert_eq!(advisor_calls(), 1);
    assert_eq!(advice_events("advice.started"), 1);
    assert_eq!(advice_events("advice.result"), 1);
    let call = fx.shared.calls(|c| c.req.prompt.starts_with(sa_engine::advice::GUIDE).then(|| c.req.clone())).pop().unwrap();
    assert_eq!(call.agent, "claude");
    assert_eq!(call.route, advisor_route);
    assert_eq!(call.profile, "readonly");
    assert!(!call.writable && call.session_id.is_none() && call.bus.is_none());
    assert!(call.prompt.ends_with(brief));
    assert_eq!(fx.all("SELECT * FROM quota_windows"), stored);
    assert_eq!(*bus.log.lock().unwrap(), bus_before);

    efficiency.write().unwrap().quota_guard = true;
    assert!(fx.engine.consult(ticket, worker, 1, brief, CancellationToken::new()).await.is_err());
    assert_eq!(advisor_calls(), 1);
    assert_eq!(advice_events("advice.started"), 1);
    fx.engine.close().await;
}
