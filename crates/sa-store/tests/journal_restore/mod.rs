use super::{
    DB_FILE, Files, JournalError, Phase, Record, RestoreOutcome, lock, marker_path, read_record,
    record_path, restore_snapshot, restore_with_cleanup, retry_restore_cleanup, with_suffix,
    write_record,
};
use crate::journal::{inspect, open_journal};
use rusqlite::Connection;
use std::{collections::BTreeMap, fs, path::Path, time::SystemTime};

fn fixture(path: &Path, value: i64) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch("CREATE TABLE recovery_value (id INTEGER)")
        .unwrap();
    conn.execute("INSERT INTO recovery_value VALUES (?1)", [value])
        .unwrap();
}

fn value(conn: &Connection) -> i64 {
    conn.query_row("SELECT id FROM recovery_value", [], |row| row.get(0))
        .unwrap()
}

fn record(dir: &Path) -> Files {
    let record = Record {
        snapshot_name: "snapshot.sqlite3".into(),
        staged_name: "hub-restore-00000000000000000001.sqlite3.pending".into(),
        damaged_name: "hub-damaged-00000000000000000001.sqlite3".into(),
        had_journal: true,
        migration_digest: None,
        phase: Phase::Installing,
    };
    write_record(&record_path(dir), &record).unwrap();
    record.files(dir).unwrap()
}

fn marker(dir: &Path, snapshot: &str) {
    write_record(
        &marker_path(dir),
        &serde_json::json!({
            "from": 1, "to": 2, "snapshot": snapshot, "started_at": SystemTime::now()
        }),
    )
    .unwrap();
}

fn files(dir: &Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    for directory in [dir.to_path_buf(), dir.join("backups")] {
        if !directory.exists() {
            continue;
        }
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                result.insert(entry.path(), fs::read(entry.path()).unwrap());
            }
        }
    }
    result
}

#[test]
fn startup_installs_staged_copy_after_the_main_file_was_archived() {
    let dir = tempfile::tempdir().unwrap();
    let files = record(dir.path());
    let original = [0xff; 4096];
    fs::write(&files.damaged, original).unwrap();
    fs::write(with_suffix(&files.live, "-wal"), b"unmoved sidecar").unwrap();
    fixture(&files.staged, 42);

    assert!(matches!(
        inspect(dir.path()),
        Some(JournalError::RestoreInterrupted {
            installed: false,
            ..
        })
    ));
    assert!(
        !files.live.exists(),
        "observer inspection must be read-only"
    );
    assert_eq!(value(&open_journal(dir.path()).unwrap()), 42);
    assert_eq!(fs::read(&files.damaged).unwrap(), original);
    assert_eq!(
        fs::read(with_suffix(&files.damaged, "-wal"))
            .unwrap()
            .as_slice(),
        b"unmoved sidecar"
    );
    assert!(!record_path(dir.path()).exists());
    assert!(!files.staged.exists());
}

#[test]
fn startup_after_install_rename_only_cleans_the_restore_record() {
    let dir = tempfile::tempdir().unwrap();
    let files = record(dir.path());
    let mut persisted = read_record(dir.path()).unwrap().unwrap();
    persisted.phase = Phase::Archived;
    write_record(&record_path(dir.path()), &persisted).unwrap();
    fixture(&files.damaged, 17);
    fixture(&files.live, 42);
    let archived = fs::read(&files.damaged).unwrap();
    assert!(matches!(
        inspect(dir.path()),
        Some(JournalError::RestoreInterrupted {
            installed: true,
            ..
        })
    ));
    assert_eq!(value(&open_journal(dir.path()).unwrap()), 42);
    assert_eq!(fs::read(&files.damaged).unwrap(), archived);
    assert!(!record_path(dir.path()).exists());
}

#[test]
fn startup_returns_archived_journal_if_staged_copy_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let files = record(dir.path());
    fixture(&files.damaged, 17);
    let conn = open_journal(dir.path()).unwrap();
    assert_eq!(value(&conn), 17);
    assert!(!record_path(dir.path()).exists());
    assert!(!files.damaged.exists());
}

#[test]
fn unresolved_record_blocks_empty_journal_creation() {
    let dir = tempfile::tempdir().unwrap();
    let files = record(dir.path());
    let error = open_journal(dir.path()).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<JournalError>(),
        Some(JournalError::RestoreInterrupted { .. })
    ));
    assert!(!files.live.exists());
    assert!(record_path(dir.path()).is_file());
}

#[test]
fn quick_check_rejects_corrupt_jobs_page_without_moving_files() {
    let dir = tempfile::tempdir().unwrap();
    drop(lock(dir.path()).unwrap());
    fixture(&dir.path().join(DB_FILE), 1);
    marker(dir.path(), "snapshot.sqlite3");
    let snapshot = dir.path().join("backups/snapshot.sqlite3");
    let conn = Connection::open(&snapshot).unwrap();
    conn.execute_batch("CREATE TABLE jobs (id INTEGER); INSERT INTO jobs VALUES (42)")
        .unwrap();
    let root: i64 = conn
        .query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name='jobs'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let page_size: i64 = conn
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .unwrap();
    drop(conn);
    assert!(root > 1);
    let mut bytes = fs::read(&snapshot).unwrap();
    let offset = usize::try_from(root - 1).unwrap() * usize::try_from(page_size).unwrap();
    bytes[offset] = 0; // The table B-tree page type is invalid.
    fs::write(&snapshot, bytes).unwrap();
    let shallow =
        Connection::open_with_flags(&snapshot, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    shallow.prepare("SELECT name FROM sqlite_schema").unwrap();
    drop(shallow);

    let before = files(dir.path());
    let failure = inspect(dir.path()).unwrap();
    let error =
        restore_snapshot(dir.path(), &snapshot, failure.restore_identity().unwrap()).unwrap_err();
    assert!(
        matches!(error.downcast_ref::<JournalError>(), Some(JournalError::InvalidSnapshot { path, .. }) if *path == snapshot)
    );
    assert_eq!(files(dir.path()), before);
}

#[test]
fn restore_cancels_after_another_process_repairs_the_journal() {
    let dir = tempfile::tempdir().unwrap();
    drop(lock(dir.path()).unwrap());
    let path = dir.path().join(DB_FILE);
    fs::write(&path, [0xff; 256]).unwrap();
    let before_failure = inspect(dir.path()).unwrap();
    assert!(matches!(
        before_failure,
        JournalError::Corrupt { code: 26, .. }
    ));
    fs::remove_file(&path).unwrap();
    fixture(&path, 12);
    let snapshot = dir.path().join("snapshot.sqlite3");
    fixture(&snapshot, 99);
    let before = files(dir.path());
    let error = restore_snapshot(
        dir.path(),
        &snapshot,
        before_failure.restore_identity().unwrap(),
    )
    .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<JournalError>(),
        Some(JournalError::RestoreCancelled)
    ));
    assert_eq!(files(dir.path()), before);
}

#[test]
fn restore_cancels_when_migration_marker_changes() {
    let dir = tempfile::tempdir().unwrap();
    drop(lock(dir.path()).unwrap());
    fixture(&dir.path().join(DB_FILE), 1);
    marker(dir.path(), "first.sqlite3");
    let failure = inspect(dir.path()).unwrap();
    marker(dir.path(), "replacement.sqlite3");
    let before = files(dir.path());
    let error = restore_snapshot(
        dir.path(),
        &dir.path().join("backups/first.sqlite3"),
        failure.restore_identity().unwrap(),
    )
    .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<JournalError>(),
        Some(JournalError::RestoreCancelled)
    ));
    assert_eq!(files(dir.path()), before);
}

#[test]
fn installed_cleanup_failure_only_retries_markers() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join(DB_FILE);
    fixture(&live, 1);
    marker(dir.path(), "snapshot.sqlite3");
    let snapshot = dir.path().join("backups/snapshot.sqlite3");
    fixture(&snapshot, 42);
    let failure = inspect(dir.path()).unwrap();
    let identity = failure.restore_identity().unwrap();
    let outcome = restore_with_cleanup(dir.path(), &snapshot, identity, |_| {
        anyhow::bail!("injected cleanup failure")
    })
    .unwrap();
    assert!(
        matches!(outcome, RestoreOutcome::InstalledMarkerNotRemoved(error) if error.to_string() == "injected cleanup failure")
    );
    assert!(matches!(
        inspect(dir.path()),
        Some(JournalError::RestoreInterrupted {
            installed: true,
            ..
        })
    ));
    let before = files(dir.path());
    assert_eq!(before[&live], before[&snapshot]);
    let err = restore_snapshot(dir.path(), &snapshot, identity).unwrap_err();
    assert!(matches!(
        err.downcast_ref::<JournalError>(),
        Some(JournalError::RestoreInterrupted {
            installed: true,
            ..
        })
    ));
    assert_eq!(files(dir.path()), before);

    assert!(matches!(
        retry_restore_cleanup(dir.path()).unwrap(),
        RestoreOutcome::Installed
    ));
    let after = files(dir.path());
    assert!(before.contains_key(&marker_path(dir.path())));
    assert!(before.contains_key(&record_path(dir.path())));
    let mut expected = before;
    expected.remove(&marker_path(dir.path()));
    expected.remove(&record_path(dir.path()));
    assert_eq!(after, expected);
}
