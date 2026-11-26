use rusqlite::Connection;
use sa_store::{
    DB_FILE,
    journal::{JournalError, inspect, open_journal},
};
use std::fs;
use tempfile::tempdir;

fn pragma(conn: &Connection, name: &str) -> i64 {
    conn.pragma_query_value(None, name, |row| row.get(0))
        .unwrap()
}

#[test]
fn new_journal_sets_writer_pragmas_and_generation() {
    let dir = tempdir().unwrap();
    let conn = open_journal(dir.path()).unwrap();
    assert_eq!(pragma(&conn, "journal_size_limit"), 134_217_728);
    assert_eq!(pragma(&conn, "synchronous"), 2);
    assert_eq!(pragma(&conn, "auto_vacuum"), 2);
    assert_eq!(pragma(&conn, "user_version"), 3);
    drop(conn);
    assert_eq!(pragma(&open_journal(dir.path()).unwrap(), "auto_vacuum"), 2);
}

#[test]
fn zero_length_file_is_initialized_with_incremental_vacuum() {
    let dir = tempdir().unwrap();
    fs::File::create_new(dir.path().join(DB_FILE)).unwrap();
    let conn = open_journal(dir.path()).unwrap();
    assert_eq!(pragma(&conn, "auto_vacuum"), 2);
}

#[test]
fn legacy_journal_keeps_its_vacuum_mode_and_rows() {
    for (mode, expected) in [("NONE", 0), ("FULL", 1)] {
        let dir = tempdir().unwrap();
        let conn = Connection::open(dir.path().join(DB_FILE)).unwrap();
        conn.pragma_update(None, "auto_vacuum", mode).unwrap();
        conn.execute_batch("CREATE TABLE legacy(id); INSERT INTO legacy VALUES (42);")
            .unwrap();
        drop(conn);
        let conn = open_journal(dir.path()).unwrap();
        assert_eq!(pragma(&conn, "auto_vacuum"), expected);
        assert_eq!(pragma(&conn, "user_version"), 3);
        let id: i64 = conn
            .query_row("SELECT id FROM legacy", [], |row| row.get(0))
            .unwrap();
        assert_eq!(id, 42);
    }
}

#[test]
fn unrecognized_file_is_classified_without_changing_its_bytes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(DB_FILE);
    let garbage = vec![0xcd; 4096];
    fs::write(&path, &garbage).unwrap();
    assert!(matches!(
        inspect(dir.path()),
        Some(JournalError::Corrupt { code: 26, .. })
    ));
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    let error = open_journal(dir.path()).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<JournalError>(),
        Some(JournalError::Corrupt { path: reported, code: 26, latest_snapshot: None, .. })
            if *reported == path
    ));
    assert_eq!(fs::read(&path).unwrap(), garbage);
    let mut files: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    files.sort_unstable();
    assert_eq!(
        files,
        [".journal-migration.lock", DB_FILE].map(std::ffi::OsString::from)
    );
}

#[test]
fn observer_probe_reports_corrupt_header_with_snapshot() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(DB_FILE);
    let snapshot = dir.path().join("backups/hub-00000000000000000001.sqlite3");
    fs::create_dir(snapshot.parent().unwrap()).unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE jobs (id INTEGER); INSERT INTO jobs VALUES (42)")
        .unwrap();
    drop(conn);
    fs::copy(&path, &snapshot).unwrap();
    let mut bytes = fs::read(&path).unwrap();
    // Report more pages in the header than the file actually contains.
    bytes[28..32].copy_from_slice(&200_u32.to_be_bytes());
    fs::write(&path, &bytes).unwrap();
    assert!(matches!(
        inspect(dir.path()),
        Some(JournalError::Corrupt { code: 11, latest_snapshot: Some((found, _)), .. })
            if found == snapshot
    ));
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[test]
fn observer_probe_leaves_healthy_wal_journal_and_sidecars_untouched() {
    let dir = tempdir().unwrap();
    let conn = open_journal(dir.path()).unwrap();
    conn.execute_batch("CREATE TABLE observer_fixture (id)")
        .unwrap();
    let files = || {
        fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), fs::read(entry.path()).unwrap())
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let before = files();
    assert!(inspect(dir.path()).is_none());
    assert_eq!(files(), before);
    drop(conn);
}
