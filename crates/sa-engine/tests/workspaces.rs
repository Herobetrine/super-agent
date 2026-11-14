mod common;
use common::*;
use sa_engine::workspaces;
use serde_json::json;
use std::{
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn isolated_writers_run_together_and_only_validated_candidates_integrate() {
    let entered = Arc::new(AtomicUsize::new(0));
    let count = entered.clone();
    let release = Gate::new();
    let wait = release.clone();
    let mut fx = Fx::new(
        script(move |c| {
            let count = count.clone();
            let wait = wait.clone();
            async move {
                if c.packet["depth"] == 0 {
                    if c.turn() == 1 {
                        return Ok(format!(
                            "SA_SEND {}\nSA_SEND {}",
                            json!({"id":"a","to":"codex","write":true,"body":"a"}),
                            json!({"id":"b","to":"codex","write":true,"body":"b"})
                        ));
                    }
                } else {
                    assert_ne!(c.packet["project"], c.packet["execution_directory"]);
                    assert_eq!(c.packet["workspace"]["state"], "ready");
                    std::fs::write(
                        c.req
                            .cwd
                            .join(format!("{}.txt", c.packet["assignment"].as_str().unwrap())),
                        "done\n",
                    )
                    .unwrap();
                    count.fetch_add(1, Ordering::SeqCst);
                    wait.wait().await;
                }
                Ok("done".into())
            }
        }),
        t1_catalog(),
    );
    init(&fx.project);
    let ticket = fx.create(fx.params().profile("full")).await;
    fx.engine.set_isolated_writers(ticket, true).unwrap();
    fx.engine.start().unwrap();
    fx.until(|| entered.load(Ordering::SeqCst) == 2).await;
    assert!(!fx.project.join("a.txt").exists());
    assert!(!fx.project.join("b.txt").exists());
    release.open();
    fx.finish(ticket, "done").await;
    // The journal and worktrees survive a scheduler restart.
    fx.restart(quiet()).await;
    let list = fx.engine.workspaces(ticket).unwrap();
    assert_eq!(list.len(), 2);
    for w in list {
        assert!(fx.engine.integrate_workspace(w.job).await.is_err());
        fx.engine
            .validate_workspace(w.job, vec!["git".into(), "diff".into(), "--check".into()])
            .await
            .unwrap();
        assert!(
            fx.engine
                .validate_workspace(w.job, vec!["nonexistent-validation-fixture.exe".into()])
                .await
                .is_err()
        );
        assert!(fx.engine.integrate_workspace(w.job).await.is_err());
        fx.engine
            .validate_workspace(w.job, vec!["git".into(), "diff".into(), "--check".into()])
            .await
            .unwrap();
        fx.engine.integrate_workspace(w.job).await.unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(fx.project.join("a.txt")).unwrap(),
        "done\n"
    );
    assert!(fx.project.join("b.txt").is_file());
    assert!(
        fx.engine
            .workspaces(ticket)
            .unwrap()
            .iter()
            .all(|w| w.state == "merged")
    );
    assert_eq!(git(&fx.project, &["status", "--porcelain"]), "");
}

#[test]
fn conflict_and_failed_validation_never_change_source_or_erase_user_edits() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("repo");
    let data = dir.path().join("data");
    std::fs::create_dir(&source).unwrap();
    init(&source);
    let make = |id| workspaces::Workspace {
        job: id,
        ticket: 1,
        source: source.to_string_lossy().into(),
        path: data
            .join("workspaces")
            .join(format!("job-{id}"))
            .to_string_lossy()
            .into(),
        branch: format!("codex/test-{id}"),
        base: String::new(),
        head: String::new(),
        state: "planned".into(),
        validation: json!({}),
        detail: String::new(),
    };
    let w = workspaces::provision(&make(1), &data).unwrap();
    std::fs::write(Path::new(&w.path).join("base.txt"), "child\n").unwrap();
    let fail = workspaces::validate(&w, &["git".into(), "bad-fixture-command".into()]).unwrap();
    assert_eq!(fail.state, "failed");
    assert!(workspaces::integrate(&fail, &data).is_err());
    let pass =
        workspaces::validate(&fail, &["git".into(), "diff".into(), "--check".into()]).unwrap();
    let original_branch = git(&source, &["symbolic-ref", "--short", "HEAD"]);
    git(&source, &["switch", "-c", "different-branch"]);
    assert!(workspaces::integrate(&pass, &data).is_err());
    git(&source, &["switch", &original_branch]);
    std::fs::write(source.join("base.txt"), "user unsaved\n").unwrap();
    assert!(workspaces::integrate(&pass, &data).is_err());
    assert_eq!(
        std::fs::read_to_string(source.join("base.txt")).unwrap(),
        "user unsaved\n"
    );
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "user"]);
    let before = git(&source, &["rev-parse", "HEAD"]);
    let conflict = workspaces::integrate(&pass, &data).unwrap();
    assert_eq!(conflict.state, "conflict");
    assert_eq!(before, git(&source, &["rev-parse", "HEAD"]));
    assert_eq!(
        std::fs::read_to_string(source.join("base.txt")).unwrap(),
        "user unsaved\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn distinct_projects_write_concurrently_but_nested_directories_are_serialized() {
    let count = Arc::new(AtomicUsize::new(0));
    let shared = count.clone();
    let release = Gate::new();
    let wait = release.clone();
    let fx = Fx::new(
        script(move |_| {
            let shared = shared.clone();
            let wait = wait.clone();
            async move {
                shared.fetch_add(1, Ordering::SeqCst);
                wait.wait().await;
                Ok("done".into())
            }
        }),
        t1_catalog(),
    );
    let other = fx.dir.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let nested = fx.project.join("nested");
    std::fs::create_dir(&nested).unwrap();
    let a = fx.begin(fx.params().profile("full")).await;
    let b = fx
        .create(
            sa_engine::CreateParams::new("b", other)
                .lead("codex")
                .profile("full"),
        )
        .await;
    let c = fx
        .create(
            sa_engine::CreateParams::new("c", nested)
                .lead("codex")
                .profile("full"),
        )
        .await;
    fx.until(|| count.load(Ordering::SeqCst) == 2).await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
    release.open();
    fx.finish(a, "done").await;
    fx.finish(b, "done").await;
    fx.finish(c, "done").await;
    assert_eq!(count.load(Ordering::SeqCst), 3);
}
