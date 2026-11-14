//! Independent regression checks for workspace data preservation and loop evidence.
use sa_engine::{
    stuck::{Detector, Signal},
    workspaces::{self, Workspace},
};
use serde_json::json;
use std::{path::Path, process::Command};

fn git(path: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Edge fixture",
            "-c",
            "user.email=edge@localhost",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=",
        ])
        .args(args)
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
fn init(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init"]);
    git(path, &["config", "core.autocrlf", "false"]);
    std::fs::write(path.join("base.txt"), "base\n").unwrap();
    std::fs::write(path.join(".gitignore"), "private.dat\n").unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-m", "base"]);
}
fn planned(source: &Path, data: &Path) -> Workspace {
    Workspace {
        job: 1,
        ticket: 1,
        source: source.to_string_lossy().into(),
        path: data.join("workspaces/job-1").to_string_lossy().into(),
        branch: "codex/edge-1".into(),
        base: String::new(),
        head: String::new(),
        state: "planned".into(),
        validation: json!({}),
        detail: String::new(),
    }
}
fn valid(w: &Workspace) -> Workspace {
    workspaces::validate(w, &["git".into(), "diff".into(), "--check".into()]).unwrap()
}

#[test]
fn integration_preserves_ignored_source_file_that_new_commit_would_track() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let data = dir.path().join("data");
    init(&source);
    let w = workspaces::provision(&planned(&source, &data), &data).unwrap();
    std::fs::write(source.join("private.dat"), "user data must survive").unwrap();
    std::fs::write(Path::new(&w.path).join("private.dat"), "child replacement").unwrap();
    git(Path::new(&w.path), &["add", "--force", "private.dat"]);
    let pass = valid(&w);
    let before = git(&source, &["rev-parse", "HEAD"]);
    assert!(workspaces::integrate(&pass, &data).is_err());
    assert_eq!(
        std::fs::read_to_string(source.join("private.dat")).unwrap(),
        "user data must survive"
    );
    assert_eq!(git(&source, &["rev-parse", "HEAD"]), before);
}

#[test]
fn validation_cannot_reuse_receipt_after_child_head_or_source_branch_changes() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let data = dir.path().join("data");
    init(&source);
    let w = workspaces::provision(&planned(&source, &data), &data).unwrap();
    std::fs::write(Path::new(&w.path).join("child.txt"), "version-one").unwrap();
    let pass = valid(&w);
    std::fs::write(Path::new(&w.path).join("child.txt"), "version-two").unwrap();
    git(Path::new(&w.path), &["add", "."]);
    git(
        Path::new(&w.path),
        &["commit", "-m", "changed after receipt"],
    );
    let before = git(&source, &["rev-parse", "HEAD"]);
    assert!(workspaces::integrate(&pass, &data).is_err());
    assert_eq!(git(&source, &["rev-parse", "HEAD"]), before);
    git(&source, &["switch", "-c", "new-source-branch"]);
    assert!(workspaces::validate(&w, &["git".into(), "diff".into(), "--check".into()]).is_err());
}

#[cfg(windows)]
#[test]
fn source_junction_cannot_redirect_to_same_branch_in_another_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let repository = dir.path().join("repository");
    let source = dir.path().join("source");
    let replacement = dir.path().join("replacement");
    let old = dir.path().join("source-before-redirection");
    let data = dir.path().join("data");
    init(&repository);
    git(
        &repository,
        &[
            "worktree",
            "add",
            "-b",
            "source-branch",
            source.to_str().unwrap(),
        ],
    );
    let w = workspaces::provision(&planned(&source, &data), &data).unwrap();
    git(
        &repository,
        &[
            "worktree",
            "add",
            "--force",
            replacement.to_str().unwrap(),
            "source-branch",
        ],
    );
    assert!(source.starts_with(dir.path()) && old.starts_with(dir.path()));
    std::fs::rename(&source, &old).unwrap();
    let output=Command::new("powershell.exe").args(["-NoProfile","-NonInteractive","-Command","$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path $env:SA_EDGE_LINK -Target $env:SA_EDGE_TARGET | Out-Null"])
  .env("SA_EDGE_LINK",&source).env("SA_EDGE_TARGET",&replacement).output().unwrap();
    assert!(
        output.status.success(),
        "junction fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rejected = workspaces::identity(&w).is_err();
    // Remove only the junction itself, never its target.
    std::fs::remove_dir(&source).unwrap();
    assert!(replacement.join("base.txt").exists());
    assert!(
        rejected,
        "same repository and branch do not prove source checkout identity"
    );
}

#[test]
fn repeated_successful_commands_are_not_proof_of_a_stuck_agent() {
    let mut detector = Detector::default();
    for i in 0..24 {
        let event = json!({"method":"item/completed","params":{"item":{"id":format!("call-{i}"),"type":"commandExecution","command":"append-one-record && echo done","cwd":"/temporary-fixture","aggregatedOutput":"done","exitCode":0}}});
        assert_ne!(
            detector.observe("codex", &event),
            Some(Signal::Stop),
            "successful identical output may accompany useful side effects"
        );
    }
}

#[test]
fn repeated_explicit_tool_errors_still_warn_and_stop() {
    let mut detector = Detector::default();
    let mut warned = false;
    let mut stopped = false;
    for i in 0..12 {
        let event = json!({"method":"item/completed","params":{"item":{"id":format!("call-{i}"),"type":"commandExecution","command":"compile","cwd":"/temporary-fixture","aggregatedOutput":"same compile error","exitCode":1}}});
        match detector.observe("codex", &event) {
            Some(Signal::Warn) => warned = true,
            Some(Signal::Stop) => stopped = true,
            None => {}
        }
    }
    assert!(warned && stopped);
}

#[cfg(windows)]
#[test]
fn validator_parent_exit_reaps_its_live_background_descendant() {
    for exit_code in [0, 7] {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let data = dir.path().join("data");
        init(&source);
        let w = workspaces::provision(&planned(&source, &data), &data).unwrap();
        let started = dir.path().join("child-started");
        let escaped = dir.path().join("child-escaped");
        let child = "const fs=require('node:fs');fs.writeFileSync(process.argv[1],process.pid.toString());setTimeout(()=>fs.writeFileSync(process.argv[2],'escaped'),800);";
        let parent = format!(
            "const fs=require('node:fs');const c=require('node:child_process').spawn(process.execPath,['-e',{},process.argv[1],process.argv[2]],{{stdio:'ignore',windowsHide:true}});c.unref();setInterval(()=>{{if(fs.existsSync(process.argv[1]))process.exit({exit_code});}},10);",
            serde_json::to_string(child).unwrap()
        );
        let result = workspaces::validate(
            &w,
            &[
                "node.exe".into(),
                "-e".into(),
                parent,
                started.to_string_lossy().into(),
                escaped.to_string_lossy().into(),
            ],
        )
        .unwrap();
        assert_eq!(
            result.state,
            if exit_code == 0 {
                "validated"
            } else {
                "failed"
            }
        );
        assert!(
            started.exists(),
            "the real background descendant must have started"
        );
        std::thread::sleep(std::time::Duration::from_millis(1000));
        assert!(
            !escaped.exists(),
            "background descendant survived validator parent exit"
        );
    }
}
