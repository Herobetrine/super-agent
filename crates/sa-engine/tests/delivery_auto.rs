//! Operator-adopted delivery criteria run automatically after a writing job's
//! completion checks pass, on a snapshot of that job's own work directory.
mod common;
use common::*;
use serde_json::{Value, json};
use std::{path::Path, process::Command, time::Duration};

fn diff() -> Vec<String> {
    [
        "git",
        "diff",
        "--no-index",
        "--exit-code",
        "--ignore-all-space",
        "--",
        "expected.txt",
        "result.txt",
    ]
    .map(str::to_owned)
    .into()
}

fn git(path: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=Fixture", "-c", "user.email=fixture@localhost"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

/// A committed repository with the operator's expected result and, when
/// given, an ignore file. Ignored inputs are visible to project checks in the
/// work directory but are not part of an automatic snapshot.
fn init(project: &Path, ignore: Option<&str>) {
    git(project, &["init"]);
    git(project, &["config", "core.autocrlf", "false"]);
    std::fs::write(project.join("expected.txt"), "42\n").unwrap();
    if let Some(ignore) = ignore {
        std::fs::write(project.join(".gitignore"), ignore).unwrap();
    }
    git(project, &["add", "."]);
    git(project, &["commit", "-m", "base"]);
}

/// Requirement with the diff check adopted and `expected.txt` pinned.
fn adopt(fx: &Fx, ticket: i64) -> i64 {
    fx.engine
        .set_project_checks(&fx.project.to_string_lossy(), vec![diff()])
        .unwrap();
    let summary = fx
        .engine
        .save_requirement(ticket, 0, None, "Result equals 42".into(), None, None, None)
        .unwrap();
    let requirement = summary.requirements[0].id;
    fx.engine
        .adopt_delivery_check_with_files(ticket, 1, requirement, diff(), vec!["expected.txt".into()])
        .unwrap();
    requirement
}

fn checks_for(fx: &Fx, job: i64) -> Value {
    payloads(fx, &format!("SELECT payload FROM job_checks WHERE job={job}"))[0].clone()
}

fn writer_job(fx: &Fx) -> i64 {
    fx.all("SELECT id FROM jobs WHERE parent IS NOT NULL AND writing=1")[0]["id"]
        .as_i64()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn record_mode_captures_the_job_directory_runs_adopted_checks_and_reports_to_the_lead() {
    let fx = Fx::new(
        script(|c| async move {
            if c.depth() == 0 {
                if c.turn() == 1 {
                    return Ok(send(json!({"id": "writer", "write": true})));
                }
                let delivery = &c.full_packet()["reports"][0]["checks"]["delivery"];
                assert_eq!(delivery["status"], "ready", "{delivery}");
                assert_eq!(delivery["requirements"][0]["status"], "pass");
                assert_eq!(delivery["gate"], false);
                return Ok(wire("SA_REPORT", json!({"status": "done", "summary": "ok"})));
            }
            std::fs::write(c.req.cwd.join("result.txt"), " 42 \n").unwrap();
            Ok(wire("SA_REPORT", json!({"status": "done", "summary": "written"})))
        }),
        t1_catalog(),
    );
    init(&fx.project, None);
    let ticket = fx.create(fx.params().profile("full")).await;
    let requirement = adopt(&fx, ticket);
    fx.engine.start().unwrap();
    fx.finish(ticket, "done").await;
    let job = writer_job(&fx);
    let checks = checks_for(&fx, job);
    assert_eq!(checks["ok"], true);
    let delivery = &checks["delivery"];
    assert_eq!(delivery["status"], "ready", "{delivery}");
    assert_eq!(delivery["selection"], "project_files");
    assert_eq!(delivery["isolated"], false);
    assert_eq!(delivery["reused"], false);
    assert!(delivery.get("assessment_id").is_none());
    // Record mode: no gate assessment, no bounce, one passing attempt.
    assert_eq!(fx.int("SELECT count(*) FROM delivery_assessments"), 0);
    assert_eq!(fx.int("SELECT count(*) FROM check_bounces"), 0);
    assert_eq!(
        fx.int("SELECT count(*) FROM delivery_behavior_results WHERE status='pass'"),
        1
    );
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='delivery.auto'"), 1);
    let summary = fx.engine.delivery_summary(ticket).unwrap();
    let snapshot = summary.latest_snapshot.unwrap();
    let origin = snapshot.origin.unwrap();
    assert_eq!((origin.job, origin.isolated), (job, false));
    assert!(snapshot.files.iter().any(|file| file.path == "result.txt"));
    let item = summary.requirements.iter().find(|r| r.id == requirement).unwrap();
    assert_eq!(item.status, "pass");
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enabled_gate_returns_a_failed_required_criterion_to_the_same_job_then_records_ready() {
    let fx = Fx::new(
        script(|c| async move {
            if c.depth() == 0 {
                if c.turn() == 1 {
                    return Ok(send(json!({"id": "writer", "write": true})));
                }
                let report = &c.full_packet()["reports"][0];
                let checks = &report["checks"];
                assert_eq!(checks["delivery"]["status"], "ready", "{report}");
                assert_eq!(checks["attempt"], 2);
                return Ok(wire("SA_REPORT", json!({"status": "done", "summary": "ok"})));
            }
            if c.turn() == 1 {
                // Passes in the work directory only because of an ignored file.
                std::fs::write(c.req.cwd.join("result.txt"), "42\n").unwrap();
                // A tracked change, so the work tree fingerprint moves.
                std::fs::write(c.req.cwd.join("notes.txt"), "draft\n").unwrap();
            } else {
                let packet = c.full_packet();
                assert_eq!(packet["checks"]["ok"], false);
                assert_eq!(packet["checks"]["delivery"]["status"], "blocked");
                let notes = packet["notes"].to_string();
                assert!(notes.contains("Delivery requirement"), "{notes}");
                std::fs::write(c.req.cwd.join(".gitignore"), "").unwrap();
            }
            Ok(wire("SA_REPORT", json!({"status": "done", "summary": "written"})))
        }),
        t1_catalog(),
    );
    init(&fx.project, Some("result.txt\n"));
    let ticket = fx.create(fx.params().profile("full")).await;
    adopt(&fx, ticket);
    fx.engine.set_delivery_gate(ticket, 0, true).unwrap();
    fx.engine.start().unwrap();
    fx.finish(ticket, "done").await;
    let job = writer_job(&fx);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='check.bounced'"), 1);
    assert_eq!(
        fx.int("SELECT count(*) FROM delivery_behavior_results WHERE status='fail'"),
        1
    );
    let statuses: Vec<String> = fx
        .all("SELECT status FROM delivery_assessments ORDER BY id")
        .iter()
        .map(|row| row["status"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(statuses, ["blocked", "ready"]);
    let checks = checks_for(&fx, job);
    assert_eq!(checks["ok"], true);
    assert_eq!(checks["delivery"]["status"], "ready");
    let summary = fx.engine.delivery_summary(ticket).unwrap();
    assert_eq!(summary.gate.status, "ready", "{}", summary.gate.reason);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn isolated_worktree_is_captured_before_merge_and_shown_as_not_yet_in_the_project() {
    let fx = Fx::new(
        script(|c| async move {
            if c.depth() == 0 {
                if c.turn() == 1 {
                    return Ok(send(json!({"id": "writer", "write": true, "body": "write"})));
                }
                return Ok("done".into());
            }
            std::fs::write(c.req.cwd.join("result.txt"), "42\n").unwrap();
            Ok("done".into())
        }),
        t1_catalog(),
    );
    init(&fx.project, None);
    let ticket = fx.create(fx.params().profile("full")).await;
    let requirement = adopt(&fx, ticket);
    fx.engine.set_isolated_writers(ticket, true).unwrap();
    fx.engine.set_integration_queue(ticket, true).unwrap();
    fx.engine.start().unwrap();
    until(&fx.engine, &fx.shared, Duration::from_secs(90), || {
        fx.int("SELECT count(*) FROM events WHERE kind='delivery.auto'") == 1
    })
    .await;
    let summary = fx.engine.delivery_summary(ticket).unwrap();
    let snapshot = summary.latest_snapshot.unwrap();
    let origin = snapshot.origin.unwrap();
    assert!(origin.isolated);
    assert!(snapshot.files.iter().any(|file| file.path == "result.txt"));
    assert!(!fx.project.join("result.txt").exists());
    let frozen = snapshot.requirements.iter().find(|r| r.id == requirement).unwrap();
    assert_eq!(frozen.status, "pass");
    // The project does not contain the candidate yet: current view is stale.
    let item = summary.requirements.iter().find(|r| r.id == requirement).unwrap();
    assert_eq!(item.status, "stale");
    let reason = &item.evidence.as_ref().unwrap().reason;
    assert!(reason.contains("隔离工作区"), "{reason}");
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tickets_without_criteria_keep_the_original_check_receipt() {
    let fx = Fx::new(
        script(|c| async move {
            if c.depth() == 0 && c.turn() == 1 {
                return Ok(send(json!({"id": "writer", "write": true})));
            }
            if c.depth() > 0 {
                std::fs::write(c.req.cwd.join("result.txt"), "42\n").unwrap();
            }
            Ok(wire("SA_REPORT", json!({"status": "done", "summary": "ok"})))
        }),
        t1_catalog(),
    );
    init(&fx.project, None);
    fx.engine
        .set_project_checks(&fx.project.to_string_lossy(), vec![diff()])
        .unwrap();
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "done").await;
    let checks = checks_for(&fx, writer_job(&fx));
    assert_eq!(checks["ok"], true);
    assert!(checks.get("delivery").is_none(), "{checks}");
    assert_eq!(fx.int("SELECT count(*) FROM delivery_snapshots"), 0);
    fx.engine.close().await;
}
