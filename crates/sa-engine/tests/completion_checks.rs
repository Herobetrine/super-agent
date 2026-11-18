//! Completion gates run outside the native harness and feed failures back to it.

mod common;

use common::*;
use serde_json::{Value, json};
use std::{path::Path, process::Command, time::Duration};

fn report(status: &str) -> String {
    wire(
        "SA_REPORT",
        json!({"status": status, "summary": "fixture result"}),
    )
}

fn finish() -> String {
    wire(
        "SA_FINISH",
        json!({"status": "done", "summary": "verified fixture"}),
    )
}

fn check_events(fx: &Fx, kind: &str) -> i64 {
    fx.int(&format!("SELECT count(*) FROM events WHERE kind='{kind}'"))
}

fn configure(fx: &Fx, checks: Vec<Vec<String>>) {
    fx.engine
        .set_project_checks(&fx.project.to_string_lossy(), checks)
        .unwrap();
}

fn passing() -> Vec<String> {
    vec!["git".into(), "--version".into()]
}

fn failing() -> Vec<String> {
    vec!["git".into(), "sa-fixture-missing-command".into()]
}

fn git(path: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@localhost",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init(path: &Path) {
    git(path, &["init"]);
    git(path, &["config", "core.autocrlf", "false"]);
    std::fs::write(path.join("base.txt"), "base\n").unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-m", "fixture"]);
}

fn quoted(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

fn shell(windows: String, unix: String) -> Vec<String> {
    if cfg!(windows) {
        vec![
            "powershell.exe".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            windows,
        ]
    } else {
        vec!["sh".into(), "-c".into(), unix]
    }
}

fn ps_path(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

fn marker_check(marker: &Path) -> Vec<String> {
    shell(
        format!(
            "Add-Content -LiteralPath {} -Value run; exit 0",
            ps_path(marker)
        ),
        format!("printf 'run\\n' >> {}; exit 0", quoted(marker)),
    )
}

fn checks_for(fx: &Fx, job: i64) -> Value {
    payloads(
        fx,
        &format!("SELECT payload FROM job_checks WHERE job={job}"),
    )[0]
    .clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unconfigured_projects_preserve_completion_without_check_events() {
    let fx = Fx::new(script(|_| async { Ok(report("done")) }), t1_catalog());
    assert!(
        fx.engine
            .project_checks(&fx.project.to_string_lossy())
            .unwrap()
            .is_empty()
    );
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "done").await;
    assert_eq!(
        fx.int("SELECT count(*) FROM events WHERE kind LIKE 'check.%'"),
        0
    );
    assert_eq!(fx.int("SELECT count(*) FROM job_checks"), 0);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writer_checks_are_durable_and_reach_the_leads_report_but_readers_skip() {
    let fx = Fx::new(
        script(|c| async move {
            if c.depth() > 0 {
                assert_eq!(c.full_packet()["completion_checks"], json!([passing()]));
                if c.packet["assignment"] == "review" {
                    assert_eq!(c.full_packet()["prerequisites"][0]["checks"]["ok"], true);
                    return Ok(report("done"));
                }
                std::fs::write(c.req.cwd.join("written.txt"), "written\n").unwrap();
                return Ok(report("done"));
            }
            if c.turn() == 1 {
                return Ok([
                    send(json!({"id": "writer", "body": "writer", "write": true})),
                    send(json!({"id": "review", "body": "review", "after": ["writer"]})),
                ]
                .join("\n"));
            }
            let packet = c.full_packet();
            assert_eq!(packet["reports"][0]["checks"]["ok"], true);
            assert!(packet["reports"][1]["checks"].is_null());
            assert_eq!(
                packet["reports"][0]["checks"]["runs"][0]["argv"],
                json!(passing())
            );
            Ok(report("done"))
        }),
        t1_catalog(),
    );
    configure(&fx, vec![passing()]);
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "done").await;
    let child = fx.all("SELECT id FROM jobs WHERE parent IS NOT NULL AND writing=1")[0]["id"]
        .as_i64()
        .unwrap();
    let checks = checks_for(&fx, child);
    assert_eq!(checks["ok"], true);
    assert_eq!(checks["cached"], false);
    assert_eq!(checks["attempt"], 1);
    assert_eq!(checks["runs"][0]["exit"], 0);
    assert!(checks["runs"][0]["seconds"].is_number());
    assert_eq!(check_events(&fx, "check.bounced"), 0);
    fx.engine.close().await;

    let fx = Fx::new(script(|_| async { Ok(report("done")) }), t1_catalog());
    configure(&fx, vec![failing()]);
    let ticket = fx.begin(fx.params().profile("readonly")).await;
    fx.finish(ticket, "done").await;
    assert_eq!(
        fx.int("SELECT count(*) FROM events WHERE kind LIKE 'check.%'"),
        0
    );
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_checks_return_to_the_same_native_session_then_pass() {
    let fx = Fx::new(
        script(|c| async move {
            if c.depth() == 0 {
                if c.turn() == 1 {
                    return Ok(send(json!({"id": "writer", "write": true})));
                }
                let packet = c.full_packet();
                assert_eq!(packet["reports"][0]["state"], "done");
                assert_eq!(packet["reports"][0]["checks"]["ok"], true);
                assert_eq!(packet["reports"][0]["checks"]["attempt"], 2);
                return Ok(report("partial"));
            }
            if c.turn() == 1 {
                std::fs::write(c.req.cwd.join("implementation.txt"), "needs repair\n").unwrap();
            }
            if c.turn() == 2 {
                assert_eq!(
                    c.req.session_id.as_deref(),
                    Some(format!("session-{}", c.job()).as_str())
                );
                let packet = c.full_packet();
                assert_eq!(packet["checks"]["ok"], false);
                assert_eq!(packet["checks"]["attempt"], 1);
                assert!(!packet["notes"].as_array().unwrap().is_empty());
                std::fs::write(c.req.cwd.join("fixed"), "fixed\n").unwrap();
            }
            Ok(report("done"))
        }),
        t1_catalog(),
    );
    configure(
        &fx,
        vec![shell(
            "if (Test-Path -LiteralPath fixed) { exit 0 } else { exit 1 }".into(),
            "test -f fixed".into(),
        )],
    );
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "partial").await;
    let child = fx.all("SELECT id FROM jobs WHERE parent IS NOT NULL AND writing=1")[0]["id"]
        .as_i64()
        .unwrap();
    assert_eq!(fx.shared.count(|c| c.packet["depth"] == 1), 2);
    assert_eq!(check_events(&fx, "check.bounced"), 1);
    assert_eq!(check_events(&fx, "check.started"), 2);
    let checks = checks_for(&fx, child);
    assert_eq!(checks["ok"], true);
    assert_eq!(checks["attempt"], 2);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commands_run_in_order_and_stop_at_the_first_failure() {
    let fx = Fx::new(script(|c| async move {
        if c.turn() == 1 {
            std::fs::write(c.req.cwd.join("implementation.txt"), "check this change\n").unwrap();
        }
        Ok(report("done"))
    }), t1_catalog());
    let first_marker = fx.dir.path().join("first-check");
    let skipped_marker = fx.dir.path().join("skipped-check");
    let first = marker_check(&first_marker);
    let failure = failing();
    configure(
        &fx,
        vec![
            first.clone(),
            failure.clone(),
            marker_check(&skipped_marker),
        ],
    );
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "partial").await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    let checks = checks_for(&fx, root);
    let runs = checks["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["argv"], json!(first));
    assert_eq!(runs[0]["ok"], true);
    assert_eq!(runs[1]["argv"], json!(failure));
    assert_eq!(runs[1]["ok"], false);
    assert_eq!(
        std::fs::read_to_string(&first_marker)
            .unwrap()
            .lines()
            .count(),
        3
    );
    assert!(
        !skipped_marker.exists(),
        "a command after the first failure ran"
    );
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn third_failure_is_partial_and_lead_receives_failed_checks() {
    let fx = Fx::new(
        script(|c| async move {
            if c.depth() > 0 {
                if c.turn() == 1 {
                    std::fs::write(c.req.cwd.join("implementation.txt"), "needs repair\n").unwrap();
                }
                if c.turn() > 1 {
                    assert_eq!(
                        c.req.session_id.as_deref(),
                        Some(format!("session-{}", c.job()).as_str())
                    );
                }
                return Ok(report("done"));
            }
            if c.turn() == 1 {
                return Ok(send(json!({"id": "writer", "write": true})));
            }
            let packet = c.full_packet();
            assert_eq!(packet["reports"][0]["state"], "partial");
            assert_eq!(packet["reports"][0]["checks"]["ok"], false);
            assert_eq!(packet["reports"][0]["checks"]["attempt"], 3);
            Ok(report("partial"))
        }),
        t1_catalog(),
    );
    configure(&fx, vec![failing()]);
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "partial").await;
    assert_eq!(fx.shared.count(|c| c.packet["depth"] == 1), 3);
    assert_eq!(check_events(&fx, "check.bounced"), 2);
    assert_eq!(check_events(&fx, "check.result"), 3);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outstanding_bounces_recheck_an_unchanged_git_tree_until_partial() {
    let fx = Fx::new(
        script(|c| async move {
            if c.turn() == 1 {
                std::fs::write(c.req.cwd.join("base.txt"), "changed\n").unwrap();
            } else {
                // Both retries leave the Git tree untouched. A prior failure
                // still needs a new check even when the fix could be external.
                assert_eq!(c.full_packet()["checks"]["attempt"], c.turn() - 1);
                assert_eq!(c.full_packet()["checks"]["ok"], false);
                assert_eq!(
                    std::fs::read_to_string(c.req.cwd.join("base.txt")).unwrap(),
                    "changed\n"
                );
            }
            Ok(report("done"))
        }),
        t1_catalog(),
    );
    init(&fx.project);
    configure(&fx, vec![failing()]);
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "partial").await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    assert_eq!(fx.shared.count(|_| true), 3);
    assert_eq!(check_events(&fx, "check.started"), 3);
    assert_eq!(check_events(&fx, "check.result"), 3);
    assert_eq!(check_events(&fx, "check.bounced"), 2);
    let checks = checks_for(&fx, root);
    assert_eq!(checks["ok"], false);
    assert_eq!(checks["cached"], false);
    assert_eq!(checks["attempt"], 3);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='reply' AND json_extract(payload,'$.state')='done'"), 0);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_outstanding_bounce_can_use_a_recorded_pass_for_the_same_tree() {
    let fx = Fx::new(
        script(|c| async move {
            if c.turn() == 1 {
                std::fs::write(c.req.cwd.join("base.txt"), "passed\n").unwrap();
            }
            Ok(report("done"))
        }),
        t1_catalog(),
    );
    init(&fx.project);
    let marker = fx.dir.path().join("cached-bounce-check");
    configure(&fx, vec![marker_check(&marker)]);
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "done").await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    // Retain the real passing fingerprint while restoring an outstanding
    // bounce receipt. Its next unchanged claim may still reuse that evidence.
    fx.engine
        .db()
        .tx(|q| {
            q.run(
                "INSERT INTO check_bounces(job,attempt) VALUES(?,1)",
                &[&root],
            )?;
            Ok(())
        })
        .unwrap();
    fx.engine
        .follow(ticket, "check the existing passing tree", false)
        .unwrap();
    fx.finish(ticket, "done").await;
    let checks = checks_for(&fx, root);
    assert_eq!(checks["ok"], true);
    assert_eq!(checks["cached"], true);
    assert_eq!(checks["attempt"], 2);
    assert_eq!(checks["runs"], json!([]));
    assert_eq!(check_events(&fx, "check.started"), 1);
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
    assert_eq!(fx.int("SELECT count(*) FROM check_bounces"), 0);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_intermediate_partial_report_clears_receipt_without_resetting_bounce_budget() {
    let fx = Fx::new(
        script(|c| async move {
            if c.turn() == 1 {
                std::fs::write(c.req.cwd.join("implementation.txt"), "needs repair\n").unwrap();
            }
            if c.turn() == 2 {
                return Ok(report("partial"));
            }
            if c.turn() == 3 {
                assert!(c.full_packet()["checks"].is_null());
            }
            if c.turn() == 4 {
                assert_eq!(c.full_packet()["checks"]["attempt"], 2);
            }
            Ok(report("done"))
        }),
        t1_catalog(),
    );
    configure(&fx, vec![failing()]);
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "partial").await;
    assert_eq!(fx.shared.count(|_| true), 2);
    assert_eq!(fx.int("SELECT count(*) FROM job_checks"), 0);
    assert_eq!(fx.int("SELECT attempt FROM check_bounces"), 1);
    fx.engine
        .follow(ticket, "retry the remaining check failure", false)
        .unwrap();
    fx.finish(ticket, "partial").await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    assert_eq!(checks_for(&fx, root)["attempt"], 3);
    assert_eq!(checks_for(&fx, root)["ok"], false);
    assert_eq!(check_events(&fx, "check.bounced"), 2);
    assert_eq!(fx.shared.count(|_| true), 4);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unchanged_git_turn_skips_and_restoring_a_passed_tree_uses_cache() {
    let fx = Fx::new(
        script(|c| async move {
            // Turn two starts with an external edit and restores the exact tree
            // checked in turn one. Turn three makes no project changes at all.
            if c.turn() < 3 {
                std::fs::write(c.req.cwd.join("base.txt"), "passed\n").unwrap();
            }
            Ok(report("done"))
        }),
        t1_catalog(),
    );
    init(&fx.project);
    let marker = fx.dir.path().join("checks-ran.txt");
    configure(&fx, vec![marker_check(&marker)]);
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "done").await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    assert_eq!(check_events(&fx, "check.started"), 1);
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
    std::fs::write(fx.project.join("base.txt"), "external edit\n").unwrap();
    fx.engine
        .follow(ticket, "restore known passing tree", false)
        .unwrap();
    fx.finish(ticket, "done").await;
    assert_eq!(checks_for(&fx, root)["cached"], true);
    assert_eq!(check_events(&fx, "check.started"), 1);
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
    let before = check_events(&fx, "check.result");
    fx.engine
        .follow(ticket, "inspect without changing anything", false)
        .unwrap();
    fx.finish(ticket, "done").await;
    assert_eq!(check_events(&fx, "check.started"), 1);
    assert_eq!(check_events(&fx, "check.result"), before);
    assert_eq!(fx.int("SELECT count(*) FROM job_checks"), 0);
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_blocked_questions_and_outstanding_children_do_not_run_checks() {
    for status in ["partial", "blocked"] {
        let fx = Fx::new(
            script(move |_| async move { Ok(report(status)) }),
            t1_catalog(),
        );
        configure(&fx, vec![failing()]);
        let ticket = fx.begin(fx.params().profile("full")).await;
        fx.finish(ticket, "partial").await;
        assert_eq!(
            fx.int("SELECT count(*) FROM events WHERE kind LIKE 'check.%'"),
            0
        );
        fx.engine.close().await;
    }
    let fx = Fx::new(
        script(|_| async {
            Ok(wire(
                "SA_ASK",
                json!({"id": "scope", "question": "which scope?", "options": ["continue", "stop"], "default": "wait"}),
            ))
        }),
        t1_catalog(),
    );
    configure(&fx, vec![failing()]);
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.until(|| fx.engine.active_count() == 0 && !fx.engine.questions(ticket).unwrap().is_empty())
        .await;
    assert_eq!(
        fx.int("SELECT count(*) FROM events WHERE kind LIKE 'check.%'"),
        0
    );
    fx.engine.cancel(ticket).unwrap();
    fx.engine.close().await;

    let gate = Gate::new();
    let child_gate = gate.clone();
    let fx = Fx::new(
        script(move |c| {
            let gate = child_gate.clone();
            async move {
                if c.depth() > 0 {
                    gate.wait().await;
                    return Ok(report("partial"));
                }
                if c.turn() == 1 {
                    return Ok(send(json!({"id": "reader"})));
                }
                Ok(report("partial"))
            }
        }),
        t1_catalog(),
    );
    configure(&fx, vec![failing()]);
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.until(|| fx.shared.count(|c| c.packet["depth"] == 1) == 1)
        .await;
    assert_eq!(
        fx.int("SELECT count(*) FROM events WHERE kind LIKE 'check.%'"),
        0
    );
    gate.open();
    fx.finish(ticket, "partial").await;
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_root_only_gates_done_finish_and_never_emits_a_failed_milestone() {
    let fx = Fx::new(
        script(|c| async move {
            assert_eq!(c.phase(), Some("work"));
            if c.turn() == 1 {
                return Ok(wire("SA_NEXT", json!({"body": "continue fixture"})));
            }
            if c.turn() == 2 {
                std::fs::write(c.req.cwd.join("implementation.txt"), "check before milestone\n").unwrap();
            }
            Ok(finish())
        }),
        t1_catalog(),
    );
    configure(&fx, vec![failing()]);
    let ticket = fx.create(fx.params().profile("auto")).await;
    fx.engine
        .db()
        .with(|q| {
            q.run(
                "UPDATE continuous_runs SET phase='work' WHERE ticket=?",
                &[&ticket],
            )
        })
        .unwrap();
    fx.engine.start().unwrap();
    fx.until(|| fx.engine.active_count() == 0 && !fx.engine.questions(ticket).unwrap().is_empty())
        .await;
    assert_eq!(fx.shared.count(|_| true), 4);
    assert_eq!(check_events(&fx, "check.started"), 3);
    assert_eq!(check_events(&fx, "check.bounced"), 2);
    assert_eq!(check_events(&fx, "auto.milestone"), 0);
    let questions = fx.engine.questions(ticket).unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0]["origin"], "platform");
    assert_eq!(questions[0]["parked"], 1);
    for _ in 0..20 {
        assert_eq!(fx.engine.step(), 0);
    }
    assert_eq!(fx.shared.count(|_| true), 4);
    fx.engine.cancel(ticket).unwrap();
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_kills_the_running_checks_process_tree() {
    let fx = Fx::new(script(|c| async move {
        std::fs::write(c.req.cwd.join("implementation.txt"), "check this change\n").unwrap();
        Ok(report("done"))
    }), t1_catalog());
    let started = fx.dir.path().join("started");
    let survived = fx.dir.path().join("survived");
    let child = fx.dir.path().join(if cfg!(windows) {
        "check-child.ps1"
    } else {
        "check-child.sh"
    });
    let argv = if cfg!(windows) {
        std::fs::write(
            &child,
            format!(
                "Start-Sleep -Seconds 3\r\nSet-Content -LiteralPath {} -Value survived\r\n",
                ps_path(&survived)
            ),
        )
        .unwrap();
        shell(
            format!(
                "$worker = Start-Process -FilePath powershell.exe -WindowStyle Hidden -ArgumentList @('-NoProfile','-NonInteractive','-File','\"{}\"') -PassThru; Set-Content -LiteralPath {} -Value started; Wait-Process -Id $worker.Id",
                child.to_string_lossy(),
                ps_path(&started)
            ),
            String::new(),
        )
    } else {
        std::fs::write(
            &child,
            format!("sleep 3\nprintf survived > {}\n", quoted(&survived)),
        )
        .unwrap();
        shell(
            String::new(),
            format!(
                "sh {} & printf started > {}; wait",
                quoted(&child),
                quoted(&started)
            ),
        )
    };
    configure(&fx, vec![argv]);
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.until(|| started.is_file()).await;
    let cancelled_at = std::time::Instant::now();
    fx.engine.cancel(ticket).unwrap();
    fx.until(|| fx.engine.active_count() == 0).await;
    assert!(
        cancelled_at.elapsed() < Duration::from_secs(2),
        "completion check cancellation did not finish promptly"
    );
    assert_eq!(root_state(&fx.engine, ticket), "cancelled");
    assert_eq!(fx.int("SELECT count(*) FROM job_checks"), 0);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(
        !survived.exists(),
        "a descendant of the cancelled check survived"
    );
    assert_eq!(check_events(&fx, "check.bounced"), 0);
    assert_eq!(fx.shared.count(|_| true), 1);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn project_checks_validate_and_persist_without_replacing_valid_settings() {
    let mut fx = Fx::new(script(|_| async { Ok("done".into()) }), t1_catalog());
    let checks = vec![passing()];
    configure(&fx, checks.clone());
    for invalid in [
        vec![passing(); 5],
        vec![vec![]],
        vec![vec![String::new()]],
        vec![vec!["git".into(); 65]],
        vec![vec!["git\0--version".into()]],
        vec![vec!["git".into(), "x".repeat(100_000)]],
    ] {
        assert!(
            fx.engine
                .set_project_checks(&fx.project.to_string_lossy(), invalid)
                .is_err()
        );
        assert_eq!(
            fx.engine
                .project_checks(&fx.project.to_string_lossy())
                .unwrap(),
            checks
        );
    }
    fx.restart(quiet()).await;
    assert_eq!(
        fx.engine
            .project_checks(&fx.project.to_string_lossy())
            .unwrap(),
        checks
    );
    configure(&fx, vec![]);
    assert!(
        fx.engine
            .project_checks(&fx.project.to_string_lossy())
            .unwrap()
            .is_empty()
    );
    fx.engine.close().await;
}
