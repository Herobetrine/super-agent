//! Controlled trial runner (architecture §13, P5) with scripted harnesses:
//! fixture materialization, hidden acceptance, scripted requirement revision,
//! follow-up and engine restart, question answering and the run record.
mod common;
use common::*;
use sa_engine::adapter::{AdapterError, ErrorClass};
use sa_engine::{EngineOptions, trials};
use serde_json::json;
use std::{path::Path, sync::{Arc, Mutex}, time::Duration};

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn diff(a: &str, b: &str) -> Vec<String> {
    ["git", "diff", "--no-index", "--exit-code", "--ignore-all-space", "--", a, b].map(String::from).into()
}

/// One task: make `result.txt` equal the pinned `expected.txt`; the hidden
/// acceptance additionally requires `extra.txt`, which the scripted follow-up
/// asks for.
fn suite(dir: &Path) -> trials::LoadedSuite {
    let fixture = dir.join("fixtures/t");
    write(&fixture.join("project/expected.txt"), "42\n");
    write(&fixture.join("project/extra-expected.txt"), "extra\n");
    write(&fixture.join("acceptance/hidden-expected.txt"), "42\n");
    write(&fixture.join("solution/result.txt"), "42\n");
    write(&fixture.join("solution/extra.txt"), "extra\n");
    let suite = json!({
        "schema_version": 1, "id": "fixture-suite", "version": "1",
        "tasks": [{
            "id": "t", "category": "requirement_change", "fixture": "fixtures/t",
            "goal": "write result.txt", "lead": "codex",
            "requirements": [{"text": "result equals 42",
                "check": {"argv": diff("expected.txt", "result.txt"), "files": ["expected.txt"]}}],
            "gate": true,
            "acceptance": [
                {"argv": diff("hidden-expected.txt", "result.txt"), "files": ["hidden-expected.txt"]},
                {"argv": diff("extra-expected.txt", "extra.txt"), "files": []}
            ],
            "script": [
                {"when": {"after_root_turn": 1}, "action": {"type": "revise", "requirement": 0,
                    "text": "result equals 42 and extra.txt exists",
                    "check": {"argv": diff("extra-expected.txt", "extra.txt"), "files": ["extra-expected.txt"]}}},
                {"when": {"after_root_turn": 1}, "action": {"type": "follow", "body": "also write extra.txt"}},
                {"when": {"after_root_turn": 2}, "action": {"type": "restart"}}
            ],
            "minutes": 2
        }]
    });
    write(&dir.join("suite.json"), &serde_json::to_string(&suite).unwrap());
    trials::load(&dir.join("suite.json")).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn validation_and_a_scripted_run_produce_a_complete_record() {
    let dir = tempfile::tempdir().unwrap();
    let loaded = suite(dir.path());
    let validation = trials::validate(&loaded).unwrap();
    assert!(validation[0].valid, "{:?}", validation[0]);

    let shared = Arc::new(Shared {
        script: Mutex::new(script(|c| async move {
            let follow = c.packet["user_updates"].as_array().is_some_and(|u| !u.is_empty())
                || c.full_packet()["user_updates"].as_array().is_some_and(|u| !u.is_empty());
            std::fs::write(c.req.cwd.join("result.txt"), "42\n").unwrap();
            if follow {
                std::fs::write(c.req.cwd.join("extra.txt"), "extra\n").unwrap();
            }
            Ok(wire("SA_REPORT", json!({"status": "done", "summary": "written"})))
        })),
        catalog: Mutex::new(catalog_of(t1_catalog())),
        calls: Mutex::new(vec![]),
        panics: Mutex::new(vec![]),
    });
    let make = {
        let shared = shared.clone();
        move || adapters(&shared)
    };
    let options = |efficiency| EngineOptions {
        efficiency: sa_store::efficiency::shared(efficiency),
        tick: Duration::from_millis(20),
        ..Default::default()
    };
    let harness = trials::Harness { adapters: &make, options: &options, poll: Duration::from_millis(50) };
    let variant = trials::Variant { id: "baseline".into(), ..Default::default() };
    let task = loaded.suite.tasks[0].clone();
    let record = trials::run(&harness, &loaded, &task, &variant, 1, &dir.path().join("work")).await.unwrap();
    assert_eq!(record.verdict, "success", "{record:#?}");
    assert_eq!(record.delivery_status.as_deref(), Some("ready"));
    assert_eq!(record.interactions.scripted_revisions, 1);
    assert_eq!(record.interactions.scripted_follows, 1);
    assert_eq!(record.interactions.restarts, 1);
    assert_eq!(record.suite["sha256"].as_str().unwrap(), loaded.sha256);
    assert!(record.ledger["coverage"]["attempts"].as_u64().unwrap() >= 2);
    // Hidden acceptance files never entered the project.
    assert!(!dir.path().join("work/project/hidden-expected.txt").exists());

    let report = trials::aggregate(&[record]).unwrap();
    assert_eq!(report.groups[0].successes, 1);
    assert!(report.comparisons.is_empty());
}

/// A restart that lands mid-turn leaves the root interrupted. The runner
/// retries it like the operator would, so the run still reaches delivery.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_during_a_turn_is_retried_and_counted_as_an_interaction() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("fixtures/r");
    write(&fixture.join("project/expected.txt"), "42\n");
    write(&fixture.join("acceptance/hidden-expected.txt"), "42\n");
    write(&fixture.join("solution/result.txt"), "42\n");
    let suite = json!({
        "schema_version": 1, "id": "fixture-suite", "version": "1",
        "tasks": [{
            "id": "r", "category": "long_recovery", "fixture": "fixtures/r",
            "goal": "write result.txt", "lead": "codex",
            "requirements": [{"text": "result equals 42",
                "check": {"argv": diff("expected.txt", "result.txt"), "files": ["expected.txt"]}}],
            "gate": true,
            "acceptance": [{"argv": diff("hidden-expected.txt", "result.txt"), "files": ["hidden-expected.txt"]}],
            "script": [{"when": {"after_seconds": 1}, "action": {"type": "restart"}}],
            "minutes": 2
        }]
    });
    write(&dir.path().join("suite.json"), &serde_json::to_string(&suite).unwrap());
    let loaded = trials::load(&dir.path().join("suite.json")).unwrap();

    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let shared = Arc::new(Shared {
        script: Mutex::new(script({
            let calls = calls.clone();
            move |c| {
                let calls = calls.clone();
                async move {
                    if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        // The first turn is still running when the scheduler restarts.
                        c.cancel.cancelled().await;
                        return Err(AdapterError::new(ErrorClass::Cancelled, "已取消"));
                    }
                    std::fs::write(c.req.cwd.join("result.txt"), "42\n").unwrap();
                    Ok(wire("SA_REPORT", json!({"status": "done", "summary": "written"})))
                }
            }
        })),
        catalog: Mutex::new(catalog_of(t1_catalog())),
        calls: Mutex::new(vec![]),
        panics: Mutex::new(vec![]),
    });
    let make = {
        let shared = shared.clone();
        move || adapters(&shared)
    };
    let options = |efficiency| EngineOptions {
        efficiency: sa_store::efficiency::shared(efficiency),
        tick: Duration::from_millis(20),
        ..Default::default()
    };
    let harness = trials::Harness { adapters: &make, options: &options, poll: Duration::from_millis(50) };
    let variant = trials::Variant { id: "baseline".into(), ..Default::default() };
    let task = loaded.suite.tasks[0].clone();
    let record = trials::run(&harness, &loaded, &task, &variant, 1, &dir.path().join("work")).await.unwrap();
    assert_eq!(record.interactions.restarts, 1, "{record:#?}");
    assert_eq!(record.interactions.interrupted_retries, 1, "{record:#?}");
    assert_eq!(record.root_state, "done", "{record:#?}");
    assert_eq!(record.verdict, "success", "{record:#?}");
    assert_eq!(record.delivery_status.as_deref(), Some("ready"));
    let report = trials::aggregate(&[record]).unwrap();
    assert_eq!(report.groups[0].human_interactions, 1);
}
