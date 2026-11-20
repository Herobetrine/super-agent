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
    // Confirming now would carry over to the revalidated candidate (see
    // source_commit_after_confirmation_revalidates_and_merges_the_same_task);
    // here the operator revalidates without confirming.
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
    // A crash between the Git publication and its receipt is reconciled in
    // tests/operations.rs (integration.publish.after_effect). After a recorded
    // merge the candidate and the task workspace are removed.
    let workspace = fx.engine.db().with(|q| sa_engine::workspaces::get(q, next.job)).unwrap().unwrap();
    let candidate = item(&fx, ticket).validation["candidate"].as_str().unwrap().to_string();
    wait(&fx, || !Path::new(&workspace.path).exists() && !Path::new(&candidate).exists()).await;
    assert_eq!(fx.int("SELECT count(*) FROM integration_confirmations"), 0);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continued_prerequisite_supplies_latest_tree_and_dependents_run_together_after_preparation() {
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
    // The prerequisite is leased only while a dependent builds its own
    // snapshot (§10.1): the second dependent runs while the first still works.
    wait(&fx, || {
        fx.shared.count(|call| call.packet["assignment"] == "c") == 1
    })
    .await;
    assert!(!first_dependent.is_open());
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
    // Platform commits on the task branches name the task, not just "merge".
    let branches = git(&fx.project, &["for-each-ref", "--format=%(refname:short)", "refs/heads/codex/"]);
    let subjects: Vec<String> = branches
        .lines()
        .flat_map(|b| git(&fx.project, &["log", "--format=%s", b]).lines().map(String::from).collect::<Vec<_>>())
        .collect();
    assert!(
        subjects.iter().any(|s| s.starts_with("Checkpoint task #") && s.contains("(ticket ")),
        "{subjects:?}"
    );
    assert!(
        subjects.iter().any(|s| s.starts_with("Merge prerequisite task #") && s.contains(" into task #")),
        "{subjects:?}"
    );
    fx.engine.close().await;
}

/// A partial isolated writer leaves an unfinished branch; dependents that
/// would merge it are skipped, unlike partial prerequisites in the shared tree.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_isolated_prerequisite_still_skips_its_dependent_writer() {
    let fx = Fx::new(
        script(|c| async move {
            if c.depth() == 0 {
                if c.turn() == 1 {
                    return Ok([
                        send(json!({"id":"a","body":"a","write":true})),
                        send(json!({"id":"c","body":"c","write":true,"after":["a"]})),
                    ]
                    .join("
"));
                }
                return Ok("summary".into());
            }
            assert_eq!(c.packet["assignment"], "a", "the dependent must not start");
            std::fs::write(c.req.cwd.join("a.txt"), "a").unwrap();
            Ok(wire("SA_REPORT", json!({"status":"partial","summary":"half done"})))
        }),
        t1_catalog(),
    );
    init(&fx.project);
    let ticket = fx.create(fx.params().profile("full")).await;
    fx.engine.set_isolated_writers(ticket, true).unwrap();
    fx.engine.start().unwrap();
    wait(&fx, || {
        fx.int("SELECT count(*) FROM jobs WHERE msgkey='c' AND state='cancelled'") == 1
    })
    .await;
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='skipped'"), 1);
    fx.engine.close().await;
}

/// The candidate carries exactly the task's tree when the source has not
/// moved, so the project checks run once, in the clean candidate (§10.2).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identical_candidate_tree_runs_the_checks_once_and_records_the_reuse() {
    let fx = Fx::new(writer(), t1_catalog());
    let marker = fx.dir.path().join("integration-runs.txt");
    let check = if cfg!(windows) {
        vec![
            "powershell.exe".into(), "-NoProfile".into(), "-NonInteractive".into(), "-Command".into(),
            format!("Add-Content -LiteralPath '{}' -Value $PWD.Path; exit 0",
                marker.to_string_lossy().replace('\'', "''")),
        ]
    } else {
        vec!["sh".into(), "-c".into(), format!("pwd >> '{}'", marker.to_string_lossy())]
    };
    let ticket = setup(&fx, vec![check]).await;
    wait(&fx, || {
        fx.engine
            .integration_queue(ticket)
            .unwrap()
            .iter()
            .any(|i| i.state == "awaiting_confirmation")
    })
    .await;
    let ready = item(&fx, ticket);
    assert_eq!(ready.validation["workspace_checks"]["reused_from"], "candidate_checks", "{}", ready.validation);
    assert_eq!(ready.validation["candidate_checks"][0]["ok"], true);
    let runs = std::fs::read_to_string(&marker).unwrap();
    // The completion check ran in the task worktree; integration ran once.
    let integration_runs = runs.lines().filter(|line| line.contains("integrations")).count();
    assert_eq!(integration_runs, 1, "{runs}");
    fx.engine.close().await;
}

/// Ignored generated resources named by the project reach the isolated
/// writer and the merge candidate; edits to them are never merged back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn isolated_writer_and_candidate_receive_configured_ignored_resources() {
    let fx = Fx::new(
        script(|c| async move {
            if c.depth() == 0 {
                if c.turn() == 1 {
                    return Ok(send(json!({"id":"writer","write":true,"body":"write"})));
                }
                return Ok("summary".into());
            }
            let cwd = &c.req.cwd;
            assert_eq!(std::fs::read_to_string(cwd.join("gen/model.json")).unwrap(), "native");
            assert!(!cwd.join("tmp/scratch.bin").exists(), "unconfigured ignored paths stay out");
            std::fs::write(cwd.join("gen/model.json"), "edited in worktree").unwrap();
            std::fs::write(cwd.join("result.txt"), "result\n").unwrap();
            Ok("done".into())
        }),
        t1_catalog(),
    );
    init(&fx.project);
    std::fs::write(fx.project.join(".gitignore"), "gen/\ntmp/\n").unwrap();
    git(&fx.project, &["add", ".gitignore"]);
    git(&fx.project, &["commit", "-m", "ignore generated"]);
    std::fs::create_dir_all(fx.project.join("gen")).unwrap();
    std::fs::create_dir_all(fx.project.join("tmp")).unwrap();
    std::fs::write(fx.project.join("gen/model.json"), "native").unwrap();
    std::fs::write(fx.project.join("tmp/scratch.bin"), "scratch").unwrap();
    let project = fx.project.to_string_lossy().to_string();
    assert!(fx.engine.set_workspace_hydration(&project, vec!["../outside".into()]).is_err());
    fx.engine.set_workspace_hydration(&project, vec!["./gen/".into()]).unwrap();
    assert_eq!(fx.engine.workspace_hydration(&project).unwrap(), ["gen"]);
    let check: Vec<String> = if cfg!(windows) {
        ["powershell.exe","-NoProfile","-NonInteractive","-Command",
            "if ($PWD.Path -like '*integrations*' -and !(Test-Path gen/model.json)) { exit 1 }; exit 0"]
            .into_iter().map(str::to_string).collect()
    } else {
        ["sh","-c","case \"$PWD\" in */integrations/*) test -f gen/model.json;; *) exit 0;; esac"]
            .into_iter().map(str::to_string).collect()
    };
    fx.engine.set_project_checks(&project, vec![check]).unwrap();
    let ticket = fx.create(fx.params().profile("full")).await;
    fx.engine.set_isolated_writers(ticket, true).unwrap();
    fx.engine.set_integration_queue(ticket, true).unwrap();
    fx.engine.start().unwrap();
    wait(&fx, || {
        fx.engine.integration_queue(ticket).unwrap().iter().any(|i| i.state == "awaiting_confirmation")
    })
    .await;
    let ready = item(&fx, ticket);
    assert_eq!(ready.validation["candidate_hydration"]["files"], 1, "{}", ready.validation);
    assert_eq!(std::fs::read_to_string(fx.project.join("gen/model.json")).unwrap(), "native");
    fx.engine.close().await;
}

/// Confirming while the primary still works in the source records the
/// confirmation; the scheduler publishes once the source is free.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confirmation_while_the_source_is_busy_publishes_between_turns() {
    let entered = Gate::new();
    let release = Gate::new();
    let fx = Fx::new(
        script({
            let (entered, release) = (entered.clone(), release.clone());
            move |c| {
                let (entered, release) = (entered.clone(), release.clone());
                async move {
                    if c.depth() == 0 {
                        if c.turn() == 1 {
                            return Ok(send(json!({"id":"writer","write":true,"body":"write"})));
                        }
                        if c.turn() >= 3 {
                            // The operator's follow-up keeps the primary busy in the source.
                            entered.open();
                            release.wait().await;
                        }
                        return Ok("summary".into());
                    }
                    std::fs::write(c.req.cwd.join("result.txt"), "result
").unwrap();
                    Ok("done".into())
                }
            }
        }),
        t1_catalog(),
    );
    let ticket = setup(&fx, vec![passing()]).await;
    wait(&fx, || {
        fx.engine.integration_queue(ticket).unwrap().iter().any(|i| i.state == "awaiting_confirmation")
            && root_state(&fx.engine, ticket) == "done"
    })
    .await;
    fx.engine.follow(ticket, "继续核对", true).unwrap();
    wait(&fx, || root_state(&fx.engine, ticket) == "running").await;
    let busy = tokio::time::timeout(std::time::Duration::from_secs(60), entered.wait()).await;
    if busy.is_err() {
        release.open();
        let jobs = fx.all("SELECT id,state,turn,ready_after FROM jobs");
        let events = fx.all("SELECT job,kind,substr(payload,1,160) AS p FROM events ORDER BY seq DESC LIMIT 12");
        panic!("the primary did not start its follow-up turn {jobs:?} {events:?} {:?}", item(&fx, ticket).state);
    }
    let ready = item(&fx, ticket);
    let token = ready.validation["confirmation_token"].as_str().unwrap().to_string();
    let confirmed = fx.engine.confirm_integration(ready.job, &token).await;
    let state = item(&fx, ticket).state;
    let merged_early = fx.project.join("result.txt").exists();
    release.open();
    confirmed.unwrap();
    assert_eq!(state, "confirmed");
    assert!(!merged_early, "nothing merges while the primary runs");
    wait(&fx, || item(&fx, ticket).state == "merged").await;
    assert!(fx.project.join("result.txt").exists());
    fx.engine.close().await;
}

/// A source commit after validation (the primary saving evidence) makes the
/// confirmation fail without merging; the candidate validates again by itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn source_commit_after_confirmation_revalidates_and_merges_the_same_task() {
    let fx = Fx::new(writer(), t1_catalog());
    let ticket = setup(&fx, vec![passing()]).await;
    wait(&fx, || {
        fx.engine.integration_queue(ticket).unwrap().iter().any(|i| i.state == "awaiting_confirmation")
    })
    .await;
    let first = item(&fx, ticket);
    std::fs::write(fx.project.join("evidence.txt"), "primary evidence\n").unwrap();
    git(&fx.project, &["add", "evidence.txt"]);
    git(&fx.project, &["commit", "-m", "evidence"]);
    let token = first.validation["confirmation_token"].as_str().unwrap().to_string();
    assert!(fx.engine.confirm_integration(first.job, &token).await.is_err());
    assert!(!fx.project.join("result.txt").exists());
    // The task itself did not change, so the confirmation carries over to the
    // revalidated candidate and it merges without a second confirmation.
    wait(&fx, || item(&fx, ticket).state == "merged").await;
    let merged = item(&fx, ticket);
    assert!(merged.validation["baseline"] != first.validation["baseline"]);
    assert!(fx.project.join("result.txt").exists() && fx.project.join("evidence.txt").exists());
    let top = git(&fx.project, &["log", "-1", "--format=%s"]);
    assert!(top.starts_with(&format!("Merge task #{} (ticket {})", merged.job, ticket)), "{top}");
    // Superseded and merged candidates and the merged task workspace are removed.
    let first_candidate = first.validation["candidate"].as_str().unwrap();
    assert!(!std::path::Path::new(first_candidate).exists(), "superseded candidate removed");
    // Cleanup runs right after the merge is recorded.
    let workspace = fx.engine.db().with(|q| sa_engine::workspaces::get(q, merged.job)).unwrap().unwrap();
    let candidate = merged.validation["candidate"].as_str().unwrap().to_string();
    wait(&fx, || !std::path::Path::new(&workspace.path).exists() && !std::path::Path::new(&candidate).exists()).await;
    assert_eq!(fx.int("SELECT count(*) FROM integration_confirmations"), 0);
    fx.engine.close().await;
}
