//! Engine-level resilience (spec §9.2 D-F and I): classified retries for
//! every job, session resume, parent notice, give-up reports, provider
//! rotation, stall recovery, breaker waits and the legacy pass-through rule.

mod common;

use common::*;
use sa_engine::adapter::{AdapterError, AdapterEvent, ErrorClass, Route};
use sa_engine::resilience::{Resilience, ResilienceConfig, ResilienceOptions, classify};
use sa_engine::runner::{PassThroughRunner, ResilientRunner, StaticRotation};
use sa_engine::{CreateParams, EngineOptions};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn fast() -> ResilienceConfig {
    ResilienceConfig {
        first_frame: Duration::from_millis(400),
        model_idle: Duration::from_millis(400),
        tool_idle: Duration::from_secs(5),
        turn_hard_cap: Duration::from_secs(20),
        interrupt_grace: Duration::from_millis(100),
        retry_base: Duration::from_millis(20),
        retry_cap: Duration::from_millis(80),
        breaker_threshold: 3,
        breaker_window: Duration::from_secs(60),
        breaker_open: Duration::from_millis(300),
    }
}

fn resilient(config: ResilienceConfig) -> EngineOptions {
    let resilience = Resilience::with_options(
        config,
        ResilienceOptions {
            rng: Box::new(|| 1.0),
            ..Default::default()
        },
    );
    EngineOptions {
        runner: Arc::new(ResilientRunner::with_resilience(resilience)),
        ..quiet()
    }
}

fn route(provider: &str, model: &str) -> Route {
    Route {
        provider: provider.into(),
        model: model.into(),
        mode: "cli".into(),
        reasoning_effort: None,
    }
}

/// Root sends one child on turn 1 and otherwise reports what it saw.
fn lead(c: &Ctx) -> Option<String> {
    if c.depth() > 0 {
        return None;
    }
    Some(if c.turn() == 1 { send(json!({})) } else { "merged".into() })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_child_failure_retries_with_resumed_session_and_notifies_parent() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let a = attempts.clone();
    let fx = Fx::with(
        script(move |c| {
            let a = a.clone();
            async move {
                if let Some(text) = lead(&c) {
                    return Ok(text);
                }
                if a.fetch_add(1, Ordering::SeqCst) == 0 {
                    let _ = c.events.send(AdapterEvent::Session("native-1".into()));
                    let mut e = classify::from_http_status(503, None);
                    e.session_id = Some("native-1".into());
                    return Err(e);
                }
                assert_eq!(c.req.session_id.as_deref(), Some("native-1"));
                assert!(!c.packet["recovery"].is_null());
                assert!(c.req.prompt.contains("previous attempt of this turn was interrupted"));
                Ok("recovered".into())
            }
        }),
        t1_catalog(),
        resilient(fast()),
    );
    let id = fx.begin(fx.params()).await;
    fx.finish(id, "done").await;
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let retries = payloads(&fx, "SELECT payload FROM events WHERE kind='turn.retry_scheduled'");
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0]["class"], json!("overloaded"));
    assert_eq!(retries[0]["resume_session"], json!(true));
    assert_eq!(fx.int("SELECT count(*) FROM native_sessions WHERE session='native-1'"), 1);
    let child = fx.int("SELECT id FROM jobs WHERE parent IS NOT NULL");
    assert_eq!(fx.all(&format!("SELECT result FROM jobs WHERE id={child}"))[0]["result"], json!("recovered"));
    let root = fx.engine.root(id).unwrap()["id"].as_i64().unwrap();
    let notes = fx.engine.notes(root).unwrap();
    assert!(notes.iter().any(|n| n["sender"] == json!("platform")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permanent_child_failure_gives_up_and_reports_a_non_retryable_failure() {
    let fx = Fx::with(
        script(|c| async move {
            if c.depth() > 0 {
                return Err(classify::from_http_status(401, None));
            }
            if c.turn() == 1 {
                return Ok(send(json!({})));
            }
            let report = &c.packet["reports"][0];
            if report["state"] == json!("failed") {
                assert_eq!(report["failure"]["class"], json!("auth"));
                assert_eq!(report["failure"]["retryable"], json!(false));
                assert!(c.req.prompt.contains("re-dispatch that work to a different harness"));
            }
            Ok("noted".into())
        }),
        t1_catalog(),
        resilient(fast()),
    );
    let id = fx.begin(fx.params()).await;
    fx.finish(id, "partial").await;
    let gave_up = payloads(&fx, "SELECT payload FROM events WHERE kind='turn.gave_up'");
    assert_eq!(gave_up.len(), 1);
    assert_eq!(gave_up[0]["class"], json!("auth"));
    assert_eq!(fx.int("SELECT count(*) FROM jobs WHERE parent IS NOT NULL AND state='failed'"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unavailable_route_rotates_to_the_next_configured_provider() {
    let mut rotation = HashMap::new();
    rotation.insert("deepseek".to_string(), vec![route("test-local", "default"), route("other", "foreign")]);
    let opts = EngineOptions {
        rotation: Arc::new(StaticRotation(rotation)),
        ..resilient(fast())
    };
    let fx = Fx::with(
        script(|c| async move {
            if let Some(text) = lead(&c) {
                return Ok(text);
            }
            if c.req.route.provider == "test-local" {
                return Err(AdapterError::new(ErrorClass::RouteUnavailable, "所选供应商或模型暂时不可用"));
            }
            assert_eq!(c.req.route.model, "foreign");
            assert_eq!(c.req.session_id, None);
            Ok("served elsewhere".into())
        }),
        t1_catalog(),
        opts,
    );
    let id = fx.begin(fx.params()).await;
    fx.finish(id, "done").await;
    let rotated = payloads(&fx, "SELECT payload FROM events WHERE kind='route.rotated'");
    assert_eq!(rotated.len(), 1);
    assert_eq!(rotated[0]["to"], json!({"provider": "other", "model": "foreign", "mode": "cli"}));
    let child = fx.int("SELECT id FROM jobs WHERE parent IS NOT NULL");
    let bound: Value = serde_json::from_str(
        fx.all(&format!("SELECT route FROM native_routes WHERE job={child}"))[0]["route"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(bound["provider"], json!("other"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn silent_stream_is_stopped_by_the_watchdog_and_retried() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let a = attempts.clone();
    let fx = Fx::with(
        script(move |c| {
            let a = a.clone();
            async move {
                if let Some(text) = lead(&c) {
                    return Ok(text);
                }
                if a.fetch_add(1, Ordering::SeqCst) == 0 {
                    c.cancel.cancelled().await;
                    return Err(AdapterError::new(ErrorClass::Cancelled, "已取消"));
                }
                Ok("answered".into())
            }
        }),
        t1_catalog(),
        resilient(fast()),
    );
    let id = fx.begin(fx.params()).await;
    fx.finish(id, "done").await;
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(fx.all("SELECT seq FROM events WHERE kind='turn.stalled'").len(), 1);
    let retries = payloads(&fx, "SELECT payload FROM events WHERE kind='turn.retry_scheduled'");
    assert_eq!(retries[0]["class"], json!("stalled"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_breaker_waits_for_the_route_without_spending_attempts() {
    let config = ResilienceConfig {
        breaker_threshold: 1,
        ..fast()
    };
    let attempts = Arc::new(AtomicUsize::new(0));
    let a = attempts.clone();
    let fx = Fx::with(
        script(move |c| {
            let a = a.clone();
            async move {
                if let Some(text) = lead(&c) {
                    return Ok(text);
                }
                if a.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(classify::from_http_status(503, None));
                }
                Ok("after the breaker".into())
            }
        }),
        t1_catalog(),
        resilient(config),
    );
    let id = fx.begin(fx.params()).await;
    fx.finish(id, "done").await;
    assert_eq!(fx.all("SELECT seq FROM events WHERE kind='turn.route_wait'").len(), 1);
    let health = payloads(&fx, "SELECT payload FROM events WHERE kind='harness.health' ORDER BY seq");
    assert!(health.iter().any(|h| h["state"] == json!("open")));
    assert!(health.iter().any(|h| h["state"] == json!("healthy")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pass_through_runner_keeps_the_legacy_auto_root_retry() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let a = attempts.clone();
    let opts = EngineOptions {
        runner: Arc::new(PassThroughRunner::default()),
        ..quiet()
    };
    let fx = Fx::with(
        script(move |c| {
            let a = a.clone();
            async move {
                if c.phase() == Some("discover") {
                    c.prove();
                    return Ok(wire(
                        "SA_EXHAUSTED",
                        json!({"status": "done", "summary": "checked", "checks": ["files"], "alternatives": ["none"]}),
                    ));
                }
                if a.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(AdapterError::new(ErrorClass::Other, "上游返回 HTTP 503"));
                }
                Ok(wire("SA_FINISH", json!({"status": "done", "summary": "ok"})))
            }
        }),
        t1_catalog(),
        opts,
    );
    let id = fx.begin(CreateParams::new("fixture", &fx.project).profile("auto")).await;
    fx.finish(id, "done").await;
    let retries = payloads(&fx, "SELECT payload FROM events WHERE kind='auto.retry'");
    assert_eq!(retries[0]["after_seconds"], json!(2));
    assert_eq!(fx.all("SELECT seq FROM events WHERE kind='turn.retry_scheduled'").len(), 0);
}
