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
async fn continuous_root_pause_uses_finite_retry_budget() {
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
    assert_eq!(fx.shared.count(|_| true), 2);
    assert_eq!(root_state(&fx.engine, ticket), "failed");
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='turn.gave_up'"), 1);
    fx.engine.close().await;
}
