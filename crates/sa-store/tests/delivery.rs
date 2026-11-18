use sa_store::{Store, delivery};

#[test]
fn readonly_legacy_projection_creates_no_delivery_schema_or_objects() {
    let data = tempfile::tempdir().unwrap();
    drop(Store::open(data.path()).unwrap());
    let writer = rusqlite::Connection::open(data.path().join(sa_store::DB_FILE)).unwrap();
    writer
        .execute(
            "INSERT INTO tickets(goal,cwd,writes,lead,created) VALUES('old','.',0,'codex',1)",
            [],
        )
        .unwrap();
    let ticket = writer.last_insert_rowid();
    let before: i64 = writer
        .query_row("SELECT COUNT(*) FROM sqlite_master", [], |row| row.get(0))
        .unwrap();
    let summary = delivery::read_summary(data.path(), ticket).unwrap();
    assert_eq!(summary.status, "legacy_unassessed");
    assert_eq!(summary.contract_revision, 0);
    assert!(summary.requirements.is_empty());
    assert!(summary.latest_snapshot.is_none());
    assert!(!summary.gate.enabled);
    assert_eq!(summary.gate.policy_revision, 0);
    assert_eq!(summary.gate.status, "disabled");
    let after: i64 = writer
        .query_row("SELECT COUNT(*) FROM sqlite_master", [], |row| row.get(0))
        .unwrap();
    assert_eq!(before, after);
    assert!(!data.path().join(delivery::OBJECT_DIRECTORY).exists());
    assert!(delivery::read_snapshot(data.path(), ticket, 1).is_err());
}

#[test]
fn readonly_pre_gate_delivery_schema_keeps_required_defaults_without_migration() {
    let data = tempfile::tempdir().unwrap();
    drop(Store::open(data.path()).unwrap());
    let writer = rusqlite::Connection::open(data.path().join(sa_store::DB_FILE)).unwrap();
    let old_schema = delivery::SCHEMA
        .split("CREATE TABLE IF NOT EXISTS delivery_requirement_dispositions")
        .next()
        .unwrap();
    writer.execute_batch(old_schema).unwrap();
    writer.execute_batch("INSERT INTO tickets(id,goal,cwd,writes,lead,created) VALUES(1,'legacy','.',0,'codex',1);
        INSERT INTO delivery_contracts(ticket,revision) VALUES(1,1);
        INSERT INTO delivery_requirements(id,ticket) VALUES(1,1);
        INSERT INTO delivery_requirement_revisions(requirement,revision,contract_revision,text,source,created) VALUES(1,1,1,'legacy required','operator',1);").unwrap();
    let before: i64 = writer
        .query_row("SELECT COUNT(*) FROM sqlite_master", [], |row| row.get(0))
        .unwrap();
    let summary = delivery::read_summary(data.path(), 1).unwrap();
    assert_eq!(summary.gate.status, "disabled");
    assert_eq!(summary.gate.policy_revision, 0);
    assert!(summary.requirements[0].required);
    assert!(!summary.requirements[0].withdrawn);
    assert!(summary.requirements[0].disposition_reason.is_none());
    let after: i64 = writer
        .query_row("SELECT COUNT(*) FROM sqlite_master", [], |row| row.get(0))
        .unwrap();
    assert_eq!(before, after);
}

#[test]
fn old_frozen_requirement_and_summary_json_remain_readable_with_gate_disabled() {
    let requirement: delivery::RequirementView = serde_json::from_value(serde_json::json!({
        "id": 1, "revision": 1, "text": "legacy", "path": null,
        "expected_sha256": null, "job": null, "source": "operator", "status": "unknown", "evidence": null
    })).unwrap();
    assert!(requirement.required);
    assert!(!requirement.withdrawn);
    let summary: delivery::DeliverySummary = serde_json::from_value(serde_json::json!({
        "ticket": 1, "contract_revision": 0, "requirements": [], "latest_snapshot": null, "status": "legacy_unassessed"
    })).unwrap();
    assert_eq!(summary.gate.status, "disabled");
    assert!(!summary.gate.enabled);
}

#[test]
fn relative_path_validation_is_platform_independent() {
    for path in [
        "/root",
        "\\\\server\\share",
        "C:\\file",
        "a/../b",
        "a//b",
        "a/./b",
        "aux.txt",
        "com1.log",
        "a/file ",
        "a/file.",
        "a:file",
        "a\0file",
    ] {
        assert!(delivery::normalize_path(path).is_err(), "{path}");
    }
    assert_eq!(
        delivery::normalize_path("目录\\文件.txt").unwrap(),
        "目录/文件.txt"
    );
    assert!(delivery::reject_links(std::path::Path::new(".")).is_ok());
}

#[cfg(windows)]
#[test]
fn summary_reads_allow_an_existing_agent_writer() {
    use std::os::windows::fs::OpenOptionsExt;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("active.txt");
    std::fs::write(&path, b"active bytes").unwrap();
    let _writer = std::fs::OpenOptions::new()
        .write(true)
        .share_mode(7)
        .open(&path)
        .unwrap();
    assert_eq!(delivery::read_shared_file(&path).unwrap(), b"active bytes");
    assert!(delivery::read_stable_file(&path).is_err());
}
