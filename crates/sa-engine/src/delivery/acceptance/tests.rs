use super::*;
use crate::EngineOptions;

struct Fixture {
    directory: tempfile::TempDir,
    engine: Engine,
    assessment: DeliveryAssessment,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        fs::create_dir(&project).unwrap();
        let engine = Engine::open(
            directory.path().join("ledger"),
            vec![],
            EngineOptions::default(),
        )
        .unwrap();
        engine.db().with(|q| q.run("INSERT INTO tickets(id,goal,cwd,writes,lead,created) VALUES(1,'fixture',?,0,'codex',1)",
            &[&project.to_string_lossy().as_ref()])).unwrap();
        for (index, name) in ["one", "two"].into_iter().enumerate() {
            fs::write(project.join(name), name.as_bytes()).unwrap();
            engine
                .save_requirement(
                    1,
                    index as i64,
                    None,
                    name.into(),
                    Some(name.into()),
                    Some(delivery::sha256(name.as_bytes())),
                    None,
                )
                .unwrap();
        }
        engine.set_delivery_gate(1, 0, true).unwrap();
        let snapshot = engine
            .capture_delivery(1, 2, vec!["one".into(), "two".into()])
            .unwrap();
        let assessment = engine.prepare_delivery(1, 2, snapshot.id, 1).unwrap();
        Self {
            directory,
            engine,
            assessment,
        }
    }
    fn object(&self, name: &str) -> PathBuf {
        self.directory
            .path()
            .join("ledger")
            .join(delivery::OBJECT_DIRECTORY)
            .join(delivery::sha256(name.as_bytes()))
    }
    fn count(&self, sql: &str) -> i64 {
        self.engine.db().with(|q| q.int(sql, &[])).unwrap()
    }
}

#[test]
fn same_snapshot_new_assessment_and_policy_changes_reject_an_inflight_confirmation() {
    for change in [
        "reassess",
        "policy",
        "contract",
        "snapshot",
        "manifest",
        "stored_scope",
    ] {
        let f = Fixture::new();
        let snapshot = f.assessment.snapshot_id;
        let result = f
            .engine
            .accept_delivery_inner(1, f.assessment.id, "ready", || {
                match change {
                    "reassess" => {
                        f.engine.prepare_delivery(1, 2, snapshot, 1)?;
                    }
                    "policy" => {
                        f.engine.set_delivery_gate(1, 1, false)?;
                        f.engine.set_delivery_gate(1, 2, true)?;
                    }
                    "contract" => {
                        f.engine
                            .save_requirement(1, 2, None, "extra".into(), None, None, None)?;
                    }
                    "snapshot" => {
                        f.engine
                            .capture_delivery(1, 2, vec!["one".into(), "two".into()])?;
                    }
                    "manifest" => {
                        f.engine.db().with(|q| {
                            q.run(
                                "UPDATE delivery_snapshots SET payload='{}' WHERE id=?",
                                &[&snapshot],
                            )
                        })?;
                    }
                    "stored_scope" => {
                        f.engine.db().with(|q| {
                            q.run(
                                "UPDATE delivery_assessments SET reason='changed' WHERE id=?",
                                &[&f.assessment.id],
                            )
                        })?;
                    }
                    _ => unreachable!(),
                }
                Ok(())
            });
        assert!(result.is_err(), "{change}");
        assert_eq!(
            f.count("SELECT COUNT(*) FROM delivery_acceptances"),
            0,
            "{change}"
        );
        assert_eq!(
            f.count("SELECT COUNT(*) FROM events WHERE kind='delivery.accepted'"),
            0,
            "{change}"
        );
    }
}

#[test]
fn damaged_ready_and_changed_partial_scope_require_fresh_operator_confirmation() {
    let f = Fixture::new();
    fs::remove_file(f.object("two")).unwrap();
    assert!(
        f.engine
            .accept_delivery(1, f.assessment.id, "ready")
            .is_err()
    );
    assert!(
        f.engine
            .accept_delivery(1, f.assessment.id, "partial")
            .is_err()
    );
    let partial = f
        .engine
        .prepare_delivery(1, 2, f.assessment.snapshot_id, 1)
        .unwrap();
    assert_eq!(partial.status, "partial");
    // Restoring a missing object in the last commit window changes the
    // exact scope even though the assessment row itself has not changed.
    assert!(
        f.engine
            .accept_delivery_inner(1, partial.id, "partial", || {
                fs::write(f.object("two"), b"two")?;
                Ok(())
            })
            .is_err()
    );
    assert_eq!(f.count("SELECT COUNT(*) FROM delivery_acceptances"), 0);
    fs::remove_file(f.object("two")).unwrap();
    let first = f.engine.accept_delivery(1, partial.id, "partial").unwrap();
    let accepted = first.gate.acceptance.unwrap();
    assert_eq!(accepted.assessment_id, partial.id);
    assert_eq!(first.gate.unmet, partial.unmet);
    let second = f.engine.accept_delivery(1, partial.id, "partial").unwrap();
    assert_eq!(second.gate.acceptance.unwrap().id, accepted.id);
    assert_eq!(
        f.count("SELECT COUNT(*) FROM events WHERE kind='delivery.accepted'"),
        1
    );
    assert_eq!(f.count("SELECT COUNT(*) FROM delivery_acceptances"), 1);
}

#[test]
fn a_behavior_rerun_cannot_inherit_the_pending_human_confirmation() {
    let f = Fixture::new();
    let first = f.engine.delivery_summary(1).unwrap().requirements[0].id;
    let command = vec!["git".to_owned(), "--version".to_owned()];
    f.engine
        .set_project_checks(
            f.directory.path().join("project").to_str().unwrap(),
            vec![command.clone()],
        )
        .unwrap();
    f.engine.adopt_delivery_check(1, 2, first, command).unwrap();
    let snapshot = f
        .engine
        .capture_delivery(1, 3, vec!["one".into(), "two".into()])
        .unwrap()
        .id;
    assert_eq!(
        f.engine
            .run_delivery_check(1, 3, snapshot, first)
            .unwrap()
            .status,
        "pass"
    );
    let assessment = f.engine.prepare_delivery(1, 3, snapshot, 1).unwrap();
    assert_eq!(assessment.status, "ready");
    let result = f
        .engine
        .accept_delivery_inner(1, assessment.id, "ready", || {
            assert_eq!(
                f.engine.run_delivery_check(1, 3, snapshot, first)?.status,
                "pass"
            );
            Ok(())
        });
    assert!(result.is_err());
    assert_eq!(f.count("SELECT COUNT(*) FROM delivery_acceptances"), 0);
}

#[cfg(windows)]
#[test]
fn windows_pins_verified_objects_through_the_acceptance_commit() {
    let f = Fixture::new();
    let summary = f
        .engine
        .accept_delivery_inner(1, f.assessment.id, "ready", || {
            assert!(fs::write(f.object("two"), b"bad").is_err());
            Ok(())
        })
        .unwrap();
    assert_eq!(
        summary.gate.acceptance.unwrap().assessment_id,
        f.assessment.id
    );
    fs::write(f.object("two"), b"bad").unwrap();
    let summary = f.engine.delivery_summary(1).unwrap();
    assert!(summary.gate.acceptance.is_none());
    assert_eq!(
        summary.gate.latest_acceptance.unwrap().assessment_id,
        f.assessment.id
    );
}
