use super::*;
use crate::EngineOptions;
use std::cell::Cell;

struct Fixture {
    engine: Engine,
    requirement: i64,
    snapshot: i64,
    _directory: tempfile::TempDir,
}

impl Fixture {
    fn new(argv: &[&str]) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("input"), b"frozen").unwrap();
        let engine = Engine::open(
            directory.path().join("ledger"),
            vec![],
            EngineOptions::default(),
        )
        .unwrap();
        engine.db().with(|q| q.run("INSERT INTO tickets(id,goal,cwd,writes,lead,created) VALUES(1,'fixture',?,0,'codex',1)", &[&project.to_string_lossy().as_ref()])).unwrap();
        let requirement = engine
            .save_requirement(1, 0, None, "example".into(), None, None, None)
            .unwrap()
            .requirements[0]
            .id;
        let argv: Vec<_> = argv.iter().map(|arg| (*arg).to_string()).collect();
        engine
            .set_project_checks(&project.to_string_lossy(), vec![argv.clone()])
            .unwrap();
        engine
            .adopt_delivery_check(1, 1, requirement, argv)
            .unwrap();
        let snapshot = engine
            .capture_delivery(1, 2, vec!["input".into()])
            .unwrap()
            .id;
        engine.set_delivery_gate(1, 0, true).unwrap();
        Self {
            engine,
            requirement,
            snapshot,
            _directory: directory,
        }
    }

    fn run(&self, cancel: &dyn Fn() -> bool) -> Result<BehaviorReceipt> {
        self.engine
            .run_delivery_check_with_cancel(1, 2, self.snapshot, self.requirement, cancel)
    }
}

#[cfg(windows)]
const PASS: &[&str] = &["cmd.exe", "/D", "/C", "echo observed-marker"];
#[cfg(not(windows))]
const PASS: &[&str] = &["/bin/sh", "-c", "echo observed-marker"];

#[test]
fn prior_ready_cannot_survive_an_interleaved_newer_cancelled_attempt() {
    let f = Fixture::new(PASS);
    let pass = f.run(&|| false).unwrap();
    assert_eq!(pass.status, "pass", "{}", pass.detail);
    assert_eq!(
        f.engine
            .prepare_delivery(1, 2, f.snapshot, 1)
            .unwrap()
            .status,
        "ready"
    );
    let started = Cell::new(false);
    let older_attempt = Cell::new(0);
    let old = f.run(&|| {
        if !started.replace(true) {
            // The older attempt has reserved an id but has not launched yet.
            let summary = f.engine.delivery_summary(1).unwrap();
            assert_ne!(summary.gate.status, "ready");
            let observed = summary.latest_snapshot.unwrap().requirements[0]
                .behavior
                .clone()
                .unwrap()
                .receipt
                .unwrap();
            assert_eq!(observed.status, "running");
            assert!(observed.id > pass.id);
            older_attempt.set(observed.id);
            assert_eq!(f.run(&|| true).unwrap().status, "invalid");
        }
        false
    });
    assert!(old.unwrap_err().to_string().contains("更新"));
    let historic = f
        .engine
        .db()
        .with(|q| delivery::behavior_receipt(q.conn(), older_attempt.get()))
        .unwrap();
    assert_eq!(historic.status, "invalid");
    assert_eq!(historic.exit_code, Some(0));
    assert!(
        historic.detail.contains("更新")
            && historic.detail.contains("pass")
            && historic.detail.contains("observed-marker"),
        "{}",
        historic.detail
    );
    let summary = f.engine.delivery_summary(1).unwrap();
    assert_ne!(summary.gate.status, "ready");
    let observed = summary.latest_snapshot.unwrap().requirements[0]
        .behavior
        .clone()
        .unwrap()
        .receipt
        .unwrap();
    assert_eq!(observed.status, "invalid");
    assert!(observed.detail.contains("取消"));
    assert_eq!(
        f.engine
            .prepare_delivery(1, 2, f.snapshot, 1)
            .unwrap()
            .status,
        "blocked"
    );
}

#[test]
fn revision_and_behavior_result_commit_windows_fail_closed() {
    let f = Fixture::new(PASS);
    assert_eq!(f.run(&|| false).unwrap().status, "pass");
    let prepare = f.engine.prepare_delivery_inner(1, 2, f.snapshot, 1, || {
        f.run(&|| true)?;
        Ok(())
    });
    assert!(prepare.unwrap_err().to_string().contains("回执"));
    let revised = Cell::new(false);
    let result = f.run(&|| {
        if !revised.replace(true) {
            f.engine
                .save_requirement(
                    1,
                    2,
                    Some(f.requirement),
                    "new meaning".into(),
                    None,
                    None,
                    None,
                )
                .unwrap();
        }
        false
    });
    assert!(result.is_err());
    assert_ne!(f.engine.delivery_summary(1).unwrap().gate.status, "ready");
}

#[test]
fn timeout_preserves_a_nonpassing_terminal_receipt() {
    #[cfg(windows)]
    let slow: &[&str] = &[
        "powershell.exe",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "Start-Sleep -Seconds 10",
    ];
    #[cfg(not(windows))]
    let slow: &[&str] = &["/bin/sh", "-c", "sleep 10"];
    let f = Fixture::new(slow);
    let receipt = f
        .engine
        .run_check_inner(
            1,
            2,
            f.snapshot,
            f.requirement,
            &|| false,
            Duration::from_millis(30),
        )
        .unwrap();
    assert_eq!(receipt.status, "invalid");
    assert!(receipt.detail.contains("超时"), "{}", receipt.detail);
    assert_eq!(receipt.exit_code, None);
    assert_eq!(
        f.engine
            .prepare_delivery(1, 2, f.snapshot, 1)
            .unwrap()
            .status,
        "blocked"
    );
}

#[test]
fn git_pointer_inputs_are_refused_while_gitignore_is_usable() {
    let f = Fixture::new(&["git", "hash-object", ".gitignore"]);
    let project = f._directory.path().join("project");
    let upstream = f._directory.path().join("other-repo");
    fs::create_dir(&upstream).unwrap();
    workspaces::git(&upstream, &["init", "--quiet"]).unwrap();
    fs::write(project.join(".gitignore"), "*.log\n").unwrap();
    let pointer = format!("gitdir: {}\n", upstream.join(".git").display());
    fs::write(project.join(".git"), &pointer).unwrap();
    fs::create_dir(project.join("sub")).unwrap();
    fs::write(project.join("sub/.GiT"), &pointer).unwrap();
    fs::create_dir_all(project.join("nested/.gIt")).unwrap();
    fs::write(project.join("nested/.gIt/HEAD"), b"ref: refs/heads/main\n").unwrap();
    for forbidden in [".git", "sub/.GiT", "nested/.gIt/HEAD"] {
        let snapshot = f
            .engine
            .capture_delivery(1, 2, vec![".gitignore".into(), forbidden.into()])
            .unwrap();
        let result = f
            .engine
            .run_delivery_check(1, 2, snapshot.id, f.requirement)
            .unwrap();
        assert_eq!(result.status, "invalid");
        assert_eq!(result.exit_code, None);
        assert!(result.detail.contains(forbidden), "{}", result.detail);
    }
    let snapshot = f
        .engine
        .capture_delivery(1, 2, vec![".gitignore".into()])
        .unwrap();
    let result = f
        .engine
        .run_delivery_check(1, 2, snapshot.id, f.requirement)
        .unwrap();
    assert_eq!(result.status, "pass", "{}", result.detail);
}

#[test]
fn git_discovery_stops_at_the_private_run_directory() {
    let outer = tempfile::tempdir().unwrap();
    workspaces::git(outer.path(), &["init", "--quiet"]).unwrap();
    let run = tempfile::tempdir_in(outer.path()).unwrap();
    let inputs = run.path().join("inputs");
    fs::create_dir(&inputs).unwrap();
    assert_eq!(
        workspaces::git(&inputs, &["rev-parse", "--is-inside-work-tree"]).unwrap(),
        "true"
    );
    let project = tempfile::tempdir().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let argv = ["git", "rev-parse", "--is-inside-work-tree"].map(str::to_string);
    let mut recorded = json!({});
    let host = host::Execution::prepare(
        run.path(),
        project.path(),
        ledger.path(),
        &argv,
        &mut recorded,
        &|| false,
    )
    .unwrap();
    let result =
        workspaces::delivery_command(&inputs, &host.argv, &host.env, TIMEOUT, &|| false).unwrap();
    assert!(!result.ok);
    assert!(result.exit.is_some_and(|code| code != 0));
    assert!(
        String::from_utf8_lossy(&result.output).contains("not a git repository"),
        "{:?}",
        result.output
    );
}

#[test]
fn unavailable_or_unsupported_programs_never_advance_adoption() {
    let f = Fixture::new(PASS);
    let project = f._directory.path().join("project");
    let unsupported = tempfile::tempdir().unwrap();
    let script = unsupported.path().join("fixture.cmd");
    fs::write(&script, b"echo fixture").unwrap();
    let missing = unsupported.path().join("missing-delivery-program.exe");
    for program in [missing, script] {
        let argv = vec![program.to_str().unwrap().to_owned()];
        f.engine
            .set_project_checks(project.to_str().unwrap(), vec![argv.clone()])
            .unwrap();
        let error = f
            .engine
            .adopt_delivery_check(1, 2, f.requirement, argv)
            .unwrap_err();
        assert!(error.to_string().contains("宿主解释器"), "{error}");
        let summary = f.engine.delivery_summary(1).unwrap();
        assert_eq!(summary.contract_revision, 2);
        assert_eq!(
            summary.requirements[0].behavior.as_ref().unwrap().argv,
            PASS
        );
    }
}

#[cfg(windows)]
#[test]
fn windows_denies_temporary_input_replacement_and_allows_scratch_outputs() {
    let f = Fixture::new(&[
        "powershell.exe",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        r#"$ErrorActionPreference='Stop'; try { [IO.File]::WriteAllText((Join-Path $pwd 'input'),'changed'); exit 11 } catch [IO.IOException] {}; try { Move-Item input moved; exit 12 } catch [IO.IOException] {}; [IO.File]::WriteAllText((Join-Path $pwd 'output'),'generated'); if ([IO.File]::ReadAllText((Join-Path $pwd 'input')) -ne 'frozen') {exit 13}; exit 0"#,
    ]);
    let receipt = f.run(&|| false).unwrap();
    assert_eq!(receipt.status, "pass", "{}", receipt.detail);
}
