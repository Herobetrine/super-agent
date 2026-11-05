//! Rust-port controls and UI surfaces of spec §9.2 F/G: `SA_CANCEL`,
//! optional automatic failover of read-only children and the live
//! `Engine::activity` API.

mod common;

use common::*;
use sa_engine::adapter::{AdapterError, AdapterEvent, ErrorClass};
use sa_engine::resilience::{CODE_ROUTE_NOT_CONFIGURED, Resilience, ResilienceConfig, ResilienceOptions, classify};
use sa_engine::runner::ResilientRunner;
use sa_engine::{ActivityPhase, BadgeTone, CreateParams, EngineOptions, JobActivity};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn config() -> ResilienceConfig {
    ResilienceConfig {
        first_frame: Duration::from_secs(10),
        model_idle: Duration::from_secs(10),
        tool_idle: Duration::from_secs(20),
        turn_hard_cap: Duration::from_secs(60),
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

fn kinds(fx: &Fx, kind: &str) -> Vec<Value> {
    payloads(fx, &format!("SELECT payload FROM events WHERE kind='{kind}' ORDER BY seq"))
}

fn state(fx: &Fx, job: i64) -> String {
    fx.all(&format!("SELECT state FROM jobs WHERE id={job}"))[0]["state"]
        .as_str()
        .unwrap()
        .to_string()
}

fn cancel(id: &str, job: Value) -> String {
    wire("SA_CANCEL", json!({"id": id, "job": job}))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sa_cancel_stops_a_direct_child_subtree_idempotently() {
    let grandchild_running = Gate::new();
    let grandchild_aborted = Gate::new();
    let grandchild_calls = Arc::new(AtomicUsize::new(0));
    let (running, aborted, calls) = (grandchild_running.clone(), grandchild_aborted.clone(), grandchild_calls.clone());
    let fx = Fx::new(
        script(move |c| {
            let (running, aborted, calls) = (running.clone(), aborted.clone(), calls.clone());
            async move {
                match c.depth() {
                    0 if c.turn() == 1 => {
                        let child = c.job() + 1;
                        let dispatch = send(json!({"id": "a"}));
                        c.emit("delta", json!(format!("{dispatch}\n")));
                        running.wait().await;
                        let stop = cancel("stop", json!(child));
                        c.emit("delta", json!(format!("{stop}\n")));
                        aborted.wait().await;
                        Ok([
                            dispatch,
                            stop,
                            cancel("stop", json!(999)),
                            cancel("self", json!(c.job())),
                            "SA_CANCEL {bad".into(),
                            cancel("", json!(child)),
                        ]
                        .join("\n"))
                    }
                    0 => {
                        let child = c.job() + 1;
                        assert_eq!(c.packet["reports"][0]["state"], json!("cancelled"));
                        Ok(format!("checked\n{}", cancel("stop", json!(child))))
                    }
                    1 => Ok(wire("SA_SEND", json!({"id": "deep", "to": "claude", "body": "inspect"}))),
                    _ => {
                        calls.fetch_add(1, Ordering::SeqCst);
                        running.open();
                        c.cancel.cancelled().await;
                        aborted.open();
                        Err(AdapterError::new(ErrorClass::Cancelled, "已取消"))
                    }
                }
            }
        }),
        t1_catalog(),
    );
    let ticket = fx.begin(fx.params()).await;
    fx.finish(ticket, "partial").await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    let (child, grandchild) = (root + 1, root + 2);
    assert_eq!(state(&fx, child), "cancelled");
    assert_eq!(state(&fx, grandchild), "cancelled");
    assert_eq!(grandchild_calls.load(Ordering::SeqCst), 1);
    let accepted = kinds(&fx, "child.cancelled");
    assert_eq!(accepted, vec![json!({"id": "stop", "job": child, "cancelled": [child, grandchild]})]);
    let reason = format!("上级任务 #{root} 取消了此任务");
    let stopped = fx.all("SELECT job,payload FROM events WHERE kind='cancelled' ORDER BY seq");
    assert_eq!(stopped.len(), 2);
    assert!(stopped.iter().all(|r| r["payload"] == json!(serde_json::to_string(&reason).unwrap())));
    let reasons: Vec<Value> = kinds(&fx, "cancel.rejected").iter().map(|r| r["reason"].clone()).collect();
    assert_eq!(
        reasons,
        vec![
            json!("conflicting_id"),
            json!("not_child"),
            json!("invalid_json"),
            json!("invalid_fields"),
            json!("terminal")
        ]
    );
    // stop (turn 1), self (turn 1), stop again on turn 2 (a new logical turn).
    assert_eq!(fx.int("SELECT count(*) FROM cancel_receipts"), 3);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='send'"), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sa_cancel_rejects_a_sibling() {
    // A child may not cancel its sibling: only its own direct children.
    let fx = Fx::new(
        script(|c| async move {
            match (c.depth(), c.turn()) {
                (0, 1) => Ok([send(json!({"id": "a"})), send(json!({"id": "b", "to": "claude"}))].join("\n")),
                (0, _) => Ok("merged".into()),
                _ if c.name == "deepseek" => Ok(cancel("sibling", json!(c.job() + 1))),
                _ => Ok("sibling done".into()),
            }
        }),
        t1_catalog(),
    );
    let ticket = fx.begin(fx.params()).await;
    fx.finish(ticket, "done").await;
    assert_eq!(fx.int("SELECT count(*) FROM jobs WHERE state='cancelled'"), 0);
    let rejected = kinds(&fx, "cancel.rejected");
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0]["reason"], json!("not_child"));
}

/// Root sends one read-only child (deepseek) on turn 1 and checks the
/// reports on later turns.
fn failover_script(reports: Arc<Mutex<Vec<Value>>>, child: impl Fn(&Ctx) -> Result<String, AdapterError> + Send + Sync + 'static) -> Script {
    let child = Arc::new(child);
    script(move |c| {
        let (reports, child) = (reports.clone(), child.clone());
        async move {
            if c.depth() > 0 {
                return child(&c);
            }
            if c.turn() == 1 {
                return Ok(send(json!({"role": "reviewer"})));
            }
            reports.lock().unwrap().push(c.packet["reports"].clone());
            Ok("merged".into())
        }
    })
}

fn not_configured() -> AdapterError {
    let mut e = AdapterError::new(ErrorClass::RouteUnavailable, "所选供应商、模型或执行端当前不可用");
    e.code = Some(CODE_ROUTE_NOT_CONFIGURED.into());
    e
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unavailable_reader_fails_over_to_the_next_harness_when_enabled() {
    let reports = Arc::new(Mutex::new(vec![]));
    let fx = Fx::with(
        failover_script(reports.clone(), |c| {
            if c.name == "deepseek" {
                Err(not_configured())
            } else {
                assert!(!c.req.writable);
                Ok(format!("served by {}", c.name))
            }
        }),
        t1_catalog(),
        resilient(config()),
    );
    let ticket = fx.begin(fx.params().failover(true)).await;
    assert!(fx.engine.failover(ticket).unwrap());
    fx.finish(ticket, "done").await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    let (original, sibling) = (root + 1, root + 2);
    assert_eq!(state(&fx, original), "cancelled");
    let jobs = fx.all(&format!("SELECT agent,body,writing,parent,assigned_role,result FROM jobs WHERE id={sibling}"));
    assert_eq!(jobs[0]["agent"], json!("codex"));
    assert_eq!(jobs[0]["body"], json!("核对  \r\n\t🧭"));
    assert_eq!(jobs[0]["writing"], json!(0));
    assert_eq!(jobs[0]["parent"], json!(root));
    assert_eq!(jobs[0]["assigned_role"], json!("reviewer"));
    assert_eq!(jobs[0]["result"], json!("served by codex"));
    let sends = kinds(&fx, "send");
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[1]["failover_of"], json!(original));
    assert_eq!(sends[1]["reason"], json!("route_unavailable"));
    assert_eq!(sends[1]["to"], json!("codex"));
    assert_eq!(sends[1]["child"], json!(sibling));
    assert_eq!(sends[1]["sha256"], json!(sha("核对  \r\n\t🧭")));
    assert_eq!(fx.int("SELECT count(*) FROM job_failovers"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='turn.retry_scheduled'"), 0);
    let last = reports.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last[0]["replaced_by"], json!(sibling));
    assert_eq!(last[1]["state"], json!("done"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failover_is_off_by_default_and_never_moves_writers() {
    for (failover, write) in [(false, false), (true, true)] {
        let fx = Fx::with(
            script(move |c| async move {
                if c.depth() > 0 {
                    return if c.name == "deepseek" { Err(not_configured()) } else { Ok("other".into()) };
                }
                if c.turn() == 1 {
                    return Ok(send(json!({"write": write})));
                }
                Ok("merged".into())
            }),
            t1_catalog(),
            resilient(config()),
        );
        let mut params = fx.params().profile("full");
        params.failover = Some(failover);
        let ticket = fx.begin(params).await;
        fx.finish(ticket, "partial").await;
        assert_eq!(fx.engine.failover(ticket).unwrap(), failover);
        assert_eq!(fx.int("SELECT count(*) FROM job_failovers"), 0);
        assert_eq!(fx.int("SELECT count(*) FROM jobs"), 2);
        assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='turn.gave_up'"), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_breaker_fails_over_a_reader_enabled_after_create() {
    let cfg = ResilienceConfig {
        breaker_threshold: 1,
        ..config()
    };
    let fx = Fx::with(
        failover_script(Arc::new(Mutex::new(vec![])), |c| {
            if c.name == "deepseek" {
                Err(classify::from_http_status(503, None))
            } else {
                Ok("after failover".into())
            }
        }),
        t1_catalog(),
        resilient(cfg),
    );
    let ticket = fx.create(fx.params()).await;
    assert!(!fx.engine.failover(ticket).unwrap());
    fx.engine.set_failover(ticket, true).unwrap();
    fx.engine.set_failover(ticket, true).unwrap();
    assert_eq!(kinds(&fx, "failover.changed"), vec![json!({"enabled": true})]);
    fx.engine.start().unwrap();
    fx.finish(ticket, "done").await;
    let sends = kinds(&fx, "send");
    assert_eq!(sends.last().unwrap()["reason"], json!("circuit_open"));
    assert_eq!(sends.last().unwrap()["to"], json!("codex"));
    let health = kinds(&fx, "harness.health");
    assert!(health.iter().any(|h| h["state"] == json!("open")));
    fx.engine.set_failover(ticket, false).unwrap();
    assert!(!fx.engine.failover(ticket).unwrap());
}

async fn change_failover_during_child_turn(initial: bool) {
    let (started, release) = (Gate::new(), Gate::new());
    let (s, r) = (started.clone(), release.clone());
    let fx = Fx::with(
        script(move |c| {
            let (s, r) = (s.clone(), r.clone());
            async move {
                if c.depth() == 0 {
                    return Ok(if c.turn() == 1 { send(json!({})) } else { "merged".into() });
                }
                if c.name == "deepseek" {
                    s.open();
                    r.wait().await;
                    return Err(not_configured());
                }
                assert!(!c.req.writable);
                Ok("replacement done".into())
            }
        }),
        t1_catalog(),
        resilient(config()),
    );
    let ticket = fx.begin(fx.params().failover(initial)).await;
    fx.until(|| started.is_open()).await;
    let child = fx.int("SELECT id FROM jobs WHERE parent IS NOT NULL");
    assert_eq!(state(&fx, child), "running");

    // The same admitted turn must honor the new setting when it fails.
    let enabled = !initial;
    fx.engine.set_failover(ticket, enabled).unwrap();
    assert_eq!(fx.engine.failover(ticket).unwrap(), enabled);
    release.open();
    fx.finish(ticket, if enabled { "done" } else { "partial" }).await;

    assert_eq!(state(&fx, child), if enabled { "cancelled" } else { "failed" });
    assert_eq!(fx.int("SELECT count(*) FROM job_failovers"), i64::from(enabled));
    assert_eq!(fx.int("SELECT count(*) FROM jobs"), if enabled { 3 } else { 2 });
    assert_eq!(fx.shared.count(|c| c.packet["depth"].as_i64().unwrap_or_default() > 0 && c.name == "codex"), usize::from(enabled));
    assert_eq!(kinds(&fx, "failover.changed"), vec![json!({"enabled": enabled})]);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabling_failover_during_a_child_turn_prevents_replacement() {
    change_failover_during_child_turn(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enabling_failover_during_a_child_turn_allows_replacement() {
    change_failover_during_child_turn(false).await;
}

fn find(fx: &Fx, ticket: i64, job: i64) -> Option<JobActivity> {
    fx.engine.activity(ticket).into_iter().find(|a| a.job == job)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn activity_tracks_tools_frames_and_approvals_of_a_running_turn() {
    let (tool, frame) = (Gate::new(), Gate::new());
    let (t, f) = (tool.clone(), frame.clone());
    let fx = Fx::with(
        script(move |c| {
            let (t, f) = (t.clone(), f.clone());
            async move {
                let _ = c.events.send(AdapterEvent::ToolStarted("call-1".into()));
                t.wait().await;
                let _ = c.events.send(AdapterEvent::ToolFinished("call-1".into()));
                c.emit("native", json!({"type": "frame"}));
                f.wait().await;
                let ok = c.host.approve(json!({"action": "edit", "scope": "workspace"})).await;
                assert!(ok);
                Ok("done".into())
            }
        }),
        t1_catalog(),
        resilient(config()),
    );
    let ticket = fx.begin(fx.params().profile("partial")).await;
    let job = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();

    fx.until(|| find(&fx, ticket, job).is_some_and(|a| a.phase == ActivityPhase::ToolRunning)).await;
    let a = find(&fx, ticket, job).unwrap();
    assert_eq!(a.agent, "codex");
    assert_eq!(a.open_tools, vec!["call-1".to_string()]);
    assert_eq!(a.threshold_ms, Some(20_000));
    assert!(a.started_at.is_some() && a.last_frame_at.is_some());
    assert_eq!(a.badge(), ("工具执行中", BadgeTone::Busy));
    assert!(a.retry.is_none());

    tool.open();
    fx.until(|| find(&fx, ticket, job).is_some_and(|a| a.phase == ActivityPhase::Streaming)).await;
    let a = find(&fx, ticket, job).unwrap();
    assert!(a.open_tools.is_empty());
    assert_eq!(a.threshold_ms, Some(10_000));
    assert_eq!(a.badge(), ("运行中", BadgeTone::Active));

    frame.open();
    fx.until(|| find(&fx, ticket, job).is_some_and(|a| a.phase == ActivityPhase::AwaitingApproval)).await;
    let a = find(&fx, ticket, job).unwrap();
    assert_eq!(a.threshold_ms, None);
    assert_eq!(a.badge().0, "等待审批");
    fx.until(|| !fx.all("SELECT id FROM approvals WHERE decision='pending'").is_empty()).await;
    let approval = fx.int("SELECT id FROM approvals WHERE decision='pending'");
    fx.engine.decide(approval, true).unwrap();
    fx.finish(ticket, "done").await;
    fx.until(|| fx.engine.activity(ticket).is_empty()).await;
    assert!(fx.engine.activity(ticket + 1).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn activity_reports_a_scheduled_retry_from_the_journal() {
    let cfg = ResilienceConfig {
        retry_base: Duration::from_secs(3),
        retry_cap: Duration::from_secs(3),
        ..config()
    };
    let attempts = Arc::new(AtomicUsize::new(0));
    let a = attempts.clone();
    let fx = Fx::with(
        script(move |c| {
            let a = a.clone();
            async move {
                if c.depth() == 0 {
                    return Ok(if c.turn() == 1 { send(json!({})) } else { "merged".into() });
                }
                if a.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(classify::from_http_status(503, None));
                }
                Ok("recovered".into())
            }
        }),
        t1_catalog(),
        resilient(cfg),
    );
    let ticket = fx.begin(fx.params()).await;
    let child = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap() + 1;
    fx.until(|| find(&fx, ticket, child).is_some_and(|a| a.retry.is_some())).await;
    let a = find(&fx, ticket, child).unwrap();
    assert_eq!(a.phase, ActivityPhase::RetryWait);
    assert_eq!(a.agent, "deepseek");
    let retry = a.retry.clone().unwrap();
    assert_eq!((retry.attempt, retry.max, retry.class.as_str()), (1, Some(5), "overloaded"));
    assert_eq!(a.health, sa_engine::resilience::Health::Degraded);
    assert_eq!(a.badge(), ("已中断，重试中", BadgeTone::Warning));
    let now = sa_engine::db::now();
    assert!(retry.next_at > now && retry.next_at <= now + 3.5);
    assert!(a.badge_text(now).starts_with("已中断，重试中 1/5 · 00:0"));
    fx.finish(ticket, "done").await;
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retrying_child_report_brings_the_sa_cancel_guide() {
    let fx = Fx::with(
        script(|c| async move {
            if c.depth() > 0 {
                return Err(classify::from_http_status(401, None));
            }
            if c.turn() == 1 {
                return Ok(send(json!({})));
            }
            assert!(c.req.prompt.contains("SA_CANCEL {\"id\":\"unique-cancel-key\""));
            Ok("noted".into())
        }),
        t1_catalog(),
        resilient(config()),
    );
    let ticket = fx.begin(CreateParams::new("fixture", &fx.project).lead("codex")).await;
    fx.finish(ticket, "partial").await;
}
