//! A real Git conflict must return to the original author, not publish a
//! best-effort merge or merely mark the model response as successful.
mod common;
use common::*;
use serde_json::json;
use std::{path::Path, process::Command};

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git").args([
        "-c", "user.name=Fixture", "-c", "user.email=fixture@localhost",
        "-c", "commit.gpgsign=false", "-c", "core.hooksPath=",
    ]).args(args).current_dir(path).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim().into()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enabled_queue_database_contention_waits_and_recovers_without_fault() {
    use sa_engine::engine::SchedulerState;
    use std::time::Duration;
    let fx = Fx::new(script(|_| async { Ok("done".into()) }), t1_catalog());
    git(&fx.project, &["init"]);
    std::fs::write(fx.project.join("base.txt"), "base\n").unwrap();
    git(&fx.project, &["add", "."]);
    git(&fx.project, &["commit", "-m", "base"]);
    fx.engine.set_project_checks(&fx.project.to_string_lossy(), vec![vec!["git".into(), "diff".into(), "--check".into()]]).unwrap();
    let ticket = fx.create(fx.params().profile("full")).await;
    fx.engine.set_isolated_writers(ticket, true).unwrap();
    fx.engine.set_integration_queue(ticket, true).unwrap();
    fx.engine.db().with(|q| { q.conn().busy_timeout(Duration::from_millis(2))?; Ok(()) }).unwrap();
    fx.engine.start().unwrap();
    fx.finish(ticket, "done").await;
    let writer = rusqlite::Connection::open(fx.data.join("hub.sqlite3")).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    fx.until(|| fx.engine.scheduler_health().state == SchedulerState::DatabaseBusy).await;
    assert!(fx.engine.fault().is_none());
    writer.execute_batch("ROLLBACK").unwrap();
    fx.until(|| fx.engine.scheduler_health().state == SchedulerState::Running).await;
    assert_eq!(fx.engine.scheduler_health().state, SchedulerState::Running);
    assert!(fx.engine.fault().is_none());
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conflicting_candidate_returns_output_to_original_session_then_requires_confirmation() {
    let entered = Gate::new();
    let release = Gate::new();
    let signal = entered.clone();
    let wait = release.clone();
    let fx = Fx::new(script(move |c| {
        let signal = signal.clone();
        let wait = wait.clone();
        async move {
            if c.depth() == 0 {
                return if c.turn() == 1 {
                    Ok(send(json!({"id":"author","body":"update base","write":true})))
                } else { Ok("isolated delivery, not merged".into()) };
            }
            if c.turn() == 1 {
                std::fs::write(c.req.cwd.join("base.txt"), "writer change\n").unwrap();
                signal.open();
                wait.wait().await;
            } else {
                assert_eq!(c.turn(), 2, "conflict should need one repair");
                assert_eq!(c.req.session_id, Some(format!("session-{}", c.job())));
                let notes = c.full_packet()["notes"].to_string();
                assert!(notes.contains("base.txt") && notes.contains("conflicts"), "{notes}");
                let project = c.packet["project"].as_str().unwrap();
                let source_head = git(Path::new(project), &["rev-parse", "HEAD"]);
                // The fixture deliberately chooses the author's content when
                // resolving this specific conflict in its own task branch.
                git(&c.req.cwd, &["merge", "--no-ff", "--no-edit", "-X", "ours", &source_head]);
            }
            Ok("done".into())
        }
    }), t1_catalog());
    git(&fx.project, &["init"]);
    git(&fx.project, &["config", "core.autocrlf", "false"]);
    std::fs::write(fx.project.join("base.txt"), "base\n").unwrap();
    git(&fx.project, &["add", "."]);
    git(&fx.project, &["commit", "-m", "base"]);
    fx.engine.set_project_checks(&fx.project.to_string_lossy(), vec![vec!["git".into(), "diff".into(), "--check".into()]]).unwrap();
    let ticket = fx.create(fx.params().profile("full")).await;
    fx.engine.set_isolated_writers(ticket, true).unwrap();
    fx.engine.set_integration_queue(ticket, true).unwrap();
    fx.engine.start().unwrap();
    fx.until(|| entered.is_open()).await;
    std::fs::write(fx.project.join("base.txt"), "source change\n").unwrap();
    git(&fx.project, &["add", "."]);
    git(&fx.project, &["commit", "-m", "concurrent source change"]);
    let before = git(&fx.project, &["rev-parse", "HEAD"]);
    release.open();
    fx.until(|| fx.engine.integration_queue(ticket).unwrap().iter().any(|item| item.state == "awaiting_confirmation")).await;
    fx.finish(ticket, "done").await;
    let item = fx.engine.integration_queue(ticket).unwrap().remove(0);
    assert_eq!(item.attempts, 1);
    assert_eq!(git(&fx.project, &["rev-parse", "HEAD"]), before);
    assert_eq!(std::fs::read_to_string(fx.project.join("base.txt")).unwrap(), "source change\n");
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='integration.bounced'"), 1);
    fx.engine.confirm_integration(item.job, item.validation["confirmation_token"].as_str().unwrap()).await.unwrap();
    assert_eq!(std::fs::read_to_string(fx.project.join("base.txt")).unwrap(), "writer change\n");
    assert_eq!(fx.engine.integration_queue(ticket).unwrap()[0].state, "merged");
    fx.engine.close().await;
}
