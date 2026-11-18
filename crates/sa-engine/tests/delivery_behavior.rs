//! Operator-owned behavior evidence on real, disposable process and Git inputs.
mod common;
use common::*;
use sa_store::delivery;
use std::{fs, path::Path, process::Command};

fn command() -> Vec<String> {
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

struct Scenario {
    fx: Fx,
    ticket: i64,
    requirement: i64,
    snapshot: i64,
}

impl Scenario {
    async fn new() -> Self {
        let fx = Fx::new(
            script(|_| async { panic!("unexpected model call") }),
            t1_catalog(),
        );
        fs::write(fx.project.join("expected.txt"), "42\n").unwrap();
        fs::write(fx.project.join("result.txt"), " 42 \n").unwrap();
        let ticket = fx.create(fx.params()).await;
        let summary = fx
            .engine
            .save_requirement(
                ticket,
                0,
                None,
                "Ignore presentation whitespace".into(),
                None,
                None,
                None,
            )
            .unwrap();
        let requirement = summary.requirements[0].id;
        let project = fx.project.to_str().unwrap();
        assert!(
            fx.engine
                .adopt_delivery_check(ticket, 1, requirement, command())
                .is_err()
        );
        fx.engine
            .set_project_checks(project, vec![command()])
            .unwrap();
        let adopted = fx
            .engine
            .adopt_delivery_check_with_files(
                ticket,
                1,
                requirement,
                command(),
                vec!["expected.txt".into()],
            )
            .unwrap();
        assert_eq!(adopted.contract_revision, 2);
        let snapshot = fx
            .engine
            .capture_delivery(ticket, 2, vec!["expected.txt".into(), "result.txt".into()])
            .unwrap()
            .id;
        fx.engine.set_delivery_gate(ticket, 0, true).unwrap();
        Self {
            fx,
            ticket,
            requirement,
            snapshot,
        }
    }

    fn recapture(&mut self) {
        self.snapshot = self
            .fx
            .engine
            .capture_delivery(
                self.ticket,
                2,
                vec!["expected.txt".into(), "result.txt".into()],
            )
            .unwrap()
            .id;
    }
    fn run(&self) -> delivery::BehaviorReceipt {
        self.fx
            .engine
            .run_delivery_check(self.ticket, 2, self.snapshot, self.requirement)
            .unwrap()
    }
    fn assess(&self) -> String {
        self.fx
            .engine
            .prepare_delivery(self.ticket, 2, self.snapshot, 1)
            .unwrap()
            .status
    }
}

#[tokio::test]
async fn adopted_assertion_survives_project_default_changes_but_cannot_certify_another_snapshot() {
    let mut case = Scenario::new().await;
    let jobs = case.fx.all("SELECT * FROM jobs");
    assert_eq!(case.assess(), "blocked");
    case.fx
        .engine
        .set_project_checks(case.fx.project.to_str().unwrap(), vec![])
        .unwrap();
    let pass = case.run();
    assert_eq!(pass.status, "pass", "{pass:?}");
    assert_eq!(pass.exit_code, Some(0));
    assert_eq!(case.assess(), "ready");
    // Even the same bytes and argv do not let a second invocation reuse the first decision.
    assert_ne!(case.run().id, pass.id);
    let summary = case.fx.engine.delivery_summary(case.ticket).unwrap();
    assert_ne!(summary.gate.status, "ready");
    assert_eq!(case.assess(), "ready");

    fs::write(case.fx.project.join("result.txt"), "unexpected\n").unwrap();
    let current = delivery::read_summary(&case.fx.data, case.ticket).unwrap();
    assert_eq!(current.requirements[0].status, "stale");
    assert_eq!(current.gate.status, "ready");
    case.recapture();
    assert_eq!(case.assess(), "blocked");
    let fail = case.run();
    assert_eq!(fail.status, "fail", "{fail:?}");
    assert_ne!(fail.exit_code, Some(0));
    assert_eq!(case.assess(), "blocked");
    let historical = delivery::read_snapshot(&case.fx.data, case.ticket, pass.snapshot_id).unwrap();
    assert_eq!(historical.requirements[0].status, "pass");
    assert_eq!(case.fx.shared.count(|_| true), 0);
    assert_eq!(case.fx.all("SELECT * FROM jobs"), jobs);
}

#[tokio::test]
async fn substituted_golden_file_cannot_weaken_the_adopted_assertion() {
    let mut case = Scenario::new().await;
    for name in ["result.txt", "expected.txt"] {
        fs::write(case.fx.project.join(name), "both wrong\n").unwrap();
    }
    case.recapture();
    let attempt =
        case.fx
            .engine
            .run_delivery_check(case.ticket, 2, case.snapshot, case.requirement);
    assert!(!attempt.is_ok_and(|run| run.status == "pass"));
    assert_eq!(case.assess(), "blocked");
    let omitted = case
        .fx
        .engine
        .capture_delivery(case.ticket, 2, vec!["result.txt".into()])
        .unwrap()
        .id;
    let attempt = case
        .fx
        .engine
        .run_delivery_check(case.ticket, 2, omitted, case.requirement);
    assert!(!attempt.is_ok_and(|run| run.status == "pass"));
    assert_eq!(
        case.fx
            .engine
            .prepare_delivery(case.ticket, 2, omitted, 1)
            .unwrap()
            .status,
        "blocked"
    );
}

#[tokio::test]
async fn successful_command_that_overwrites_an_input_is_never_passing_evidence() {
    let case = Scenario::new().await;
    let argv: Vec<String> = if cfg!(windows) {
        vec!["cmd.exe", "/D", "/C", "echo altered>result.txt"]
    } else {
        vec!["sh", "-c", "echo altered > result.txt"]
    }
    .into_iter()
    .map(str::to_owned)
    .collect();
    let engine = &case.fx.engine;
    engine
        .set_project_checks(case.fx.project.to_str().unwrap(), vec![argv.clone()])
        .unwrap();
    let adopted = engine
        .adopt_delivery_check(case.ticket, 2, case.requirement, argv)
        .unwrap();
    let revision = adopted.contract_revision;
    let frozen = engine
        .capture_delivery(case.ticket, revision, vec!["result.txt".into()])
        .unwrap();
    let result = engine
        .run_delivery_check(case.ticket, revision, frozen.id, case.requirement)
        .unwrap();
    // Windows can deny the write during execution; post-run hashing also
    // rejects an otherwise successful process that did change a captured file.
    assert!(
        matches!(result.status.as_str(), "invalid" | "fail"),
        "{result:?}"
    );
    assert_eq!(
        engine
            .prepare_delivery(case.ticket, revision, frozen.id, 1)
            .unwrap()
            .status,
        "blocked"
    );
    assert_eq!(
        fs::read(case.fx.project.join("result.txt")).unwrap(),
        b" 42 \n"
    );
}

#[tokio::test]
async fn byte_criterion_and_adopted_behavior_must_both_pass() {
    let case = Scenario::new().await;
    let engine = &case.fx.engine;
    let requirement = case.requirement;
    let revised = engine
        .save_requirement(
            case.ticket,
            2,
            Some(requirement),
            "Whitespace tolerant output, sealed reference".into(),
            Some("expected.txt".into()),
            Some(delivery::sha256(b"wrong SHA on purpose")),
            None,
        )
        .unwrap();
    let adopted = engine
        .adopt_delivery_check(
            case.ticket,
            revised.contract_revision,
            requirement,
            command(),
        )
        .unwrap();
    let revision = adopted.contract_revision;
    let frozen = engine
        .capture_delivery(
            case.ticket,
            revision,
            vec!["expected.txt".into(), "result.txt".into()],
        )
        .unwrap();
    let run = engine
        .run_delivery_check(case.ticket, revision, frozen.id, requirement)
        .unwrap();
    assert_eq!(run.status, "pass", "{run:?}");
    let gate = engine
        .prepare_delivery(case.ticket, revision, frozen.id, 1)
        .unwrap();
    assert_eq!(gate.status, "blocked");
    assert_eq!(gate.unmet[0].status, "fail");
    assert_eq!(
        engine.delivery_summary(case.ticket).unwrap().gate.status,
        "blocked"
    );
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(path)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@localhost",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn accepting_partial_delivery_preserves_unmet_requirements_and_is_specific_to_one_assessment()
{
    let case = Scenario::new().await;
    let engine = &case.fx.engine;
    let added = engine
        .save_requirement(
            case.ticket,
            2,
            None,
            "Manual layout review".into(),
            None,
            None,
            None,
        )
        .unwrap();
    let revision = added.contract_revision;
    let snapshot = engine
        .capture_delivery(
            case.ticket,
            revision,
            vec!["expected.txt".into(), "result.txt".into()],
        )
        .unwrap()
        .id;
    engine
        .run_delivery_check(case.ticket, revision, snapshot, case.requirement)
        .unwrap();
    let partial = engine
        .prepare_delivery(case.ticket, revision, snapshot, 1)
        .unwrap();
    assert_eq!(partial.status, "partial");
    assert_eq!(partial.unmet.len(), 1);
    assert!(
        engine
            .accept_delivery(case.ticket, partial.id, "ready")
            .is_err()
    );
    engine
        .accept_delivery(case.ticket, partial.id, "partial")
        .unwrap();
    engine
        .accept_delivery(case.ticket, partial.id, "partial")
        .unwrap();
    let gate = delivery::read_summary(&case.fx.data, case.ticket)
        .unwrap()
        .gate;
    let accepted = gate.acceptance.unwrap();
    assert_eq!(accepted.assessment_id, partial.id);
    assert_eq!(accepted.status, "partial");
    assert_eq!(gate.status, "partial");
    assert_eq!(
        gate.unmet[0].requirement_id,
        partial.unmet[0].requirement_id
    );
    let next = engine
        .prepare_delivery(case.ticket, revision, snapshot, 1)
        .unwrap();
    assert_ne!(next.id, partial.id);
    assert!(
        engine
            .accept_delivery(case.ticket, partial.id, "partial")
            .is_err()
    );
    let gate = delivery::read_summary(&case.fx.data, case.ticket)
        .unwrap()
        .gate;
    assert!(gate.acceptance.is_none());
    assert_eq!(gate.latest_acceptance.unwrap().id, accepted.id);
}

#[tokio::test]
async fn equal_partial_status_does_not_waive_a_changed_unmet_list() {
    let case = Scenario::new().await;
    let engine = &case.fx.engine;
    let file = "independent.txt";
    let digest = delivery::sha256(b"independent");
    fs::write(case.fx.project.join(file), b"independent").unwrap();
    engine
        .save_requirement(
            case.ticket,
            2,
            None,
            "Second criterion".into(),
            Some(file.into()),
            Some(digest.clone()),
            None,
        )
        .unwrap();
    engine
        .save_requirement(
            case.ticket,
            3,
            None,
            "Review layout".into(),
            None,
            None,
            None,
        )
        .unwrap();
    // Keep one independent file passing even if all behavior evidence becomes unknown.
    engine
        .save_requirement(
            case.ticket,
            4,
            None,
            "Reference bytes".into(),
            Some("expected.txt".into()),
            Some(delivery::sha256(b"42\n")),
            None,
        )
        .unwrap();
    let snapshot = engine
        .capture_delivery(
            case.ticket,
            5,
            vec!["expected.txt".into(), "result.txt".into(), file.into()],
        )
        .unwrap()
        .id;
    engine
        .run_delivery_check(case.ticket, 5, snapshot, case.requirement)
        .unwrap();
    let partial = engine
        .prepare_delivery(case.ticket, 5, snapshot, 1)
        .unwrap();
    assert_eq!(partial.status, "partial");
    assert_eq!(partial.unmet.len(), 1);
    fs::remove_file(case.fx.data.join(delivery::OBJECT_DIRECTORY).join(digest)).unwrap();
    let changed = engine.delivery_summary(case.ticket).unwrap().gate;
    assert_eq!(changed.status, "partial");
    assert!(changed.unmet.len() > partial.unmet.len());
    assert!(
        engine
            .accept_delivery(case.ticket, partial.id, "partial")
            .is_err()
    );
    assert!(
        engine
            .delivery_summary(case.ticket)
            .unwrap()
            .gate
            .acceptance
            .is_none()
    );
}

#[tokio::test]
async fn source_fast_forward_during_check_does_not_transfer_frozen_pass_to_merged_files() {
    let case = Scenario::new().await;
    let project = &case.fx.project;
    git(project, &["init"]);
    git(project, &["config", "core.autocrlf", "false"]);
    git(project, &["add", "."]);
    git(project, &["commit", "--allow-empty", "-m", "tested files"]);
    let branch = git(project, &["branch", "--show-current"]);
    git(project, &["checkout", "-b", "candidate"]);
    fs::write(project.join("result.txt"), "43\n").unwrap();
    git(project, &["commit", "-am", "candidate changes the answer"]);
    git(project, &["checkout", &branch]);
    let merged = std::cell::Cell::new(false);
    let check = case
        .fx
        .engine
        .run_delivery_check_with_cancel(case.ticket, 2, case.snapshot, case.requirement, &|| {
            if !merged.replace(true) {
                git(project, &["merge", "--ff-only", "candidate"]);
            }
            false
        })
        .unwrap();
    assert!(merged.get());
    assert_eq!(check.status, "pass", "{check:?}");
    assert_eq!(case.assess(), "ready");
    let current = delivery::read_summary(&case.fx.data, case.ticket).unwrap();
    assert_eq!(current.requirements[0].status, "stale");
    assert_eq!(
        fs::read_to_string(project.join("result.txt")).unwrap(),
        "43\n"
    );
}
