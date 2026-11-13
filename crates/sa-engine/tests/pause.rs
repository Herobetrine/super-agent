mod common;

use common::*;
use sa_engine::{Engine, PauseStatus};
use serde_json::json;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::Duration;

async fn settled(engine: &Engine) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while engine.active_count() != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_root_finishes_without_cancellation_and_follow_requires_explicit_resume() {
    let gate = Gate::new();
    let cancelled = Arc::new(AtomicBool::new(false));
    let fx = Fx::new(script({
        let gate = gate.clone();
        let cancelled = cancelled.clone();
        move |c| {
            let gate = gate.clone();
            let cancelled = cancelled.clone();
            async move {
                gate.wait().await;
                cancelled.store(c.cancel.is_cancelled(), Ordering::SeqCst);
                Ok("handoff evidence".into())
            }
        }
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    fx.until(|| fx.shared.count(|_| true) == 1).await;
    assert_eq!(fx.engine.request_pause_after_handoff(ticket).unwrap(), PauseStatus::Handoff);
    assert_eq!(fx.engine.request_pause_after_handoff(ticket).unwrap(), PauseStatus::Handoff);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='ticket.pause_requested'"), 1);
    gate.open();
    fx.until(|| fx.engine.pause_status(ticket).unwrap() == PauseStatus::Paused).await;
    settled(&fx.engine).await;
    assert!(!cancelled.load(Ordering::SeqCst));
    assert_eq!(fx.engine.root(ticket).unwrap()["result"], "handoff evidence");
    fx.engine.follow(ticket, "additional task", true).unwrap();
    assert_eq!(fx.engine.step(), 0);
    assert_eq!(fx.shared.count(|_| true), 1);
    assert_eq!(fx.engine.resume_paused(ticket).unwrap(), PauseStatus::Running);
    assert_eq!(fx.engine.resume_paused(ticket).unwrap(), PauseStatus::Running);
    assert_eq!(fx.engine.pause_status(ticket).unwrap(), PauseStatus::Running);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='ticket.resumed'"), 1);
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    let notes = fx.engine.notes(root).unwrap();
    assert_eq!(notes.iter().filter(|note| note["body"].as_str().is_some_and(|body| body.contains("用户已明确恢复工单"))).count(), 1);
    fx.until(|| fx.shared.count(|_| true) == 2).await;
    fx.finish(ticket, "done").await;
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waiting_root_drains_children_and_gets_exactly_one_consolidation() {
    let child_gate = Gate::new();
    let fx = Fx::new(script({
        let child_gate = child_gate.clone();
        move |c| {
            let child_gate = child_gate.clone();
            async move {
                if c.depth() > 0 {
                    child_gate.wait().await;
                    assert!(!c.cancel.is_cancelled());
                    return Ok("child receipt".into());
                }
                if c.turn() == 1 {
                    return Ok(send(json!({})));
                }
                assert_eq!(c.packet["pause_after_handoff"]["consolidation"], true);
                assert_eq!(c.full_packet()["reports"][0]["result"], "child receipt");
                // New assignments must be rejected without losing this receipt.
                Ok(format!("{}\nfinal handoff", send(json!({"id":"forbidden"}))))
            }
        }
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    fx.until(|| root_state(&fx.engine, ticket) == "waiting" && fx.shared.count(|c| c.packet["depth"] == 1) == 1).await;
    assert_eq!(fx.engine.request_pause_after_handoff(ticket).unwrap(), PauseStatus::Draining);
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    fx.engine.note(ticket, root, "wake-up must stay behind the gate").unwrap();
    for _ in 0..4 { fx.engine.step(); }
    assert_eq!(fx.shared.count(|c| c.packet["depth"] == 0), 1);
    child_gate.open();
    fx.until(|| fx.engine.pause_status(ticket).unwrap() == PauseStatus::Paused).await;
    settled(&fx.engine).await;
    for _ in 0..4 { fx.engine.step(); }
    assert_eq!(fx.shared.count(|c| c.packet["depth"] == 0), 2);
    assert_eq!(fx.int("SELECT count(*) FROM jobs WHERE parent IS NOT NULL"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='ticket.handoff_started'"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='ticket.paused'"), 1);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_requested_during_root_allows_its_already_planned_children_to_drain() {
    let root_gate = Gate::new();
    let child_gate = Gate::new();
    let fx = Fx::new(script({
        let root_gate = root_gate.clone();
        let child_gate = child_gate.clone();
        move |c| {
            let root_gate = root_gate.clone();
            let child_gate = child_gate.clone();
            async move {
                if c.depth() > 0 {
                    child_gate.wait().await;
                    return Ok("child done".into());
                }
                if c.turn() == 1 {
                    root_gate.wait().await;
                    return Ok(send(json!({})));
                }
                assert_eq!(c.packet["pause_after_handoff"]["consolidation"], true);
                Ok("handoff".into())
            }
        }
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    fx.until(|| fx.shared.count(|_| true) == 1).await;
    fx.engine.request_pause_after_handoff(ticket).unwrap();
    root_gate.open();
    fx.until(|| fx.engine.pause_status(ticket).unwrap() == PauseStatus::Draining && fx.shared.count(|c| c.packet["depth"] == 1) == 1).await;
    child_gate.open();
    fx.until(|| fx.engine.pause_status(ticket).unwrap() == PauseStatus::Paused).await;
    assert_eq!(fx.shared.count(|_| true), 3);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paused_gate_survives_restart_and_does_not_starve_other_tickets() {
    let mut fx = Fx::new(script(|_| async { Ok("done".into()) }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    fx.finish(ticket, "done").await;
    settled(&fx.engine).await;
    assert_eq!(fx.engine.request_pause_after_handoff(ticket).unwrap(), PauseStatus::Paused);
    fx.engine.follow(ticket, "queued but paused", false).unwrap();
    fx.restart(quiet()).await;
    assert_eq!(fx.engine.pause_status(ticket).unwrap(), PauseStatus::Paused);
    fx.engine.start().unwrap();
    let other = fx.create(fx.params()).await;
    fx.finish(other, "done").await;
    assert_eq!(fx.shared.count(|c| c.packet["ticket"] == ticket), 1);
    assert_eq!(fx.shared.count(|c| c.packet["ticket"] == other), 1);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_handoff_is_paused_with_original_failure_preserved() {
    let gate = Gate::new();
    let fx = Fx::new(script({
        let gate = gate.clone();
        move |_| {
            let gate = gate.clone();
            async move {
                gate.wait().await;
                Err(sa_engine::adapter::AdapterError::new(sa_engine::adapter::ErrorClass::Permission, "fixture denied"))
            }
        }
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    fx.until(|| fx.shared.count(|_| true) == 1).await;
    fx.engine.request_pause_after_handoff(ticket).unwrap();
    gate.open();
    fx.until(|| fx.engine.pause_status(ticket).unwrap() == PauseStatus::Paused).await;
    assert_eq!(root_state(&fx.engine, ticket), "failed");
    assert_eq!(fx.engine.root(ticket).unwrap()["result"], "fixture denied");
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn draining_request_survives_restart_and_consolidates_interrupted_child() {
    let mut fx = Fx::new(script(|c| async move {
        if c.depth() > 0 {
            c.cancel.cancelled().await;
            return Err(sa_engine::adapter::AdapterError::new(sa_engine::adapter::ErrorClass::Cancelled, "fixture stopped"));
        }
        if c.turn() == 1 {
            return Ok(send(json!({})));
        }
        assert_eq!(c.packet["pause_after_handoff"]["consolidation"], true);
        assert_eq!(c.full_packet()["reports"][0]["state"], "interrupted");
        Ok("restart handoff with unresolved child".into())
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    fx.until(|| root_state(&fx.engine, ticket) == "waiting" && fx.shared.count(|c| c.packet["depth"] == 1) == 1).await;
    fx.engine.request_pause_after_handoff(ticket).unwrap();
    fx.restart(quiet()).await;
    assert_eq!(fx.engine.pause_status(ticket).unwrap(), PauseStatus::Draining);
    fx.engine.start().unwrap();
    fx.until(|| fx.engine.pause_status(ticket).unwrap() == PauseStatus::Paused).await;
    assert_eq!(fx.shared.count(|c| c.packet["depth"] == 0), 2);
    assert_eq!(fx.shared.count(|c| c.packet["depth"] == 1), 1);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continuous_root_pause_stops_at_first_failure_boundary() {
    use sa_engine::resilience::{Resilience, ResilienceConfig, ResilienceOptions};
    use sa_engine::runner::ResilientRunner;

    let gate = Gate::new();
    let mut options = ResilienceOptions::default();
    options.limits.transient = 1;
    let config = ResilienceConfig {
        retry_base: Duration::from_millis(5),
        retry_cap: Duration::from_millis(5),
        breaker_threshold: 100,
        ..Default::default()
    };
    let runner = ResilientRunner::with_resilience(Resilience::with_options(config, options));
    let fx = Fx::with(script({
        let gate = gate.clone();
        move |_| {
            let gate = gate.clone();
            async move {
                gate.wait().await;
                Err(sa_engine::adapter::AdapterError::new(sa_engine::adapter::ErrorClass::Network, "fixture network failure"))
            }
        }
    }), t1_catalog(), sa_engine::EngineOptions { runner: Arc::new(runner), ..quiet() });
    let ticket = fx.begin(fx.params().profile("auto")).await;
    fx.until(|| fx.shared.count(|_| true) == 1).await;
    fx.engine.request_pause_after_handoff(ticket).unwrap();
    gate.open();
    fx.until(|| fx.engine.pause_status(ticket).unwrap() == PauseStatus::Paused).await;
    assert_eq!(fx.shared.count(|_| true), 1);
    assert_eq!(root_state(&fx.engine, ticket), "failed");
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='turn.pause_failure'"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='turn.retry_scheduled'"), 0);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_root_resume_keeps_lifecycle_and_delivers_revocation_on_follow() {
    for terminal in ["done", "cancelled", "failed"] {
        let mut fx = Fx::new(script(|c| async move {
            if c.turn() > 1 {
                assert_eq!(c.full_packet()["pause_control"]["state"], "running");
                assert!(c.full_packet()["pause_control"]["instruction"].as_str().unwrap().contains("之前的小结交接后暂停指令已撤销"));
            }
            Ok("terminal fixture".into())
        }), t1_catalog());
        let ticket = fx.begin(fx.params()).await;
        fx.finish(ticket, "done").await;
        settled(&fx.engine).await;
        // Fixtures cover each terminal lifecycle without invoking a model.
        fx.engine.db().tx(|q| {
            q.run("UPDATE jobs SET state=? WHERE ticket=? AND parent IS NULL", &[&terminal, &ticket])?;
            Ok(())
        }).unwrap();
        let before = fx.engine.root(ticket).unwrap();
        assert_eq!(fx.engine.request_pause_after_handoff(ticket).unwrap(), PauseStatus::Paused);
        assert_eq!(fx.engine.resume_paused(ticket).unwrap(), PauseStatus::Running);
        assert_eq!(fx.engine.resume_paused(ticket).unwrap(), PauseStatus::Running);
        assert_eq!(fx.engine.root(ticket).unwrap(), before);
        assert_eq!(fx.engine.step(), 0);
        assert_eq!(fx.shared.count(|_| true), 1);
        assert_eq!(fx.int("SELECT count(*) FROM mailbox"), 0);
        assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='user.follow'"), 0);
        assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='ticket.resumed'"), 1);
        let plan = fx.all(&format!("EXPLAIN QUERY PLAN SELECT payload FROM events WHERE ticket={ticket} AND kind='ticket.resumed' ORDER BY seq DESC LIMIT 1"));
        assert!(plan.iter().any(|row| row["detail"].as_str().is_some_and(|detail| detail.contains("events_ticket_kind"))));
        fx.restart(quiet()).await;
        fx.engine.follow(ticket, "explicit next task", true).unwrap();
        fx.engine.start().unwrap();
        fx.until(|| fx.shared.count(|_| true) == 2).await;
        fx.finish(ticket, "done").await;
        fx.engine.close().await;
    }
}

struct RecoveryFixture {
    decision: sa_engine::resilience::Recovery,
    blocked: bool,
}

impl sa_engine::runner::TurnRunner for RecoveryFixture {
    fn run_turn(
        &self,
        adapter: Arc<dyn sa_engine::adapter::Adapter>,
        request: sa_engine::adapter::RunRequest,
        host: Arc<dyn sa_engine::adapter::TurnHost>,
        forward: tokio::sync::mpsc::UnboundedSender<sa_engine::adapter::AdapterEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> futures::future::BoxFuture<'static, sa_engine::resilience::TurnResult> {
        sa_engine::runner::TurnRunner::run_turn(
            &sa_engine::runner::PassThroughRunner::default(), adapter, request, host, forward, cancel,
        )
    }

    fn decide(
        &self,
        _ledger: &mut sa_engine::resilience::TurnLedger,
        _agent: &str,
        _route: &sa_engine::adapter::Route,
        _error: &sa_engine::adapter::AdapterError,
        _rotation: &[sa_engine::adapter::Route],
        _now_secs: f64,
    ) -> Option<sa_engine::resilience::Recovery> {
        Some(self.decision.clone())
    }

    fn admission_blocked_until(&self, _agent: &str, _route: &sa_engine::adapter::Route, time: f64) -> Option<f64> {
        self.blocked.then_some(time + 3600.0)
    }
}

fn recovery_fixtures() -> Vec<sa_engine::resilience::Recovery> {
    use sa_engine::resilience::Recovery;
    vec![
        Recovery::Retry { delay: Duration::from_secs(3600), attempt: 1, max: 5, resume_session: None },
        Recovery::Rotate {
            to: sa_engine::adapter::Route { provider: "other".into(), model: "foreign".into(), mode: "cli".into(), reasoning_effort: None },
            delay: Duration::from_secs(3600), attempt: 1, reason: sa_engine::adapter::ErrorClass::Overloaded,
        },
        Recovery::WaitForRoute { until_secs: 4_000_000_000.0 },
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requested_pause_preempts_all_recovery_decisions() {
    for decision in recovery_fixtures() {
        let gate = Gate::new();
        let fx = Fx::with(script({
            let gate = gate.clone();
            move |_| {
                let gate = gate.clone();
                async move {
                    gate.wait().await;
                    Err(sa_engine::resilience::classify::from_http_status(503, None))
                }
            }
        }), t1_catalog(), sa_engine::EngineOptions {
            runner: Arc::new(RecoveryFixture { decision, blocked: false }), ..quiet()
        });
        let ticket = fx.begin(fx.params().profile("auto")).await;
        fx.until(|| fx.shared.count(|_| true) == 1).await;
        fx.engine.request_pause_after_handoff(ticket).unwrap();
        gate.open();
        fx.until(|| fx.engine.pause_status(ticket).unwrap() == PauseStatus::Paused).await;
        settled(&fx.engine).await;
        assert_eq!(fx.shared.count(|_| true), 1);
        assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind IN ('turn.retry_scheduled','route.rotated','turn.route_wait','auto.retry')"), 0);
        assert_eq!(fx.all("SELECT payload FROM events WHERE kind='turn.pause_error'").len(), 1);
        let error: serde_json::Value = serde_json::from_str(fx.all("SELECT payload FROM events WHERE kind='turn.pause_error'")[0]["payload"].as_str().unwrap()).unwrap();
        assert_eq!(error["error"]["class"], "overloaded");
        assert_eq!(error["error"]["http_status"], 503);
        assert_eq!(root_state(&fx.engine, ticket), "failed");
        fx.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_recovery_pause_survives_restart_without_new_admission() {
    for decision in recovery_fixtures() {
        let mut fx = Fx::with(script(|_| async {
            Err(sa_engine::adapter::AdapterError::new(sa_engine::adapter::ErrorClass::Overloaded, "original 503 fixture"))
        }), t1_catalog(), sa_engine::EngineOptions {
            runner: Arc::new(RecoveryFixture { decision, blocked: false }), ..quiet()
        });
        let ticket = fx.begin(fx.params().profile("auto")).await;
        fx.until(|| fx.int("SELECT count(*) FROM events WHERE kind IN ('turn.retry_scheduled','route.rotated','turn.route_wait')") == 1).await;
        settled(&fx.engine).await;
        assert_eq!(root_state(&fx.engine, ticket), "queued");
        assert_eq!(fx.engine.request_pause_after_handoff(ticket).unwrap(), PauseStatus::Paused);
        assert_eq!(fx.engine.root(ticket).unwrap()["result"], "original 503 fixture");
        fx.restart(quiet()).await;
        fx.engine.start().unwrap();
        for _ in 0..3 { fx.engine.step(); }
        assert_eq!(fx.shared.count(|_| true), 1);
        assert_eq!(fx.int("SELECT auto_cycles FROM tickets"), 1);
        assert_eq!(fx.engine.pause_status(ticket).unwrap(), PauseStatus::Paused);
        assert_eq!(root_state(&fx.engine, ticket), "failed");
        fx.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordinary_initial_queue_gets_one_handoff_but_deferred_route_pauses() {
    let fx = Fx::new(script(|_| async { Ok("first handoff".into()) }), t1_catalog());
    let ticket = fx.create(fx.params()).await;
    assert_eq!(fx.engine.request_pause_after_handoff(ticket).unwrap(), PauseStatus::Handoff);
    fx.engine.start().unwrap();
    fx.until(|| fx.engine.pause_status(ticket).unwrap() == PauseStatus::Paused).await;
    assert_eq!(fx.shared.count(|_| true), 1);
    fx.engine.close().await;

    for pause_before_defer in [false, true] {
        let fx = Fx::with(script(|_| async { panic!("blocked route must not run") }), t1_catalog(), sa_engine::EngineOptions {
            runner: Arc::new(RecoveryFixture { decision: recovery_fixtures().remove(0), blocked: true }), ..quiet()
        });
        let ticket = fx.create(fx.params().profile("auto")).await;
        // A persisted native route makes the breaker visible before admission.
        let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
        fx.engine.db().tx(|q| {
            q.run("INSERT INTO native_routes VALUES(?,?)", &[&root, &json!({"provider":"test-local","model":"default","mode":"cli"}).to_string()])?;
            Ok(())
        }).unwrap();
        if pause_before_defer { fx.engine.request_pause_after_handoff(ticket).unwrap(); }
        fx.engine.start().unwrap();
        if !pause_before_defer {
            fx.until(|| fx.int("SELECT count(*) FROM events WHERE kind='turn.deferred'") == 1).await;
            assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='turn.deferred'"), 1);
            fx.engine.request_pause_after_handoff(ticket).unwrap();
        } else {
            fx.until(|| fx.engine.pause_status(ticket).unwrap() == PauseStatus::Paused).await;
        }
        assert_eq!(fx.engine.pause_status(ticket).unwrap(), PauseStatus::Paused);
        assert_eq!(fx.int("SELECT auto_cycles FROM tickets"), 0);
        assert_eq!(fx.shared.count(|_| true), 0);
        fx.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_root_drains_live_child_then_failed_consolidation_is_not_retried() {
    let root_gate = Gate::new();
    let child_gate = Gate::new();
    let fx = Fx::new(script({
        let root_gate = root_gate.clone();
        let child_gate = child_gate.clone();
        move |c| {
            let root_gate = root_gate.clone();
            let child_gate = child_gate.clone();
            async move {
                if c.depth() > 0 {
                    child_gate.wait().await;
                    assert!(!c.cancel.is_cancelled());
                    return Ok("natural child receipt".into());
                }
                if c.turn() == 1 {
                    c.emit("delta", json!(format!("{}\n", send(json!({})))));
                    root_gate.wait().await;
                } else {
                    assert_eq!(c.full_packet()["reports"][0]["result"], "natural child receipt");
                    assert_eq!(c.packet["pause_after_handoff"]["consolidation"], true);
                }
                Err(sa_engine::resilience::classify::from_http_status(503, None))
            }
        }
    }), t1_catalog());
    let ticket = fx.begin(fx.params().profile("auto")).await;
    fx.until(|| fx.shared.count(|c| c.packet["depth"] == 1) == 1).await;
    fx.engine.request_pause_after_handoff(ticket).unwrap();
    root_gate.open();
    fx.until(|| fx.engine.pause_status(ticket).unwrap() == PauseStatus::Draining).await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    fx.engine.note(ticket, root, "keep draining after failed boundary").unwrap();
    for _ in 0..4 { fx.engine.step(); }
    assert_eq!(fx.shared.count(|c| c.packet["depth"] == 0), 1);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='turn.pause_failure'"), 1);
    child_gate.open();
    fx.until(|| fx.engine.pause_status(ticket).unwrap() == PauseStatus::Paused).await;
    settled(&fx.engine).await;
    assert_eq!(fx.shared.count(|c| c.packet["depth"] == 0), 2);
    assert_eq!(fx.shared.count(|c| c.packet["depth"] == 1), 1);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='turn.pause_failure'"), 2);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='turn.retry_scheduled'"), 0);
    assert_eq!(root_state(&fx.engine, ticket), "failed");
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn persisted_handoff_reconciles_recovery_queue_before_admission() {
    for kind in ["turn.retry_scheduled", "route.rotated", "turn.route_wait", "turn.deferred", "auto.retry"] {
        let mut fx = Fx::new(script(|_| async { panic!("persisted recovery must not run") }), t1_catalog());
        let ticket = fx.create(fx.params().profile("auto")).await;
        let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
        fx.engine.db().tx(|q| {
            q.run("UPDATE jobs SET turn=2,ready_after=4000000000 WHERE id=?", &[&root])?;
            q.log(sa_engine::db::Who { ticket, id: root, turn: 1 }, kind, &json!({"reason":"persisted failure"}))?;
            q.run("INSERT INTO ticket_pause VALUES(?,'handoff',0)", &[&ticket])?;
            Ok(())
        }).unwrap();
        fx.restart(quiet()).await;
        fx.engine.start().unwrap();
        fx.until(|| fx.engine.pause_status(ticket).unwrap() == PauseStatus::Paused).await;
        assert_eq!(fx.shared.count(|_| true), 0);
        assert_eq!(fx.int("SELECT auto_cycles FROM tickets"), 0);
        assert_eq!(root_state(&fx.engine, ticket), "failed");
        fx.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_user_directed_recheck_is_allowed_one_handoff() {
    let fx = Fx::new(script(|_| async { Ok("user requested handoff".into()) }), t1_catalog());
    let ticket = fx.create(fx.params()).await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    fx.engine.db().tx(|q| {
        q.run("UPDATE jobs SET turn=2 WHERE id=?", &[&root])?;
        q.log(sa_engine::db::Who { ticket, id: root, turn: 1 }, "auto.retry", &json!({"reason":"按新追加的要求重新核对。","after_seconds":0}))?;
        Ok(())
    }).unwrap();
    assert_eq!(fx.engine.request_pause_after_handoff(ticket).unwrap(), PauseStatus::Handoff);
    fx.engine.start().unwrap();
    fx.until(|| fx.engine.pause_status(ticket).unwrap() == PauseStatus::Paused).await;
    assert_eq!(fx.shared.count(|_| true), 1);
    assert_eq!(root_state(&fx.engine, ticket), "done");
    fx.engine.close().await;
}
