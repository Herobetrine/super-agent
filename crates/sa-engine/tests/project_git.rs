//! Platform Git preparation through normal creation and legacy execution.
//! Every project and journal in these tests is disposable, with fake adapters.
mod common;

use common::*;
use serde_json::json;
use std::{path::Path, process::Command, time::Duration};

fn git(path: &Path, arguments: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@localhost",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=",
        ])
        .args(arguments)
        .current_dir(path)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

async fn wait(fixture: &Fx, condition: impl Fn() -> bool) {
    common::until(
        &fixture.engine,
        &fixture.shared,
        Duration::from_secs(60),
        condition,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readonly_creation_bootstraps_existing_files_and_preserves_task_permissions() {
    let fixture = Fx::new(
        script(|context| async move {
            assert_eq!(context.req.profile, "readonly");
            assert!(!context.req.writable);
            assert_eq!(
                std::fs::read(context.req.cwd.join("initial.bin")).unwrap(),
                b"\0\xff\r\n"
            );
            assert!(context.req.cwd.join(".git").is_dir());
            Ok("read initial files".into())
        }),
        t1_catalog(),
    );
    std::fs::write(fixture.project.join("initial.bin"), b"\0\xff\r\n").unwrap();
    std::fs::write(fixture.project.join(".gitignore"), "ignored.tmp\n").unwrap();
    std::fs::write(fixture.project.join("ignored.tmp"), "keep ignored bytes").unwrap();

    let ticket = fixture.create(fixture.params().profile("readonly")).await;
    let events = payloads(
        &fixture,
        "SELECT payload FROM events WHERE kind='project.git'",
    );
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]["root"],
        json!(std::fs::canonicalize(&fixture.project).unwrap())
    );
    assert!(
        events[0]["initialized"] == true
            || events[0]["initial_commit"]
                .as_str()
                .is_some_and(|head| !head.is_empty())
    );
    assert_eq!(
        git(&fixture.project, &["show", "HEAD:initial.bin"]),
        b"\0\xff\r\n"
    );
    assert!(git(&fixture.project, &["ls-files", "ignored.tmp"]).is_empty());
    assert!(
        git(
            &fixture.project,
            &["status", "--porcelain=v1", "--untracked-files=all"]
        )
        .is_empty()
    );
    fixture.engine.start().unwrap();
    wait(&fixture, || root_state(&fixture.engine, ticket) == "done").await;
    assert_eq!(
        std::fs::read_to_string(fixture.project.join("ignored.tmp")).unwrap(),
        "keep ignored bytes"
    );
    assert_eq!(fixture.shared.count(|_| true), 1);
    fixture
        .engine
        .follow(ticket, "Read the same project again", false)
        .unwrap();
    wait(&fixture, || root_state(&fixture.engine, ticket) == "done").await;
    assert_eq!(
        fixture.int("SELECT count(*) FROM events WHERE kind='project.git'"),
        1
    );
    assert_eq!(fixture.shared.count(|_| true), 2);
    fixture.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn synthetic_legacy_queued_ticket_prepares_its_current_original_project() {
    let fixture = Fx::new(
        script(|context| async move {
            assert_eq!(
                std::fs::read_to_string(context.req.cwd.join("legacy.txt")).unwrap(),
                "legacy content\n"
            );
            assert!(context.req.cwd.join(".git").is_dir());
            assert_eq!(context.req.profile, "readonly");
            Ok("legacy read complete".into())
        }),
        t1_catalog(),
    );
    let ticket = fixture.create(fixture.params().profile("readonly")).await;
    let legacy_project = fixture.dir.path().join("legacy-project");
    std::fs::create_dir(&legacy_project).unwrap();
    std::fs::write(legacy_project.join("legacy.txt"), "legacy content\n").unwrap();
    let legacy_project = sa_engine::services::realpath(&legacy_project).unwrap();
    fixture
        .engine
        .db()
        .tx(|query| {
            query.run(
                "UPDATE tickets SET cwd=? WHERE id=?",
                &[&legacy_project.to_string_lossy().as_ref(), &ticket],
            )?;
            Ok(())
        })
        .unwrap();
    assert!(!legacy_project.join(".git").exists());
    fixture.engine.start().unwrap();
    wait(&fixture, || root_state(&fixture.engine, ticket) == "done").await;
    assert_eq!(
        git(&legacy_project, &["show", "HEAD:legacy.txt"]),
        b"legacy content\n"
    );
    assert!(git(&legacy_project, &["status", "--porcelain=v1"]).is_empty());
    assert_eq!(fixture.shared.count(|_| true), 1);
    let events = payloads(
        &fixture,
        "SELECT payload FROM events WHERE kind='project.git'",
    );
    assert_eq!(
        events.len(),
        2,
        "one receipt for creation, one for the synthetic legacy project"
    );
    let legacy_events: Vec<_> = events
        .iter()
        .filter(|event| event["root"] == json!(std::fs::canonicalize(&legacy_project).unwrap()))
        .collect();
    assert_eq!(legacy_events.len(), 1);
    assert!(
        legacy_events[0]["initialized"] == true
            || legacy_events[0]["initial_commit"]
                .as_str()
                .is_some_and(|head| !head.is_empty())
    );
    fixture
        .engine
        .follow(ticket, "Read the legacy project again", false)
        .unwrap();
    wait(&fixture, || root_state(&fixture.engine, ticket) == "done").await;
    assert_eq!(
        fixture.int("SELECT count(*) FROM events WHERE kind='project.git'"),
        2
    );
    assert_eq!(fixture.shared.count(|_| true), 2);
    fixture.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn isolated_writer_receives_initial_snapshot_without_source_file_changes() {
    let fixture = Fx::new(
        script(|context| async move {
            if context.depth() == 0 {
                if context.turn() == 1 {
                    return Ok(send(
                        json!({"id":"writer", "write":true, "body":"write isolated result"}),
                    ));
                }
            } else {
                assert_ne!(
                    context.packet["project"],
                    context.packet["execution_directory"]
                );
                assert_eq!(
                    std::fs::read_to_string(context.req.cwd.join("initial.txt")).unwrap(),
                    "initial snapshot\n"
                );
                std::fs::write(context.req.cwd.join("result.txt"), "isolated result\n").unwrap();
            }
            Ok("done".into())
        }),
        t1_catalog(),
    );
    std::fs::write(fixture.project.join("initial.txt"), "initial snapshot\n").unwrap();
    let ticket = fixture.create(fixture.params().profile("full")).await;
    // This fixture compares exact checkout bytes independently of machine-wide
    // autocrlf settings. Production keeps the user's normal Git policy.
    git(&fixture.project, &["config", "core.autocrlf", "false"]);
    let initial_head = git(&fixture.project, &["rev-parse", "HEAD"]);
    fixture.engine.set_isolated_writers(ticket, true).unwrap();
    fixture.engine.start().unwrap();
    wait(&fixture, || root_state(&fixture.engine, ticket) == "done").await;
    let workspace = fixture.engine.workspaces(ticket).unwrap().remove(0);
    assert_eq!(
        std::fs::read_to_string(Path::new(&workspace.path).join("result.txt")).unwrap(),
        "isolated result\n"
    );
    assert!(!fixture.project.join("result.txt").exists());
    assert_eq!(git(&fixture.project, &["rev-parse", "HEAD"]), initial_head);
    assert!(git(&fixture.project, &["status", "--porcelain=v1"]).is_empty());
    fixture.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broken_repository_prevents_creation_before_any_adapter_call() {
    let fixture = Fx::new(
        script(|_| async { panic!("adapter must not run after repository preparation fails") }),
        t1_catalog(),
    );
    let broken_git = b"gitdir: nonexistent-git-metadata\n";
    std::fs::write(fixture.project.join(".git"), broken_git).unwrap();
    std::fs::write(fixture.project.join("initial.txt"), "preserve initial file").unwrap();
    assert!(
        fixture
            .engine
            .create(fixture.params().profile("readonly"))
            .await
            .is_err()
    );
    assert_eq!(fixture.shared.count(|_| true), 0);
    assert_eq!(fixture.int("SELECT count(*) FROM tickets"), 0);
    assert_eq!(
        std::fs::read(fixture.project.join(".git")).unwrap(),
        broken_git
    );
    assert_eq!(
        std::fs::read_to_string(fixture.project.join("initial.txt")).unwrap(),
        "preserve initial file"
    );
    fixture.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_selections_or_checkpoint_reject_before_repository_side_effects() {
    for invalid_checkpoint in [false, true] {
        let fixture = Fx::new(
            script(|_| async { panic!("invalid ticket must never invoke an adapter") }),
            t1_catalog(),
        );
        std::fs::write(fixture.project.join("initial.txt"), "preserve initial file").unwrap();
        let mut parameters = fixture.params().profile("readonly");
        if invalid_checkpoint {
            parameters.checkpoint = Some((999_999, 999_999));
        } else {
            parameters.selections = Some(json!("invalid route configuration"));
        }
        assert!(fixture.engine.create(parameters).await.is_err());
        assert!(!fixture.project.join(".git").exists());
        assert_eq!(fixture.int("SELECT count(*) FROM tickets"), 0);
        assert_eq!(fixture.shared.count(|_| true), 0);
        assert_eq!(
            std::fs::read_to_string(fixture.project.join("initial.txt")).unwrap(),
            "preserve initial file"
        );
        fixture.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn existing_dirty_repository_keeps_head_index_and_working_files() {
    let fixture = Fx::new(
        script(|_| async { Ok("read dirty source".into()) }),
        t1_catalog(),
    );
    git(&fixture.project, &["init"]);
    git(&fixture.project, &["config", "core.autocrlf", "false"]);
    std::fs::write(fixture.project.join("tracked.txt"), "baseline\n").unwrap();
    git(&fixture.project, &["add", "."]);
    git(&fixture.project, &["commit", "-m", "existing user commit"]);
    std::fs::write(fixture.project.join("tracked.txt"), "staged content\n").unwrap();
    git(&fixture.project, &["add", "tracked.txt"]);
    std::fs::write(fixture.project.join("tracked.txt"), "unstaged content\n").unwrap();
    std::fs::write(fixture.project.join("untracked.bin"), b"\0\xff").unwrap();
    let head = git(&fixture.project, &["rev-parse", "HEAD"]);
    let index = git(&fixture.project, &["diff", "--cached", "--binary"]);
    let unstaged = git(&fixture.project, &["diff", "--binary"]);
    let status = git(
        &fixture.project,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    );

    let ticket = fixture.create(fixture.params().profile("readonly")).await;
    fixture.engine.start().unwrap();
    wait(&fixture, || root_state(&fixture.engine, ticket) == "done").await;
    assert_eq!(git(&fixture.project, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git(&fixture.project, &["diff", "--cached", "--binary"]),
        index
    );
    assert_eq!(git(&fixture.project, &["diff", "--binary"]), unstaged);
    assert_eq!(
        git(
            &fixture.project,
            &["status", "--porcelain=v1", "--untracked-files=all"]
        ),
        status
    );
    assert_eq!(
        std::fs::read(fixture.project.join("untracked.bin")).unwrap(),
        b"\0\xff"
    );
    assert_eq!(
        fixture.int("SELECT count(*) FROM events WHERE kind='project.git'"),
        0
    );
    fixture.engine.close().await;
}
