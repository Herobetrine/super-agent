//! P2 rule-based execution policy: decisions are always recorded; only mode
//! `apply` changes behavior, and only in the three documented ways.

mod common;

use common::*;
use sa_engine::resilience::{Resilience, ResilienceConfig, ResilienceOptions, classify};
use sa_engine::runner::ResilientRunner;
use sa_engine::EngineOptions;
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn resilient() -> EngineOptions {
    let config = ResilienceConfig {
        first_frame: Duration::from_millis(400),
        model_idle: Duration::from_millis(400),
        tool_idle: Duration::from_secs(5),
        turn_hard_cap: Duration::from_secs(20),
        interrupt_grace: Duration::from_millis(100),
        retry_base: Duration::from_millis(20),
        retry_cap: Duration::from_millis(80),
        breaker_threshold: 50,
        breaker_window: Duration::from_secs(60),
        breaker_open: Duration::from_millis(300),
    };
    let resilience = Resilience::with_options(config, ResilienceOptions { rng: Box::new(|| 1.0), ..Default::default() });
    EngineOptions { runner: Arc::new(ResilientRunner::with_resilience(resilience)), ..quiet() }
}

fn git(path: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=Fixture", "-c", "user.email=fixture@localhost", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

fn init(path: &Path) {
    git(path, &["init"]);
    git(path, &["config", "core.autocrlf", "false"]);
    std::fs::write(path.join("base.txt"), "base\n").unwrap();
    std::fs::write(path.join("a.txt"), "a\n").unwrap();
    std::fs::write(path.join("b.txt"), "b\n").unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-m", "fixture"]);
}

fn decisions(fx: &Fx, ticket: i64) -> Vec<Value> {
    let mut all = fx.engine.policy_decisions(ticket, 1000).unwrap();
    all.reverse();
    all
}

fn selected(fx: &Fx, ticket: i64, point: &str) -> Vec<String> {
    decisions(fx, ticket)
        .iter()
        .filter(|d| d["point"] == point)
        .map(|d| d["selected"].as_str().unwrap().to_string())
        .collect()
}

/// Journal rows that stand in for spent automatic recoveries, one per
/// (historical) job turn: the budget counts distinct recoveries.
fn spend(fx: &Fx, ticket: i64, job: i64, count: usize) {
    fx.engine
        .db()
        .with(|q| {
            for i in 0..count {
                let kind = ["auto.retry", "check.bounced", "integration.bounced", "route.rotated"][i % 4];
                let turn = 1000 + i as i64;
                q.run(
                    "INSERT INTO events(ts,ticket,job,turn,kind,payload,sha256) VALUES(0,?,?,?,?,'{}','x')",
                    &[&ticket, &job, &turn, &kind],
                )?;
            }
            Ok(())
        })
        .unwrap();
}

fn root_id(fx: &Fx, ticket: i64) -> i64 {
    fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap()
}

/// Lead sends a read-only child and writers over `a.txt` / `b.txt` in turn 1,
/// all held by a gate so they stay active, then reports.
fn dispatch_scenario(gate: Gate) -> Script {
    script(move |c| {
        let gate = gate.clone();
        async move {
            if c.depth() > 0 {
                gate.wait().await;
                return Ok("done".into());
            }
            if c.turn() == 1 {
                let lines = [
                    send(json!({"id": "reader", "body": "read only look", "write": false})),
                    send(json!({"id": "wa", "to": "codex", "body": "edit a", "write": true, "files": ["a.txt"]})),
                    send(json!({"id": "wb", "to": "codex", "body": "edit b", "write": true, "files": ["b.txt"]})),
                    send(json!({"id": "wc", "to": "codex", "body": "edit a again", "write": true, "files": ["a.txt"]})),
                ];
                return Ok(lines.join("\n"));
            }
            Ok("merged".into())
        }
    })
}

async fn run_dispatch(mode: &str, isolated: bool) -> (Fx, i64) {
    let gate = Gate::new();
    let fx = Fx::new(dispatch_scenario(gate.clone()), t1_catalog());
    init(&fx.project);
    let ticket = fx.create(fx.params().profile("full")).await;
    fx.engine.set_execution_policy(&fx.project.to_string_lossy(), mode).unwrap();
    if isolated {
        fx.engine.set_isolated_writers(ticket, true).unwrap();
    }
    fx.engine.start().unwrap();
    fx.until(|| fx.engine.policy_decisions(ticket, 100).unwrap().len() >= 4).await;
    gate.open();
    fx.finish(ticket, "done").await;
    (fx, ticket)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn record_mode_logs_dispatch_decisions_without_changing_anything() {
    let (fx, ticket) = run_dispatch("record", false).await;
    assert_eq!(selected(&fx, ticket, "dispatch"), ["limited_parallel", "single", "limited_parallel", "serialize"]);
    assert_eq!(fx.int("SELECT count(*) FROM job_deps"), 0);
    assert!(decisions(&fx, ticket).iter().all(|d| d["applied"] == false && d["mode"] == "record" && d["policy_version"] == "rules-1"));
    // Every decision is mirrored by an event with the same payload.
    let events = payloads(&fx, "SELECT payload FROM events WHERE kind='policy.decision' ORDER BY seq");
    let stored = decisions(&fx, ticket);
    assert_eq!(events.len(), stored.len());
    for (event, row) in events.iter().zip(&stored) {
        assert_eq!(event["id"], row["id"]);
        assert_eq!(event["selected"], row["selected"]);
        assert_eq!(event["reason"], row["reason"]);
        assert_eq!(event["inputs"], row["inputs"]);
    }
    let overlap = stored.iter().find(|d| d["selected"] == "serialize").unwrap();
    assert_eq!(overlap["inputs"]["files"], json!(["a.txt"]));
    assert_eq!(overlap["inputs"]["isolated_writers"], false);
    assert!(overlap["alternatives"].as_array().unwrap().iter().any(|a| a == "add_review"));
    assert_eq!(fx.int("SELECT count(*) FROM jobs WHERE parent IS NOT NULL AND state='done'"), 4);
    assert_eq!(fx.engine.execution_policy(&fx.project.to_string_lossy()).unwrap(), "record");
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_mode_without_isolation_serializes_by_advice_only() {
    let (fx, ticket) = run_dispatch("apply", false).await;
    assert_eq!(selected(&fx, ticket, "dispatch"), ["limited_parallel", "single", "limited_parallel", "serialize"]);
    assert_eq!(fx.int("SELECT count(*) FROM job_deps"), 0);
    assert!(decisions(&fx, ticket).iter().all(|d| d["applied"] == false && d["mode"] == "apply"));
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_mode_adds_dependencies_only_between_overlapping_isolated_writers() {
    let (fx, ticket) = run_dispatch("apply", true).await;
    let jobs = fx.all("SELECT id,msgkey FROM jobs WHERE parent IS NOT NULL ORDER BY id");
    let id = |key: &str| jobs.iter().find(|j| j["msgkey"] == key).unwrap()["id"].as_i64().unwrap();
    let deps = fx.all("SELECT job,needs FROM job_deps ORDER BY job");
    assert_eq!(deps.len(), 1, "{deps:?}");
    assert_eq!((deps[0]["job"].as_i64().unwrap(), deps[0]["needs"].as_i64().unwrap()), (id("wc"), id("wa")));
    let applied: Vec<bool> = decisions(&fx, ticket).iter().map(|d| d["applied"] == true).collect();
    assert_eq!(applied, [false, false, false, true]);
    assert_eq!(fx.int("SELECT count(*) FROM jobs WHERE parent IS NOT NULL AND state='done'"), 4);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finished_siblings_never_receive_a_dependency() {
    let fx = Fx::new(
        script(|c| async move {
            if c.depth() > 0 {
                return Ok("done".into());
            }
            Ok(match c.turn() {
                1 => send(json!({"id": "first", "to": "codex", "body": "edit a", "write": true, "files": ["a.txt"]})),
                2 => send(json!({"id": "second", "to": "codex", "body": "edit a later", "write": true, "files": ["a.txt"]})),
                _ => "merged".into(),
            })
        }),
        t1_catalog(),
    );
    init(&fx.project);
    let ticket = fx.create(fx.params().profile("full")).await;
    fx.engine.set_execution_policy(&fx.project.to_string_lossy(), "apply").unwrap();
    fx.engine.set_isolated_writers(ticket, true).unwrap();
    fx.engine.start().unwrap();
    fx.finish(ticket, "done").await;
    assert_eq!(selected(&fx, ticket, "dispatch"), ["single", "single"]);
    assert_eq!(fx.int("SELECT count(*) FROM job_deps"), 0);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn high_risk_writer_is_advised_an_independent_review() {
    let fx = Fx::new(
        script(|c| async move {
            if c.depth() > 0 {
                return Ok("done".into());
            }
            Ok(if c.turn() == 1 {
                send(json!({"id": "risky", "to": "codex", "body": "重写并发恢复逻辑", "write": true}))
            } else {
                "merged".into()
            })
        }),
        t1_catalog(),
    );
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "done").await;
    let all = decisions(&fx, ticket);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0]["selected"], "add_review");
    assert_eq!(all[0]["inputs"]["high_risk"], true);
    assert!(all[0]["reason"].as_str().unwrap().contains("独立审查"));
    assert_eq!(fx.int("SELECT count(*) FROM job_deps"), 0);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replayed_sa_send_records_one_decision() {
    let fx = Fx::new(
        script(|c| async move {
            if c.depth() > 0 {
                return Ok("done".into());
            }
            Ok(if c.turn() == 1 {
                let line = send(json!({"id": "same", "to": "codex", "body": "edit", "write": true}));
                format!("{line}\n{line}")
            } else {
                "merged".into()
            })
        }),
        t1_catalog(),
    );
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "done").await;
    assert_eq!(fx.int("SELECT count(*) FROM jobs WHERE parent IS NOT NULL"), 1);
    assert_eq!(decisions(&fx, ticket).len(), 1);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='policy.decision'"), 1);
    fx.engine.close().await;
}

fn flaky_child(calls: Arc<Mutex<u32>>, succeed_on: u32) -> Script {
    script(move |c| {
        let calls = calls.clone();
        async move {
            if c.depth() == 0 {
                return Ok(if c.turn() == 1 { send(json!({"to": "codex"})) } else { "noted".into() });
            }
            let n = {
                let mut calls = calls.lock().unwrap();
                *calls += 1;
                *calls
            };
            if n >= succeed_on {
                return Ok("recovered".into());
            }
            Err(classify::from_http_status(503, None))
        }
    })
}

async fn retry_ticket(fx: &Fx, mode: &str, spent: usize) -> i64 {
    let ticket = fx.create(fx.params()).await;
    fx.engine.set_execution_policy(&fx.project.to_string_lossy(), mode).unwrap();
    spend(fx, ticket, root_id(fx, ticket), spent);
    fx.engine.start().unwrap();
    ticket
}

fn child_state(fx: &Fx) -> (String, String) {
    let row = &fx.all("SELECT state,result FROM jobs WHERE parent IS NOT NULL")[0];
    (row["state"].as_str().unwrap().into(), row["result"].as_str().unwrap_or_default().into())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ticket_budget_stops_the_next_automatic_retry_only_in_apply_mode() {
    let calls = Arc::new(Mutex::new(0));
    let fx = Fx::with(flaky_child(calls.clone(), 2), t1_catalog(), resilient());
    let ticket = retry_ticket(&fx, "apply", 40).await;
    fx.finish(ticket, "partial").await;
    assert_eq!(*calls.lock().unwrap(), 1, "no retry may follow once the budget is spent");
    let (state, result) = child_state(&fx);
    assert_eq!((state.as_str(), result.as_str()), ("failed", "工单自动重试与修复已达上限（40 次），请人工处理"));
    let stop = decisions(&fx, ticket).into_iter().find(|d| d["selected"] == "stop").unwrap();
    assert_eq!((stop["point"].clone(), stop["applied"].clone()), (json!("retry"), json!(true)));
    assert_eq!(stop["inputs"]["automatic_attempts"], 40);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='turn.retry_scheduled'"), 0);
    fx.engine.close().await;

    let calls = Arc::new(Mutex::new(0));
    let fx = Fx::with(flaky_child(calls.clone(), 2), t1_catalog(), resilient());
    let ticket = retry_ticket(&fx, "record", 40).await;
    fx.finish(ticket, "done").await;
    assert_eq!(*calls.lock().unwrap(), 2);
    assert_eq!(child_state(&fx).0, "done");
    let stop = decisions(&fx, ticket).into_iter().find(|d| d["selected"] == "stop").unwrap();
    assert_eq!((stop["applied"].clone(), stop["mode"].clone()), (json!(false), json!("record")));
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn budget_just_below_the_limit_still_retries() {
    let calls = Arc::new(Mutex::new(0));
    let fx = Fx::with(flaky_child(calls.clone(), 2), t1_catalog(), resilient());
    let ticket = retry_ticket(&fx, "apply", 38).await;
    fx.finish(ticket, "done").await;
    assert_eq!(*calls.lock().unwrap(), 2);
    assert!(selected(&fx, ticket, "retry").iter().all(|s| s == "retry"));
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_same_class_failures_rediagnose_in_apply_and_only_log_in_record() {
    let calls = Arc::new(Mutex::new(0));
    let fx = Fx::with(flaky_child(calls.clone(), u32::MAX), t1_catalog(), resilient());
    let ticket = retry_ticket(&fx, "apply", 0).await;
    fx.finish(ticket, "partial").await;
    assert_eq!(*calls.lock().unwrap(), 3);
    let (state, result) = child_state(&fx);
    assert_eq!(state, "failed");
    assert_eq!(result, "同类失败连续 3 次，平台不再原样重试；请重新诊断后再派发");
    assert_eq!(selected(&fx, ticket, "retry"), ["retry", "retry", "rediagnose"]);
    assert_eq!(decisions(&fx, ticket).last().unwrap()["applied"], true);
    fx.engine.close().await;

    let calls = Arc::new(Mutex::new(0));
    let fx = Fx::with(flaky_child(calls.clone(), u32::MAX), t1_catalog(), resilient());
    let ticket = retry_ticket(&fx, "record", 0).await;
    fx.finish(ticket, "partial").await;
    assert!(*calls.lock().unwrap() > 3, "record mode keeps the platform's own retry limit");
    let rediagnose: Vec<Value> = decisions(&fx, ticket).into_iter().filter(|d| d["selected"] == "rediagnose").collect();
    assert!(!rediagnose.is_empty() && rediagnose.iter().all(|d| d["applied"] == false));
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='turn.gave_up'"), 1);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_different_error_class_resets_the_consecutive_count() {
    let calls = Arc::new(Mutex::new(0));
    let seen = calls.clone();
    let fx = Fx::with(
        script(move |c| {
            let seen = seen.clone();
            async move {
                if c.depth() == 0 {
                    return Ok(if c.turn() == 1 { send(json!({"to": "codex"})) } else { "noted".into() });
                }
                let n = {
                    let mut calls = seen.lock().unwrap();
                    *calls += 1;
                    *calls
                };
                match n {
                    1 | 2 => Err(classify::from_http_status(503, None)),
                    3 | 4 => Err(classify::from_http_status(429, None)),
                    _ => Ok("recovered".into()),
                }
            }
        }),
        t1_catalog(),
        resilient(),
    );
    let ticket = retry_ticket(&fx, "apply", 0).await;
    fx.finish(ticket, "done").await;
    assert_eq!(*calls.lock().unwrap(), 5);
    assert!(selected(&fx, ticket, "retry").iter().all(|s| s == "retry"));
    fx.engine.close().await;
}

fn failing_check() -> Vec<String> {
    vec!["git".into(), "sa-fixture-missing-command".into()]
}

fn bouncing_root() -> Script {
    script(|c| async move {
        if c.turn() == 1 {
            std::fs::write(c.req.cwd.join("base.txt"), "changed\n").unwrap();
        }
        Ok(wire("SA_REPORT", json!({"status": "done", "summary": "fixture result"})))
    })
}

async fn repair_ticket(fx: &Fx, mode: &str, spent: usize) -> i64 {
    init(&fx.project);
    fx.engine.set_project_checks(&fx.project.to_string_lossy(), vec![failing_check()]).unwrap();
    let ticket = fx.create(fx.params().profile("full")).await;
    fx.engine.set_execution_policy(&fx.project.to_string_lossy(), mode).unwrap();
    spend(fx, ticket, root_id(fx, ticket), spent);
    fx.engine.start().unwrap();
    ticket
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spent_budget_ends_the_completion_check_loop_without_bouncing_in_apply_mode() {
    let fx = Fx::new(bouncing_root(), t1_catalog());
    let ticket = repair_ticket(&fx, "apply", 40).await;
    fx.finish(ticket, "partial").await;
    assert_eq!(fx.shared.count(|_| true), 1);
    // Only the ten synthetic bounces exist; none was added.
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='check.bounced'"), 10);
    let all = decisions(&fx, ticket);
    let repair = all.iter().find(|d| d["point"] == "repair").unwrap();
    assert_eq!((repair["selected"].clone(), repair["applied"].clone()), (json!("stop"), json!(true)));
    assert_eq!(repair["inputs"]["bounce"], "check");
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spent_budget_is_only_logged_for_check_bounces_in_record_mode() {
    let fx = Fx::new(bouncing_root(), t1_catalog());
    let ticket = repair_ticket(&fx, "record", 40).await;
    fx.finish(ticket, "partial").await;
    assert_eq!(fx.shared.count(|_| true), 3);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='check.bounced'"), 12);
    let all: Vec<Value> = decisions(&fx, ticket).into_iter().filter(|d| d["point"] == "repair").collect();
    assert_eq!(all.len(), 2);
    assert!(all.iter().all(|d| d["selected"] == "stop" && d["applied"] == false));
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unspent_budget_records_repair_and_bounces_as_before() {
    let fx = Fx::new(bouncing_root(), t1_catalog());
    let ticket = repair_ticket(&fx, "apply", 0).await;
    fx.finish(ticket, "partial").await;
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='check.bounced'"), 2);
    let all: Vec<Value> = decisions(&fx, ticket).into_iter().filter(|d| d["point"] == "repair").collect();
    assert!(all.iter().all(|d| d["selected"] == "repair" && d["applied"] == false));
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_feedback_reaches_the_lead_only_and_stays_bounded() {
    let feedback: Arc<Mutex<Vec<(i64, Value)>>> = Arc::default();
    let seen = feedback.clone();
    let fx = Fx::new(
        script(move |c| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push((c.depth(), c.full_packet()["policy_feedback"].clone()));
                if c.depth() > 0 {
                    return Ok("done".into());
                }
                Ok(if c.turn() == 1 { send(json!({"to": "codex"})) } else { "noted".into() })
            }
        }),
        t1_catalog(),
    );
    let ticket = fx.create(fx.params()).await;
    let root = root_id(&fx, ticket);
    spend(&fx, ticket, root, 7);
    fx.engine
        .db()
        .with(|q| {
            for i in 0..30 {
                let reason = format!("理由{i}{}", "长".repeat(3000));
                q.run(
                    "INSERT INTO policy_decisions(ticket,job,turn,point,policy_version,mode,inputs,alternatives,selected,reason,applied,created)
                     VALUES(?,?,1,'retry','rules-1','record','{}','[]','rediagnose',?,0,0)",
                    &[&ticket, &root, &reason],
                )?;
            }
            q.run(
                "INSERT INTO policy_decisions(ticket,job,turn,point,policy_version,mode,inputs,alternatives,selected,reason,applied,created)
                 VALUES(?,?,1,'dispatch','rules-1','record','{}','[]','single','默认项不应出现',0,0)",
                &[&ticket, &root],
            )?;
            Ok(())
        })
        .unwrap();
    fx.engine.start().unwrap();
    fx.finish(ticket, "done").await;
    let seen = feedback.lock().unwrap().clone();
    let lead: Vec<&Value> = seen.iter().filter(|(d, _)| *d == 0).map(|(_, v)| v).collect();
    assert!(lead.iter().all(|v| !v.is_null()));
    let first = lead[0];
    assert_eq!(first["mode"], "record");
    assert_eq!(first["policy_version"], "rules-1");
    assert_eq!(first["automatic_attempts"], json!({"used": 7, "budget": 40}));
    let recent = first["recent"].as_array().unwrap();
    assert_eq!(recent.len(), 5);
    assert!(recent.iter().all(|r| r["selected"] == "rediagnose" && r["applied"] == false));
    assert!(serde_json::to_string(first).unwrap().len() <= 4096);
    assert!(seen.iter().filter(|(d, _)| *d > 0).all(|(_, v)| v.is_null()));
    assert!(fx.shared.calls(|c| (c.packet["depth"] == 0).then(|| c.req.prompt.clone())).iter().all(|p| p.contains("policy_feedback, when present")));
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_feedback_is_omitted_when_there_is_nothing_to_say() {
    let feedback: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen = feedback.clone();
    let fx = Fx::new(
        script(move |c| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(c.full_packet()["policy_feedback"].clone());
                Ok("done".into())
            }
        }),
        t1_catalog(),
    );
    let ticket = fx.begin(fx.params()).await;
    fx.finish(ticket, "done").await;
    assert!(feedback.lock().unwrap().iter().all(Value::is_null));
    assert!(fx.shared.calls(|c| Some(c.req.prompt.clone())).iter().all(|p| !p.contains("policy_feedback, when present")));
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn route_outcomes_summarize_recent_collaborators_of_the_project() {
    let feedback: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen = feedback.clone();
    let fx = Fx::new(
        script(move |c| {
            let seen = seen.clone();
            async move {
                if c.depth() > 0 {
                    return Ok(if c.job() % 2 == 0 { "done".to_string() } else { wire("SA_REPORT", json!({"status": "partial", "summary": "half"})) });
                }
                if c.turn() == 1 {
                    return Ok(["one", "two"].iter().map(|k| send(json!({"id": k, "to": "codex"}))).collect::<Vec<_>>().join("\n"));
                }
                seen.lock().unwrap().push(c.full_packet()["policy_feedback"].clone());
                Ok("noted".into())
            }
        }),
        t1_catalog(),
    );
    let ticket = fx.begin(fx.params()).await;
    fx.finish(ticket, "partial").await;
    let seen = feedback.lock().unwrap().clone();
    let routes = seen[0]["routes"].as_array().unwrap();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0]["agent"], "codex");
    let total: i64 = ["done", "partial", "failed"].iter().map(|k| routes[0][*k].as_i64().unwrap()).sum();
    assert_eq!(total, 2);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mode_api_validates_and_defaults_to_record() {
    let fx = Fx::new(script(|_| async { Ok("done".into()) }), t1_catalog());
    let cwd = fx.project.to_string_lossy().into_owned();
    assert_eq!(fx.engine.execution_policy(&cwd).unwrap(), "record");
    assert!(fx.engine.set_execution_policy(&cwd, "auto").is_err());
    fx.engine.set_execution_policy(&cwd, "apply").unwrap();
    assert_eq!(fx.engine.execution_policy(&cwd).unwrap(), "apply");
    fx.engine.set_execution_policy(&cwd, "record").unwrap();
    assert_eq!(fx.engine.execution_policy(&cwd).unwrap(), "record");
    // A per-project switch never touches another project.
    assert_eq!(fx.engine.execution_policy("elsewhere").unwrap(), "record");
    fx.engine.close().await;
}

/// One recovery of a job turn spends the budget once, however many events
/// it journaled; retries and repairs of the same turn are separate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_budget_counts_distinct_recoveries_not_events() {
    let fx = Fx::new(script(|_| async move { Ok("done".into()) }), t1_catalog());
    let ticket = fx.create(fx.params().profile("full")).await;
    let job = root_id(&fx, ticket);
    let used = |fx: &Fx| fx.engine.db().with(|q| sa_engine::execution_policy::ticket_attempts(q, ticket)).unwrap();
    fx.engine
        .db()
        .with(|q| {
            for kind in ["turn.retry_scheduled", "auto.retry", "check.bounced"] {
                q.run("INSERT INTO events(ts,ticket,job,turn,kind,payload,sha256) VALUES(0,?,?,1,?,'{}','x')",
                    &[&ticket, &job, &kind])?;
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(used(&fx), 2);
    fx.engine.close().await;
}
