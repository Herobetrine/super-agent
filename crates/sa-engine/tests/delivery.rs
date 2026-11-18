mod common;
use common::*;
use sa_store::delivery::{self, sha256};
use std::{fs, process::Command};

fn fixture() -> Fx {
    let fx = Fx::new(
        script(|_| async {
            Ok("SA_REPORT {\"status\":\"done\",\"summary\":\"all requirements passed\"}".into())
        }),
        t1_catalog(),
    );
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&fx.project)
            .status()
            .unwrap()
            .success()
    );
    fx
}

#[tokio::test]
async fn hash_evidence_is_platform_produced_revisioned_and_historical() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    assert_eq!(
        fx.engine.delivery_summary(ticket).unwrap().status,
        "legacy_unassessed"
    );
    fs::write(fx.project.join("answer.txt"), b"accepted bytes").unwrap();
    let summary = fx
        .engine
        .save_requirement(
            ticket,
            0,
            None,
            "Preserve declared bytes".into(),
            Some("answer.txt".into()),
            Some(sha256(b"accepted bytes").to_uppercase()),
            None,
        )
        .unwrap();
    let requirement = summary.requirements[0].id;
    assert_eq!(summary.status, "unknown");
    assert_eq!(summary.requirements[0].source, "operator");
    assert_eq!(
        summary.requirements[0].expected_sha256.as_deref(),
        Some(sha256(b"accepted bytes").as_str())
    );
    let frozen = fx
        .engine
        .capture_delivery(ticket, 1, vec!["answer.txt".into()])
        .unwrap();
    assert_eq!(frozen.evidence[0].status, "pass");
    assert_eq!(frozen.evidence[0].source, "platform_sha256");
    assert_eq!(fx.engine.delivery_summary(ticket).unwrap().status, "pass");
    fs::write(fx.project.join("answer.txt"), b"later edit").unwrap();
    assert_eq!(fx.engine.delivery_summary(ticket).unwrap().status, "stale");
    assert_eq!(
        delivery::snapshot_file_sha256(&fx.data, ticket, frozen.id, "answer.txt").unwrap(),
        sha256(b"accepted bytes")
    );
    assert_eq!(
        delivery::read_snapshot(&fx.data, ticket, frozen.id)
            .unwrap()
            .evidence[0]
            .status,
        "pass"
    );
    let new = fx
        .engine
        .save_requirement(
            ticket,
            1,
            Some(requirement),
            "Changed criterion".into(),
            Some("answer.txt".into()),
            Some(sha256(b"later edit")),
            None,
        )
        .unwrap();
    assert_eq!(new.contract_revision, 2);
    assert_eq!(new.requirements[0].revision, 2);
    assert_eq!(new.requirements[0].status, "unknown");
    assert!(new.warning.is_some());
    assert_eq!(fx.engine.delivery_summary(ticket).unwrap().status, "stale");
    assert!(
        fx.engine
            .capture_delivery(ticket, 1, vec!["answer.txt".into()])
            .is_err()
    );
    let old = delivery::read_snapshot(&fx.data, ticket, frozen.id).unwrap();
    assert_eq!(old.requirements[0].text, "Preserve declared bytes");
    assert_eq!(old.requirements[0].revision, 1);
    assert_eq!(old.evidence[0].status, "pass");
    let final_snapshot = fx
        .engine
        .capture_delivery(ticket, 2, vec!["answer.txt".into()])
        .unwrap();
    assert_ne!(final_snapshot.id, frozen.id);
    assert_eq!(fx.engine.delivery_summary(ticket).unwrap().status, "pass");
    assert_eq!(
        fx.int("SELECT COUNT(*) FROM delivery_requirement_revisions"),
        2
    );
}

#[tokio::test]
async fn explicit_criteria_fail_and_uncaptured_or_unverifiable_requirements_stay_unknown() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    fs::write(fx.project.join("answer.txt"), b"wrong").unwrap();
    fx.engine
        .save_requirement(
            ticket,
            0,
            None,
            "Text requiring human review".into(),
            None,
            None,
            None,
        )
        .unwrap();
    fx.engine
        .save_requirement(
            ticket,
            1,
            None,
            "Expected bytes".into(),
            Some("answer.txt".into()),
            Some(sha256(b"expected")),
            None,
        )
        .unwrap();
    fx.engine
        .save_requirement(
            ticket,
            2,
            None,
            "Uncaptured bytes".into(),
            Some("missing.txt".into()),
            Some(sha256(b"expected")),
            None,
        )
        .unwrap();
    let frozen = fx
        .engine
        .capture_delivery(ticket, 3, vec!["answer.txt".into()])
        .unwrap();
    assert_eq!(
        frozen
            .evidence
            .iter()
            .map(|item| item.status.as_str())
            .collect::<Vec<_>>(),
        ["unknown", "fail", "unknown"]
    );
    assert_eq!(fx.engine.delivery_summary(ticket).unwrap().status, "fail");
    assert!(delivery::snapshot_file_sha256(&fx.data, ticket, frozen.id, "missing.txt").is_err());
    assert!(delivery::snapshot_file_sha256(&fx.data, ticket, frozen.id, "../outside.txt").is_err());
}

#[tokio::test]
async fn rejects_cross_ticket_references_stale_writers_and_malformed_inputs() {
    let fx = fixture();
    let first = fx.create(fx.params()).await;
    let second = fx.create(fx.params()).await;
    let second_job = fx.engine.root(second).unwrap()["id"].as_i64().unwrap();
    assert!(
        fx.engine
            .save_requirement(
                first,
                0,
                None,
                "invalid job".into(),
                None,
                None,
                Some(second_job)
            )
            .is_err()
    );
    let saved = fx
        .engine
        .save_requirement(first, 0, None, "first".into(), None, None, None)
        .unwrap();
    assert!(
        fx.engine
            .save_requirement(first, 0, None, "stale editor".into(), None, None, None)
            .is_err()
    );
    assert!(
        fx.engine
            .save_requirement(
                second,
                0,
                Some(saved.requirements[0].id),
                "cross ticket".into(),
                None,
                None,
                None
            )
            .is_err()
    );
    assert!(
        fx.engine
            .save_requirement(
                first,
                1,
                None,
                "bad sha".into(),
                Some("answer.txt".into()),
                Some("bad".into()),
                None
            )
            .is_err()
    );
    assert!(
        fx.engine
            .save_requirement(
                first,
                1,
                None,
                "path only".into(),
                Some("answer.txt".into()),
                None,
                None
            )
            .is_err()
    );
    fs::write(fx.project.join("answer.txt"), b"bytes").unwrap();
    let frozen = fx
        .engine
        .capture_delivery(first, 1, vec!["answer.txt".into()])
        .unwrap();
    assert!(delivery::read_snapshot(&fx.data, second, frozen.id).is_err());
    assert!(fx.engine.delivery_summary(999_999).is_err());
    assert_eq!(
        fx.int("SELECT COUNT(*) FROM delivery_requirement_revisions"),
        1
    );
}

#[tokio::test]
async fn missing_corrupt_objects_and_manifest_never_preserve_a_pass() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    fs::write(fx.project.join("answer.txt"), b"bytes").unwrap();
    fx.engine
        .save_requirement(
            ticket,
            0,
            None,
            "bytes".into(),
            Some("answer.txt".into()),
            Some(sha256(b"bytes")),
            None,
        )
        .unwrap();
    let frozen = fx
        .engine
        .capture_delivery(ticket, 1, vec!["answer.txt".into()])
        .unwrap();
    let object = fx
        .data
        .join(delivery::OBJECT_DIRECTORY)
        .join(&frozen.files[0].sha256);
    fs::remove_file(&object).unwrap();
    assert_eq!(
        fx.engine.delivery_summary(ticket).unwrap().status,
        "unknown"
    );
    assert_eq!(
        delivery::read_snapshot(&fx.data, ticket, frozen.id)
            .unwrap()
            .evidence[0]
            .status,
        "unknown"
    );
    assert!(delivery::snapshot_file_sha256(&fx.data, ticket, frozen.id, "answer.txt").is_err());
    fs::write(&object, b"fake!").unwrap();
    assert_eq!(
        fx.engine.delivery_summary(ticket).unwrap().status,
        "unknown"
    );
    let repaired = fx
        .engine
        .capture_delivery(ticket, 1, vec!["answer.txt".into()])
        .unwrap();
    assert_eq!(repaired.evidence[0].status, "pass");
    let backups = fs::read_dir(fx.data.join(delivery::OBJECT_DIRECTORY).join("quarantine"))
        .unwrap()
        .collect::<std::io::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(backups.len(), 1);
    assert_eq!(fs::read(backups[0].path()).unwrap(), b"fake!");
    assert_eq!(
        delivery::read_snapshot(&fx.data, ticket, frozen.id)
            .unwrap()
            .evidence[0]
            .status,
        "pass"
    );
    fx.engine
        .db()
        .with(|q| {
            q.run(
                "UPDATE delivery_snapshots SET payload='{}' WHERE id=?",
                &[&frozen.id],
            )?;
            Ok(())
        })
        .unwrap();
    assert!(delivery::read_snapshot(&fx.data, ticket, frozen.id).is_err());
}

#[tokio::test]
async fn rejects_escape_duplicates_directories_and_oversized_capture_atomically() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    fs::write(fx.project.join("ok.txt"), b"ok").unwrap();
    for path in [
        "../outside",
        "C:/outside",
        "ok.txt:stream",
        "./ok.txt",
        "sub/../ok.txt",
        ".",
        "CON",
        "name.",
        "name ",
    ] {
        assert!(
            fx.engine
                .capture_delivery(ticket, 0, vec![path.into()])
                .is_err(),
            "{path}"
        );
    }
    assert!(
        fx.engine
            .capture_delivery(ticket, 0, vec!["ok.txt".into(), "OK.TXT".into()])
            .is_err()
    );
    fs::create_dir(fx.project.join("directory")).unwrap();
    assert!(
        fx.engine
            .capture_delivery(ticket, 0, vec!["directory".into()])
            .is_err()
    );
    fs::write(
        fx.project.join("large"),
        vec![0; delivery::MAX_FILE_BYTES as usize + 1],
    )
    .unwrap();
    assert!(
        fx.engine
            .capture_delivery(ticket, 0, vec!["ok.txt".into(), "large".into()])
            .is_err()
    );
    assert_eq!(fx.int("SELECT COUNT(*) FROM delivery_snapshots"), 0);
}

#[tokio::test]
async fn rejects_reparse_point_sources_and_object_directories() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    let outside = fx.dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("answer.txt"), b"outside").unwrap();
    let link = fx.project.join("linked");
    #[cfg(windows)]
    assert!(
        Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .output()
            .unwrap()
            .status
            .success()
    );
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    assert!(
        fx.engine
            .capture_delivery(ticket, 0, vec!["linked/answer.txt".into()])
            .is_err()
    );
    let objects = fx.data.join(delivery::OBJECT_DIRECTORY);
    #[cfg(windows)]
    assert!(
        Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&objects)
            .arg(&outside)
            .output()
            .unwrap()
            .status
            .success()
    );
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &objects).unwrap();
    fs::write(fx.project.join("answer.txt"), b"outside").unwrap();
    assert!(
        fx.engine
            .capture_delivery(ticket, 0, vec!["answer.txt".into()])
            .is_err()
    );
    assert_eq!(fx.int("SELECT COUNT(*) FROM delivery_snapshots"), 0);
    assert!(!outside.join(sha256(b"outside")).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_completion_claims_and_done_do_not_supply_requirement_evidence() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    fx.engine
        .save_requirement(
            ticket,
            0,
            None,
            "Requires an independent acceptance".into(),
            None,
            None,
            None,
        )
        .unwrap();
    fx.engine.start().unwrap();
    fx.finish(ticket, "done").await;
    let summary = fx.engine.delivery_summary(ticket).unwrap();
    assert_eq!(summary.status, "unknown");
    assert!(summary.latest_snapshot.is_none());
    assert!(summary.requirements[0].evidence.is_none());
    assert_eq!(fx.engine.root(ticket).unwrap()["state"], "done");
}

#[tokio::test]
async fn concurrent_operator_edits_use_contract_compare_and_swap() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    let barrier = std::sync::Barrier::new(2);
    let outcomes = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            barrier.wait();
            fx.engine
                .save_requirement(ticket, 0, None, "First editor".into(), None, None, None)
        });
        let second = scope.spawn(|| {
            barrier.wait();
            fx.engine
                .save_requirement(ticket, 0, None, "Second editor".into(), None, None, None)
        });
        [first.join().unwrap(), second.join().unwrap()]
    });
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
    assert_eq!(
        fx.engine
            .delivery_summary(ticket)
            .unwrap()
            .contract_revision,
        1
    );
    assert_eq!(fx.int("SELECT COUNT(*) FROM delivery_requirements"), 1);
    assert_eq!(
        fx.int("SELECT COUNT(*) FROM events WHERE kind='requirement.revised'"),
        1
    );
}

#[tokio::test]
async fn concurrent_revision_cannot_attach_old_capture_to_a_new_contract() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    let bytes = vec![7u8; delivery::MAX_FILE_BYTES as usize];
    fs::write(fx.project.join("answer.txt"), &bytes).unwrap();
    let saved = fx
        .engine
        .save_requirement(
            ticket,
            0,
            None,
            "Initial criterion".into(),
            Some("answer.txt".into()),
            Some(sha256(&bytes)),
            None,
        )
        .unwrap();
    let requirement = saved.requirements[0].id;
    let barrier = std::sync::Barrier::new(2);
    let (capture, edit) = std::thread::scope(|scope| {
        let capture = scope.spawn(|| {
            barrier.wait();
            fx.engine
                .capture_delivery(ticket, 1, vec!["answer.txt".into()])
        });
        let edit = scope.spawn(|| {
            barrier.wait();
            fx.engine.save_requirement(
                ticket,
                1,
                Some(requirement),
                "Revised criterion".into(),
                Some("answer.txt".into()),
                Some(sha256(b"other bytes")),
                None,
            )
        });
        (capture.join().unwrap(), edit.join().unwrap())
    });
    assert!(edit.is_ok());
    let summary = fx.engine.delivery_summary(ticket).unwrap();
    assert_eq!(summary.contract_revision, 2);
    assert_ne!(summary.status, "pass");
    if let Ok(frozen) = capture {
        assert_eq!(frozen.contract_revision, 1);
        assert_eq!(summary.status, "stale");
    } else {
        assert_eq!(fx.int("SELECT COUNT(*) FROM delivery_snapshots"), 0);
    }
}

#[tokio::test]
async fn read_side_recomputes_forged_status_and_accepts_uppercase_hashes() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    fs::write(fx.project.join("answer.txt"), b"bytes").unwrap();
    fx.engine
        .save_requirement(
            ticket,
            0,
            None,
            "Declared bytes".into(),
            Some("answer.txt".into()),
            Some(sha256(b"bytes")),
            None,
        )
        .unwrap();
    fx.engine
        .save_requirement(ticket, 1, None, "Human review".into(), None, None, None)
        .unwrap();
    let mut frozen = fx
        .engine
        .capture_delivery(ticket, 2, vec!["answer.txt".into()])
        .unwrap();
    frozen.files[0].sha256.make_ascii_uppercase();
    frozen.requirements[0]
        .expected_sha256
        .as_mut()
        .unwrap()
        .make_ascii_uppercase();
    frozen.evidence[1].status = "pass".into();
    frozen.requirements[1].status = "pass".into();
    let payload = serde_json::to_string(&frozen).unwrap();
    fx.engine
        .db()
        .with(|q| {
            q.run(
                "UPDATE delivery_snapshots SET payload=?,sha256=? WHERE id=?",
                &[
                    &payload,
                    &sha256(payload.as_bytes()).to_uppercase(),
                    &frozen.id,
                ],
            )?;
            Ok(())
        })
        .unwrap();
    let verified = delivery::read_snapshot(&fx.data, ticket, frozen.id).unwrap();
    assert_eq!(verified.evidence[0].status, "pass");
    assert_eq!(verified.evidence[1].status, "unknown");
    assert_eq!(verified.files[0].sha256, sha256(b"bytes"));
    frozen.requirements[0].expected_sha256 = Some(sha256(b"wrong"));
    frozen.evidence[0].status = "pass".into();
    let payload = serde_json::to_string(&frozen).unwrap();
    fx.engine
        .db()
        .with(|q| {
            q.run(
                "UPDATE delivery_snapshots SET payload=?,sha256=? WHERE id=?",
                &[&payload, &sha256(payload.as_bytes()), &frozen.id],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        delivery::read_snapshot(&fx.data, ticket, frozen.id)
            .unwrap()
            .evidence[0]
            .status,
        "fail"
    );
}

#[tokio::test]
async fn damaged_latest_manifest_keeps_requirements_editable_and_recapture_recovers() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    fs::write(fx.project.join("answer.txt"), b"bytes").unwrap();
    let saved = fx
        .engine
        .save_requirement(
            ticket,
            0,
            None,
            "Initial".into(),
            Some("answer.txt".into()),
            Some(sha256(b"bytes")),
            None,
        )
        .unwrap();
    let requirement = saved.requirements[0].id;
    let frozen = fx
        .engine
        .capture_delivery(ticket, 1, vec!["answer.txt".into()])
        .unwrap();
    fx.engine
        .db()
        .with(|q| {
            q.run(
                "UPDATE delivery_snapshots SET payload='{}' WHERE id=?",
                &[&frozen.id],
            )?;
            Ok(())
        })
        .unwrap();
    let damaged = fx.engine.delivery_summary(ticket).unwrap();
    assert_eq!(damaged.status, "unknown");
    assert_eq!(damaged.contract_revision, 1);
    assert_eq!(damaged.requirements[0].text, "Initial");
    assert!(
        damaged
            .warning
            .as_ref()
            .unwrap()
            .contains(&frozen.id.to_string())
    );
    assert!(damaged.latest_snapshot.is_none());
    let committed = fx
        .engine
        .save_requirement(
            ticket,
            1,
            Some(requirement),
            "Updated".into(),
            Some("answer.txt".into()),
            Some(sha256(b"bytes")),
            None,
        )
        .unwrap();
    assert_eq!(committed.contract_revision, 2);
    assert_eq!(committed.requirements[0].text, "Updated");
    assert_eq!(committed.requirements[0].status, "unknown");
    assert!(committed.warning.is_some());
    assert_eq!(
        fx.int("SELECT COUNT(*) FROM delivery_requirement_revisions"),
        2
    );
    let refreshed = fx.engine.delivery_summary(ticket).unwrap();
    assert_eq!(refreshed.contract_revision, 2);
    assert!(refreshed.warning.is_some());
    let recovered = fx
        .engine
        .capture_delivery(ticket, 2, vec!["answer.txt".into()])
        .unwrap();
    assert_ne!(recovered.id, frozen.id);
    let refreshed = fx.engine.delivery_summary(ticket).unwrap();
    assert_eq!(refreshed.status, "pass");
    assert!(refreshed.warning.is_none());
}

#[cfg(windows)]
#[tokio::test]
async fn object_repair_refuses_busy_files_without_quarantining_or_rewriting() {
    use std::os::windows::fs::OpenOptionsExt;
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    fs::write(fx.project.join("answer.txt"), b"bytes").unwrap();
    let frozen = fx
        .engine
        .capture_delivery(ticket, 0, vec!["answer.txt".into()])
        .unwrap();
    let directory = fx.data.join(delivery::OBJECT_DIRECTORY);
    let object = directory.join(&frozen.files[0].sha256);
    fs::write(&object, b"damaged").unwrap();
    let writer = fs::OpenOptions::new()
        .write(true)
        .share_mode(7)
        .open(&object)
        .unwrap();
    assert!(
        fx.engine
            .capture_delivery(ticket, 0, vec!["answer.txt".into()])
            .is_err()
    );
    assert_eq!(fs::read(&object).unwrap(), b"damaged");
    assert!(!directory.join("quarantine").exists());
    drop(writer);
    assert!(
        fx.engine
            .capture_delivery(ticket, 0, vec!["answer.txt".into()])
            .is_ok()
    );
    assert_eq!(fs::read(&object).unwrap(), b"bytes");
}

#[cfg(windows)]
#[tokio::test]
async fn object_repair_refuses_reparse_leaf_and_quarantine_directory() {
    let fx = fixture();
    let ticket = fx.create(fx.params()).await;
    fs::write(fx.project.join("answer.txt"), b"bytes").unwrap();
    let frozen = fx
        .engine
        .capture_delivery(ticket, 0, vec!["answer.txt".into()])
        .unwrap();
    let directory = fx.data.join(delivery::OBJECT_DIRECTORY);
    let object = directory.join(&frozen.files[0].sha256);
    fs::remove_file(&object).unwrap();
    let outside = fx.dir.path().join("repair-outside");
    fs::create_dir(&outside).unwrap();
    assert!(
        Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&object)
            .arg(&outside)
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        fx.engine
            .capture_delivery(ticket, 0, vec!["answer.txt".into()])
            .is_err()
    );
    assert!(!directory.join("quarantine").exists());
    fs::remove_dir(&object).unwrap();
    fs::write(&object, b"damaged").unwrap();
    assert!(
        Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(directory.join("quarantine"))
            .arg(&outside)
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        fx.engine
            .capture_delivery(ticket, 0, vec!["answer.txt".into()])
            .is_err()
    );
    assert_eq!(fs::read(&object).unwrap(), b"damaged");
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
}
