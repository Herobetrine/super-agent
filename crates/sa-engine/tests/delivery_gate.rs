mod common;
use common::*;
use sa_store::delivery::{self, sha256};
use std::fs;

fn fixture() -> Fx {
    Fx::new(
        script(|_| async { panic!("delivery gate must not call models") }),
        t1_catalog(),
    )
}

fn add_bytes(fx: &Fx, ticket: i64, revision: i64) -> i64 {
    fs::write(fx.project.join("answer.txt"), b"accepted").unwrap();
    fx.engine
        .save_requirement(
            ticket,
            revision,
            None,
            "Declared bytes".into(),
            Some("answer.txt".into()),
            Some(sha256(b"accepted")),
            None,
        )
        .unwrap()
        .requirements[0]
        .id
}

fn capture(fx: &Fx, ticket: i64, revision: i64) -> i64 {
    fx.engine
        .capture_delivery(ticket, revision, vec!["answer.txt".into()])
        .unwrap()
        .id
}

#[tokio::test]
async fn gate_is_opt_in_and_all_operations_preserve_execution_and_call_counts() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    let jobs = fx.all("SELECT * FROM jobs");
    let tickets = fx.all("SELECT * FROM tickets");
    assert!(!fx.engine.delivery_summary(ticket).unwrap().gate.enabled);
    add_bytes(&fx, ticket, 0);
    let snapshot = capture(&fx, ticket, 1);
    assert!(fx.engine.prepare_delivery(ticket, 1, snapshot, 0).is_err());
    assert_eq!(fx.int("SELECT COUNT(*) FROM delivery_assessments"), 0);
    fx.engine.set_delivery_gate(ticket, 0, true).unwrap();
    let assessment = fx.engine.prepare_delivery(ticket, 1, snapshot, 1).unwrap();
    assert_eq!(assessment.status, "ready");
    assert_eq!(
        fx.engine.delivery_summary(ticket).unwrap().gate.status,
        "ready"
    );
    assert_eq!(jobs, fx.all("SELECT * FROM jobs"));
    assert_eq!(tickets, fx.all("SELECT * FROM tickets"));
    assert_eq!(fx.shared.count(|_| true), 0);
    assert_eq!(fx.int("SELECT COUNT(*) FROM delivery_assessments"), 1);
    assert_eq!(
        fx.int("SELECT COUNT(*) FROM events WHERE kind='delivery.assessed'"),
        1
    );
}

#[tokio::test]
async fn required_unknown_blocks_optional_unknown_allows_ready_and_text_edits_keep_policy() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    let id = add_bytes(&fx, ticket, 0);
    let summary = fx
        .engine
        .save_requirement(ticket, 1, None, "Human review".into(), None, None, None)
        .unwrap();
    let human = summary.requirements[1].id;
    assert!(
        summary
            .requirements
            .iter()
            .all(|item| item.required && !item.withdrawn)
    );
    let first = capture(&fx, ticket, 2);
    fx.engine.set_delivery_gate(ticket, 0, true).unwrap();
    let partial = fx.engine.prepare_delivery(ticket, 2, first, 1).unwrap();
    assert_eq!(partial.status, "partial");
    assert_eq!(partial.unmet.len(), 1);
    assert_eq!(partial.unmet[0].requirement_id, human);
    assert_eq!(partial.unmet[0].status, "unknown");
    fx.engine
        .set_requirement_disposition(
            ticket,
            2,
            human,
            false,
            false,
            "Human review follows separately".into(),
        )
        .unwrap();
    assert_eq!(
        fx.engine.delivery_summary(ticket).unwrap().gate.status,
        "stale"
    );
    assert!(fx.engine.prepare_delivery(ticket, 3, first, 1).is_err());
    let second = capture(&fx, ticket, 3);
    assert_eq!(
        fx.engine
            .prepare_delivery(ticket, 3, second, 1)
            .unwrap()
            .status,
        "ready"
    );
    let summary = fx.engine.delivery_summary(ticket).unwrap();
    assert_eq!(summary.status, "unknown");
    assert_eq!(summary.gate.status, "ready");
    assert!(summary.gate.unmet.is_empty());
    let edited = fx
        .engine
        .save_requirement(
            ticket,
            3,
            Some(human),
            "Reworded human review".into(),
            None,
            None,
            None,
        )
        .unwrap();
    assert!(!edited.requirements[1].required);
    assert_eq!(
        edited.requirements[1].disposition_reason.as_deref(),
        Some("Human review follows separately")
    );
    assert_eq!(edited.requirements[0].id, id);
    assert_eq!(
        fx.engine.delivery_summary(ticket).unwrap().gate.status,
        "stale"
    );
    let historical = delivery::read_snapshot(&fx.data, ticket, first).unwrap();
    assert!(historical.requirements[1].required);
    assert_eq!(historical.requirements[1].text, "Human review");
    assert_eq!(fx.int("SELECT COUNT(*) FROM delivery_assessments"), 2);
}

#[tokio::test]
async fn withdrawal_requires_reason_contract_cas_and_cannot_create_vacuous_ready() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    let other = fx.create(fx.params()).await;
    let id = add_bytes(&fx, ticket, 0);
    assert!(
        fx.engine
            .set_requirement_disposition(ticket, 1, id, true, true, "  ".into())
            .is_err()
    );
    assert!(
        fx.engine
            .set_requirement_disposition(other, 0, id, true, true, "wrong ticket".into())
            .is_err()
    );
    fx.engine
        .set_requirement_disposition(ticket, 1, id, true, true, "Operator withdrew scope".into())
        .unwrap();
    assert!(
        fx.engine
            .set_requirement_disposition(ticket, 1, id, false, false, "stale".into())
            .is_err()
    );
    let summary = fx.engine.delivery_summary(ticket).unwrap();
    assert!(summary.requirements[0].withdrawn);
    let snapshot = capture(&fx, ticket, 2);
    fx.engine.set_delivery_gate(ticket, 0, true).unwrap();
    let assessment = fx.engine.prepare_delivery(ticket, 2, snapshot, 1).unwrap();
    assert_eq!(assessment.status, "blocked");
    assert!(assessment.reason.contains("没有有效的必需标准"));
    assert!(assessment.unmet.is_empty());
    fx.engine
        .set_requirement_disposition(ticket, 2, id, true, false, "Operator restored scope".into())
        .unwrap();
    let snapshot = capture(&fx, ticket, 3);
    assert_eq!(
        fx.engine
            .prepare_delivery(ticket, 3, snapshot, 1)
            .unwrap()
            .status,
        "ready"
    );
    assert_eq!(
        fx.int("SELECT COUNT(*) FROM delivery_requirement_dispositions"),
        2
    );
}

#[tokio::test]
async fn policy_aba_new_capture_and_stale_expected_revisions_require_explicit_prepare() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    add_bytes(&fx, ticket, 0);
    let first = capture(&fx, ticket, 1);
    fx.engine.set_delivery_gate(ticket, 0, true).unwrap();
    fx.engine.prepare_delivery(ticket, 1, first, 1).unwrap();
    let second = capture(&fx, ticket, 1);
    assert_eq!(
        fx.engine.delivery_summary(ticket).unwrap().gate.status,
        "stale"
    );
    assert!(fx.engine.prepare_delivery(ticket, 1, first, 1).is_err());
    assert!(fx.engine.prepare_delivery(ticket, 0, second, 1).is_err());
    fx.engine.prepare_delivery(ticket, 1, second, 1).unwrap();
    fx.engine.set_delivery_gate(ticket, 1, false).unwrap();
    assert_eq!(
        fx.engine.delivery_summary(ticket).unwrap().gate.status,
        "disabled"
    );
    assert!(fx.engine.prepare_delivery(ticket, 1, second, 2).is_err());
    fx.engine.set_delivery_gate(ticket, 2, true).unwrap();
    assert_eq!(
        fx.engine.delivery_summary(ticket).unwrap().gate.status,
        "stale"
    );
    assert!(fx.engine.set_delivery_gate(ticket, 1, false).is_err());
    assert!(fx.engine.prepare_delivery(ticket, 1, second, 1).is_err());
    fx.engine.prepare_delivery(ticket, 1, second, 3).unwrap();
    assert_eq!(fx.int("SELECT COUNT(*) FROM delivery_gate_policies"), 3);
    assert_eq!(fx.int("SELECT COUNT(*) FROM delivery_assessments"), 3);
}

#[tokio::test]
async fn ready_rechecks_frozen_objects_and_keeps_current_project_match_separate() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    add_bytes(&fx, ticket, 0);
    let snapshot = capture(&fx, ticket, 1);
    fx.engine.set_delivery_gate(ticket, 0, true).unwrap();
    let assessment = fx.engine.prepare_delivery(ticket, 1, snapshot, 1).unwrap();
    fs::write(fx.project.join("answer.txt"), b"later current workspace").unwrap();
    let summary = fx.engine.delivery_summary(ticket).unwrap();
    assert_eq!(summary.status, "stale");
    assert_eq!(summary.gate.status, "ready");
    let object = fx
        .data
        .join(delivery::OBJECT_DIRECTORY)
        .join(sha256(b"accepted"));
    fs::write(&object, b"tampered").unwrap();
    let summary = fx.engine.delivery_summary(ticket).unwrap();
    assert_eq!(summary.gate.status, "blocked");
    assert_eq!(summary.gate.unmet[0].status, "unknown");
    assert_eq!(summary.gate.latest_assessment.unwrap().id, assessment.id);
    fs::remove_file(&object).unwrap();
    assert_eq!(
        fx.engine.delivery_summary(ticket).unwrap().gate.status,
        "blocked"
    );
    assert_eq!(
        fx.engine
            .prepare_delivery(ticket, 1, snapshot, 1)
            .unwrap()
            .status,
        "blocked"
    );
    fs::write(&object, b"accepted").unwrap();
    assert_eq!(
        fx.engine.delivery_summary(ticket).unwrap().gate.status,
        "stale"
    );
    assert_eq!(
        fx.engine
            .prepare_delivery(ticket, 1, snapshot, 1)
            .unwrap()
            .status,
        "ready"
    );
}

#[tokio::test]
async fn broken_assessment_or_manifest_can_be_recovered_without_editing_history() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    let id = add_bytes(&fx, ticket, 0);
    let snapshot = capture(&fx, ticket, 1);
    fx.engine.set_delivery_gate(ticket, 0, true).unwrap();
    fx.engine.prepare_delivery(ticket, 1, snapshot, 1).unwrap();
    fx.engine
        .db()
        .with(|q| {
            q.run("UPDATE delivery_assessments SET unmet='{'", &[])
                .map(|_| ())
        })
        .unwrap();
    assert_eq!(
        fx.engine.delivery_summary(ticket).unwrap().gate.status,
        "blocked"
    );
    fx.engine.set_delivery_gate(ticket, 1, false).unwrap();
    fx.engine
        .save_requirement(
            ticket,
            1,
            Some(id),
            "Still editable".into(),
            Some("answer.txt".into()),
            Some(sha256(b"accepted")),
            None,
        )
        .unwrap();
    fx.engine.set_delivery_gate(ticket, 2, true).unwrap();
    let newer = capture(&fx, ticket, 2);
    fx.engine.prepare_delivery(ticket, 2, newer, 3).unwrap();
    fx.engine
        .db()
        .with(|q| {
            q.run(
                "UPDATE delivery_snapshots SET payload='{}' WHERE id=?",
                &[&newer],
            )
            .map(|_| ())
        })
        .unwrap();
    assert_eq!(
        fx.engine.delivery_summary(ticket).unwrap().gate.status,
        "blocked"
    );
    assert!(fx.engine.prepare_delivery(ticket, 2, newer, 3).is_err());
    assert_eq!(fx.int("SELECT COUNT(*) FROM delivery_assessments"), 2);
    let final_snapshot = capture(&fx, ticket, 2);
    fx.engine
        .prepare_delivery(ticket, 2, final_snapshot, 3)
        .unwrap();
    assert_eq!(
        fx.engine.delivery_summary(ticket).unwrap().gate.status,
        "ready"
    );
}

#[tokio::test]
async fn concurrent_gate_and_disposition_writers_use_compare_and_swap() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    let id = add_bytes(&fx, ticket, 0);
    let outcomes = std::thread::scope(|scope| {
        let first = scope.spawn(|| fx.engine.set_delivery_gate(ticket, 0, true));
        let second = scope.spawn(|| fx.engine.set_delivery_gate(ticket, 0, false));
        [first.join().unwrap(), second.join().unwrap()]
    });
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
    let outcomes = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            fx.engine
                .set_requirement_disposition(ticket, 1, id, false, false, "Optional".into())
        });
        let second = scope.spawn(|| {
            fx.engine
                .set_requirement_disposition(ticket, 1, id, true, true, "Withdraw".into())
        });
        [first.join().unwrap(), second.join().unwrap()]
    });
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
    assert_eq!(
        fx.int("SELECT COUNT(*) FROM delivery_requirement_dispositions"),
        1
    );
}

#[tokio::test]
async fn concurrent_prepare_appends_complete_assessments_and_projects_latest_id() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    add_bytes(&fx, ticket, 0);
    let snapshot = capture(&fx, ticket, 1);
    fx.engine.set_delivery_gate(ticket, 0, true).unwrap();
    let outcomes = std::thread::scope(|scope| {
        let first = scope.spawn(|| fx.engine.prepare_delivery(ticket, 1, snapshot, 1));
        let second = scope.spawn(|| fx.engine.prepare_delivery(ticket, 1, snapshot, 1));
        [
            first.join().unwrap().unwrap(),
            second.join().unwrap().unwrap(),
        ]
    });
    assert!(
        outcomes
            .iter()
            .all(|assessment| assessment.status == "ready" && assessment.unmet.is_empty())
    );
    assert_ne!(outcomes[0].id, outcomes[1].id);
    let gate = fx.engine.delivery_summary(ticket).unwrap().gate;
    assert_eq!(gate.status, "ready");
    assert_eq!(
        gate.latest_assessment.unwrap().id,
        outcomes.iter().map(|item| item.id).max().unwrap()
    );
    assert_eq!(
        fx.int("SELECT COUNT(*) FROM events WHERE kind='delivery.assessed'"),
        2
    );
}

#[tokio::test]
async fn unknown_required_and_forged_serialized_status_never_create_ready() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    fs::write(fx.project.join("answer.txt"), b"accepted").unwrap();
    fx.engine
        .save_requirement(
            ticket,
            0,
            None,
            "Independent human review".into(),
            None,
            None,
            None,
        )
        .unwrap();
    let snapshot = capture(&fx, ticket, 1);
    fx.engine.set_delivery_gate(ticket, 0, true).unwrap();
    let blocked = fx.engine.prepare_delivery(ticket, 1, snapshot, 1).unwrap();
    assert_eq!(blocked.status, "blocked");
    assert_eq!(blocked.unmet[0].status, "unknown");
    fx.engine.db().with(|q| q.run("UPDATE delivery_assessments SET status='ready',unmet='[]',reason='forged ready' WHERE id=?", &[&blocked.id]).map(|_| ())).unwrap();
    let gate = fx.engine.delivery_summary(ticket).unwrap().gate;
    assert_eq!(gate.status, "blocked");
    assert_eq!(gate.unmet[0].status, "unknown");
    assert_ne!(gate.reason, "forged ready");
    assert_eq!(gate.latest_assessment.unwrap().status, "ready");
}

#[tokio::test]
async fn oversized_assessment_json_is_rejected_before_parse_and_does_not_block_recovery() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    let id = add_bytes(&fx, ticket, 0);
    let snapshot = capture(&fx, ticket, 1);
    fx.engine.set_delivery_gate(ticket, 0, true).unwrap();
    fx.engine.prepare_delivery(ticket, 1, snapshot, 1).unwrap();
    let oversized = " ".repeat(delivery::MAX_MANIFEST_BYTES + 1);
    fx.engine
        .db()
        .with(|q| {
            q.run("UPDATE delivery_assessments SET unmet=?", &[&oversized])
                .map(|_| ())
        })
        .unwrap();
    let summary = fx.engine.delivery_summary(ticket).unwrap();
    assert_eq!(summary.gate.status, "blocked");
    assert_eq!(summary.gate.latest_assessment.unwrap().status, "invalid");
    fx.engine.set_delivery_gate(ticket, 1, false).unwrap();
    fx.engine
        .save_requirement(
            ticket,
            1,
            Some(id),
            "Still editable after oversized history".into(),
            Some("answer.txt".into()),
            Some(sha256(b"accepted")),
            None,
        )
        .unwrap();
    assert_eq!(
        fx.engine.delivery_summary(ticket).unwrap().gate.status,
        "disabled"
    );
}

#[tokio::test]
async fn malformed_assessment_column_types_fail_closed_and_allow_new_assessment() {
    for assignment in [
        "unmet=x'5B5D'",
        "unmet=CAST(x'FF' AS TEXT)",
        "created='x'",
        "created=1e999",
        "contract_revision='x'",
        "snapshot_id=-1",
        "policy_revision=0",
        "manifest_sha256=CAST(x'FF' AS TEXT)",
        "status=x'7265616479'",
        "reason=CAST(x'FF' AS TEXT)",
    ] {
        let fx = fixture();
        let ticket = fx.create(fx.params()).await;
        let id = add_bytes(&fx, ticket, 0);
        let snapshot = capture(&fx, ticket, 1);
        fx.engine.set_delivery_gate(ticket, 0, true).unwrap();
        let original = fx.engine.prepare_delivery(ticket, 1, snapshot, 1).unwrap();
        // External damage can bypass the engine connection's foreign keys.
        let corruption = rusqlite::Connection::open(fx.data.join(sa_store::DB_FILE)).unwrap();
        corruption
            .pragma_update(None, "foreign_keys", false)
            .unwrap();
        corruption
            .execute(&format!("UPDATE delivery_assessments SET {assignment}"), [])
            .unwrap();
        drop(corruption);
        let summary = fx.engine.delivery_summary(ticket).unwrap();
        assert_eq!(summary.gate.status, "blocked", "{assignment}");
        let historical = summary.gate.latest_assessment.unwrap();
        assert_eq!(historical.id, original.id, "{assignment}");
        assert_eq!(historical.status, "invalid", "{assignment}");
        fx.engine.set_delivery_gate(ticket, 1, false).unwrap();
        fx.engine
            .save_requirement(
                ticket,
                1,
                Some(id),
                "Recoverable contract".into(),
                Some("answer.txt".into()),
                Some(sha256(b"accepted")),
                None,
            )
            .unwrap();
        fx.engine.set_delivery_gate(ticket, 2, true).unwrap();
        let snapshot = capture(&fx, ticket, 2);
        let recovered = fx.engine.prepare_delivery(ticket, 2, snapshot, 3).unwrap();
        assert_eq!(recovered.status, "ready", "{assignment}");
        assert!(recovered.id > original.id, "{assignment}");
        assert_eq!(fx.int("SELECT COUNT(*) FROM delivery_assessments"), 2);
    }
}

#[tokio::test]
async fn malformed_policy_fields_disable_gate_and_allow_append_at_known_revision() {
    for assignment in [
        "enabled='x'",
        "enabled=2",
        "enabled=x'31'",
        "created='x'",
        "created=1e999",
        "created=CAST(x'FF' AS TEXT)",
    ] {
        let fx = fixture();
        let ticket = fx.create(fx.params()).await;
        let id = add_bytes(&fx, ticket, 0);
        let snapshot = capture(&fx, ticket, 1);
        fx.engine.set_delivery_gate(ticket, 0, true).unwrap();
        fx.engine
            .db()
            .with(|q| {
                q.run(
                    &format!("UPDATE delivery_gate_policies SET {assignment}"),
                    &[],
                )
                .map(|_| ())
            })
            .unwrap();
        let gate = fx.engine.delivery_summary(ticket).unwrap().gate;
        assert_eq!(gate.status, "blocked", "{assignment}");
        assert!(!gate.enabled, "{assignment}");
        assert_eq!(gate.policy_revision, 1);
        assert!(fx.engine.prepare_delivery(ticket, 1, snapshot, 1).is_err());
        fx.engine
            .save_requirement(
                ticket,
                1,
                Some(id),
                "Editable despite damaged policy".into(),
                Some("answer.txt".into()),
                Some(sha256(b"accepted")),
                None,
            )
            .unwrap();
        fx.engine.set_delivery_gate(ticket, 1, false).unwrap();
        fx.engine.set_delivery_gate(ticket, 2, true).unwrap();
        let snapshot = capture(&fx, ticket, 2);
        assert_eq!(
            fx.engine
                .prepare_delivery(ticket, 2, snapshot, 3)
                .unwrap()
                .status,
            "ready",
            "{assignment}"
        );
        assert_eq!(fx.int("SELECT COUNT(*) FROM delivery_gate_policies"), 3);
    }
}

#[tokio::test]
async fn damaged_policy_revision_never_reuses_unknown_high_water_and_keeps_requirements_editable() {
    for assignment in [
        "revision='broken'",
        "revision=0",
        "revision=-1",
        "revision=x'31'",
    ] {
        let fx = fixture();
        let ticket = fx.create(fx.params()).await;
        let id = add_bytes(&fx, ticket, 0);
        let snapshot = capture(&fx, ticket, 1);
        fx.engine.set_delivery_gate(ticket, 0, true).unwrap();
        fx.engine
            .db()
            .with(|q| {
                q.run(
                    &format!("UPDATE delivery_gate_policies SET {assignment}"),
                    &[],
                )
                .map(|_| ())
            })
            .unwrap();
        let gate = fx.engine.delivery_summary(ticket).unwrap().gate;
        assert!(!gate.enabled, "{assignment}");
        assert_eq!(gate.policy_revision, -1, "{assignment}");
        assert_eq!(gate.status, "blocked");
        assert!(gate.reason.contains("维护人员"));
        fx.engine
            .save_requirement(
                ticket,
                1,
                Some(id),
                "Editable despite unknown policy revision".into(),
                Some("answer.txt".into()),
                Some(sha256(b"accepted")),
                None,
            )
            .unwrap();
        assert!(fx.engine.set_delivery_gate(ticket, -1, false).is_err());
        assert!(fx.engine.set_delivery_gate(ticket, 0, true).is_err());
        assert!(fx.engine.prepare_delivery(ticket, 2, snapshot, -1).is_err());
        assert_eq!(fx.int("SELECT COUNT(*) FROM delivery_gate_policies"), 1);
    }
}

#[tokio::test]
async fn snapshot_matching_binds_original_manifest_bytes_and_rejects_different_digest() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    add_bytes(&fx, ticket, 0);
    let snapshot = capture(&fx, ticket, 1);
    let frozen = delivery::read_snapshot(&fx.data, ticket, snapshot).unwrap();
    let mut legacy = serde_json::to_value(frozen).unwrap();
    for requirement in legacy["requirements"].as_array_mut().unwrap() {
        let object = requirement.as_object_mut().unwrap();
        object.remove("required");
        object.remove("withdrawn");
        object.remove("disposition_reason");
    }
    let original = serde_json::to_string(&legacy).unwrap();
    let original_digest = sha256(original.as_bytes());
    fx.engine
        .db()
        .with(|q| {
            q.run(
                "UPDATE delivery_snapshots SET payload=?,sha256=? WHERE id=?",
                &[&original, &original_digest, &snapshot],
            )
            .map(|_| ())
        })
        .unwrap();
    let verified =
        delivery::read_snapshot_matching(&fx.data, ticket, snapshot, &original_digest).unwrap();
    assert!(verified.requirements[0].required);
    assert_ne!(
        sha256(serde_json::to_string(&verified).unwrap().as_bytes()),
        original_digest
    );
    legacy["requirements"][0]["text"] = "Externally substituted manifest".into();
    let substituted = serde_json::to_string(&legacy).unwrap();
    let substituted_digest = sha256(substituted.as_bytes());
    fx.engine
        .db()
        .with(|q| {
            q.run(
                "UPDATE delivery_snapshots SET payload=?,sha256=? WHERE id=?",
                &[&substituted, &substituted_digest, &snapshot],
            )
            .map(|_| ())
        })
        .unwrap();
    assert!(
        delivery::read_snapshot_matching(&fx.data, ticket, snapshot, &original_digest).is_err()
    );
    fx.engine
        .db()
        .with(|q| {
            q.run(
                "UPDATE delivery_snapshots SET payload=?,sha256=? WHERE id=?",
                &[&original, &original_digest, &snapshot],
            )
            .map(|_| ())
        })
        .unwrap();
    fx.engine.set_delivery_gate(ticket, 0, true).unwrap();
    let prepared = fx.engine.prepare_delivery(ticket, 1, snapshot, 1).unwrap();
    assert_eq!(prepared.status, "ready");
    assert_eq!(prepared.manifest_sha256, original_digest);
}
