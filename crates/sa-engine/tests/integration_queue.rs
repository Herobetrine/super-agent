//! Automatic preparation uses real disposable Git repositories and subprocesses.
//! Publication is always a separate operator call. No live model is involved.
mod common;
use common::*;
use sa_engine::integration_queue::IntegrationItem;
use serde_json::json;
use std::{path::Path, process::Command};

fn git(path: &Path, args: &[&str]) -> String {
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
    String::from_utf8(out.stdout).unwrap().trim().into()
}
fn init(path: &Path) {
    git(path, &["init"]);
    git(path, &["config", "core.autocrlf", "false"]);
    std::fs::write(path.join("base.txt"), "base\n").unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-m", "base"]);
}
fn passing() -> Vec<String> {
    vec!["git".into(), "diff".into(), "--check".into()]
}
fn item(fx: &Fx, ticket: i64) -> IntegrationItem {
    fx.engine
        .integration_queue(ticket)
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
}
async fn setup(fx: &Fx, commands: Vec<Vec<String>>) -> i64 {
    init(&fx.project);
    fx.engine
        .set_project_checks(&fx.project.to_string_lossy(), commands)
        .unwrap();
    let ticket = fx.create(fx.params().profile("full")).await;
    assert!(fx.engine.set_integration_queue(ticket, true).is_err());
    fx.engine.set_isolated_writers(ticket, true).unwrap();
    fx.engine.set_integration_queue(ticket, true).unwrap();
    fx.engine.start().unwrap();
    ticket
}
async fn wait(fx: &Fx, condition: impl Fn() -> bool) {
    common::until(
        &fx.engine,
        &fx.shared,
        std::time::Duration::from_secs(90),
        condition,
    )
    .await;
}
fn writer() -> Script {
    script(|c| async move {
        if c.depth() == 0 {
            if c.turn() == 1 {
                return Ok(send(json!({"id":"writer","write":true,"body":"write"})));
            }
        } else {
            std::fs::write(c.req.cwd.join("result.txt"), "result\n").unwrap();
        }
        Ok("done".into())
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn candidate_waits_for_confirmation_survives_restart_and_rejects_stale_inputs() {
    let mut fx = Fx::new(writer(), t1_catalog());
    let ticket = setup(&fx, vec![passing()]).await;
    let baseline = git(&fx.project, &["rev-parse", "HEAD"]);
    wait(&fx, || {
        fx.engine
            .integration_queue(ticket)
            .unwrap()
            .iter()
            .any(|i| i.state == "awaiting_confirmation")
    })
    .await;
    assert!(!fx.project.join("result.txt").exists());
    assert_eq!(git(&fx.project, &["rev-parse", "HEAD"]), baseline);
    let ready = item(&fx, ticket);
    let head = ready.validation["confirmation_token"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        fx.engine
            .confirm_integration(ready.job, "wrong-commit")
            .await
            .is_err()
    );
    fx.restart(quiet()).await;
    fx.engine.start().unwrap();
    assert_eq!(item(&fx, ticket).state, "awaiting_confirmation");
    std::fs::write(fx.project.join("user.txt"), "unsaved").unwrap();
    assert!(
        fx.engine
            .confirm_integration(ready.job, &head)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(fx.project.join("user.txt")).unwrap(),
        "unsaved"
    );
    std::fs::remove_file(fx.project.join("user.txt")).unwrap();
    fx.engine.revalidate_integration(ready.job).unwrap();
    wait(&fx, || item(&fx, ticket).state == "awaiting_confirmation").await;
    let next = item(&fx, ticket);
    assert_ne!(
        next.validation["confirmation_token"],
        ready.validation["confirmation_token"]
    );
    assert!(
        fx.engine
            .confirm_integration(next.job, &head)
            .await
            .is_err()
    );
    std::fs::write(fx.project.join("source-update.txt"), "new source\n").unwrap();
    git(&fx.project, &["add", "."]);
    git(&fx.project, &["commit", "-m", "source updated"]);
    assert!(
        fx.engine
            .confirm_integration(
                next.job,
                next.validation["confirmation_token"].as_str().unwrap()
            )
            .await
            .is_err()
    );
    assert!(!fx.project.join("result.txt").exists());
    fx.engine.revalidate_integration(next.job).unwrap();
    wait(&fx, || item(&fx, ticket).state == "awaiting_confirmation").await;
    let next = item(&fx, ticket);
    fx.engine
        .set_project_checks(
            &fx.project.to_string_lossy(),
            vec![vec!["git".into(), "--version".into()]],
        )
        .unwrap();
    assert!(
        fx.engine
            .confirm_integration(
                next.job,
                next.validation["confirmation_token"].as_str().unwrap()
            )
            .await
            .is_err()
    );
    fx.engine.revalidate_integration(next.job).unwrap();
    wait(&fx, || item(&fx, ticket).state == "awaiting_confirmation").await;
    let next = item(&fx, ticket);
    fx.engine
        .confirm_integration(
            next.job,
            next.validation["confirmation_token"].as_str().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(item(&fx, ticket).state, "merged");
    assert_eq!(
        std::fs::read_to_string(fx.project.join("result.txt")).unwrap(),
        "result\n"
    );
    assert_eq!(git(&fx.project, &["status", "--porcelain"]), "");
    // Simulate a process dying after Git published but before its journal
    // receipt. Confirmation reconciles the exact commit without another merge.
    let published_head = git(&fx.project, &["rev-parse", "HEAD"]);
    fx.engine
        .db()
        .tx(|q| {
            q.run(
                "UPDATE integration_queue SET state='publishing' WHERE job=?",
                &[&next.job],
            )?;
            q.run(
                "UPDATE job_workspaces SET state='awaiting_confirmation' WHERE job=?",
                &[&next.job],
            )?;
            Ok(())
        })
        .unwrap();
    fx.restart(quiet()).await;
    fx.engine.start().unwrap();
    fx.engine
        .confirm_integration(
            next.job,
            next.validation["confirmation_token"].as_str().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(git(&fx.project, &["rev-parse", "HEAD"]), published_head);
    assert_eq!(item(&fx, ticket).state, "merged");
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continued_prerequisite_supplies_latest_tree_and_shared_preparation_is_serialized() {
    let first_dependent = Gate::new();
    let fx = Fx::new(
        script({
            let first_dependent = first_dependent.clone();
            move |c| {
                let first_dependent = first_dependent.clone();
                async move {
                    if c.depth() == 0 {
                        return match c.turn() {
                        1=>Ok(send(json!({"id":"a","body":"a","write":true}))),
                        2=>Ok([
                            wire("SA_SEND",json!({"id":"fix-a","continue":c.full_packet()["reports"][0]["id"],"body":"update"})),
                            send(json!({"id":"b","body":"b","write":true,"after":["fix-a"]})),
                            send(json!({"id":"c","body":"c","write":true,"after":["fix-a"]})),
                        ].join("\n")),
                        _=>Ok("summary".into()),
                    };
                    }
                    let name = c.packet["assignment"].as_str().unwrap();
                    if name == "a" {
                        std::fs::write(
                            c.req.cwd.join("version.txt"),
                            format!("version {}", c.turn()),
                        )
                        .unwrap();
                    } else {
                        assert_eq!(
                            std::fs::read_to_string(c.req.cwd.join("version.txt")).unwrap(),
                            "version 2"
                        );
                        if name == "b" {
                            first_dependent.wait().await;
                        }
                        std::fs::write(c.req.cwd.join(format!("{name}.txt")), name).unwrap();
                    }
                    Ok("done".into())
                }
            }
        }),
        t1_catalog(),
    );
    let ticket = setup(&fx, vec![passing()]).await;
    wait(&fx, || {
        fx.shared.count(|call| call.packet["assignment"] == "b") == 1
    })
    .await;
    for _ in 0..16 {
        fx.engine.step();
        tokio::task::yield_now().await;
    }
    assert_eq!(fx.shared.count(|call| call.packet["assignment"] == "c"), 0);
    first_dependent.open();
    wait(&fx, || {
        fx.engine.integration_queue(ticket).unwrap().len() == 3
            && fx
                .engine
                .integration_queue(ticket)
                .unwrap()
                .iter()
                .all(|i| i.state == "awaiting_confirmation")
    })
    .await;
    assert_eq!(
        fx.int("SELECT count(*) FROM events WHERE kind='continued'"),
        1
    );
    assert!(!fx.project.join("version.txt").exists());
    fx.engine.close().await;
}

fn candidate_check() -> Vec<String> {
    if cfg!(windows) {
        ["powershell.exe","-NoProfile","-NonInteractive","-Command",
            "if ($PWD.Path -like '*integrations*') { Write-Output 'candidate check output'; if (!(Test-Path fixed.txt)) { exit 1 } }; exit 0"]
            .into_iter().map(str::to_string).collect()
    } else {
        ["sh","-c","case \"$PWD\" in */integrations/*) echo 'candidate check output'; test -f fixed.txt;; *) exit 0;; esac"]
            .into_iter().map(str::to_string).collect()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_candidate_repairs_original_session_and_exhaustion_finishes_after_chain() {
    for repaired in [true, false] {
        let fx = Fx::new(
            script(move |c| async move {
                if c.depth() == 0 {
                    if c.turn() == 1 {
                        return Ok([
                    send(json!({"id":"writer","write":true,"body":"write"})),
                    send(json!({"id":"dependent","write":true,"body":"dependent","after":["writer"]})),
                ].join("\n"));
                    }
                    return Ok("summary".into());
                }
                if c.packet["assignment"] == "write" {
                    if c.turn() > 1 {
                        assert_eq!(
                            c.req.session_id.as_deref(),
                            Some(format!("session-{}", c.job()).as_str())
                        );
                        assert!(
                            c.full_packet()["notes"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|n| n["body"]
                                    .as_str()
                                    .is_some_and(|s| s.contains("candidate check output")))
                        );
                        if repaired {
                            std::fs::write(c.req.cwd.join("fixed.txt"), "fixed\n").unwrap();
                        }
                    }
                    std::fs::write(c.req.cwd.join("result.txt"), "result\n").unwrap();
                } else {
                    assert!(repaired);
                    assert!(c.req.cwd.join("fixed.txt").exists());
                }
                Ok("done".into())
            }),
            t1_catalog(),
        );
        let ticket = setup(&fx, vec![candidate_check()]).await;
        if repaired {
            wait(&fx, || {
                fx.engine.integration_queue(ticket).unwrap().len() == 2
                    && fx
                        .engine
                        .integration_queue(ticket)
                        .unwrap()
                        .iter()
                        .all(|i| i.state == "awaiting_confirmation")
            })
            .await;
            assert_eq!(item(&fx, ticket).attempts, 1);
        } else {
            wait(&fx, || {
                fx.engine
                    .integration_queue(ticket)
                    .unwrap()
                    .iter()
                    .any(|i| i.state == "blocked")
            })
            .await;
            fx.finish(ticket, "partial").await;
            assert_eq!(item(&fx, ticket).attempts, 2);
            assert_eq!(
                fx.int("SELECT count(*) FROM jobs WHERE msgkey='dependent' AND state='cancelled'"),
                1
            );
            assert_eq!(fx.shared.count(|c| c.packet["assignment"] == "write"), 3);
        }
        assert!(!fx.project.join("result.txt").exists());
        fx.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn isolated_writer_starts_from_all_completed_prerequisite_branches_without_queue() {
    let fx = Fx::new(
        script(|c| async move {
            if c.depth() == 0 {
                if c.turn() == 1 {
                    return Ok([
                        send(json!({"id":"a","body":"a","write":true})),
                        send(json!({"id":"b","body":"b","write":true})),
                        send(json!({"id":"c","body":"c","write":true,"after":["a","b"]})),
                    ]
                    .join("\n"));
                }
            } else {
                let name = c.packet["assignment"].as_str().unwrap();
                if name == "c" {
                    assert!(c.req.cwd.join("a.txt").exists());
                    assert!(c.req.cwd.join("b.txt").exists());
                }
                std::fs::write(c.req.cwd.join(format!("{name}.txt")), name).unwrap();
            }
            Ok("done".into())
        }),
        t1_catalog(),
    );
    init(&fx.project);
    let ticket = fx.create(fx.params().profile("full")).await;
    fx.engine.set_isolated_writers(ticket, true).unwrap();
    fx.engine.start().unwrap();
    fx.finish(ticket, "done").await;
    assert!(fx.engine.integration_queue(ticket).unwrap().is_empty());
    assert!(!fx.project.join("a.txt").exists());
    fx.engine.close().await;
}
