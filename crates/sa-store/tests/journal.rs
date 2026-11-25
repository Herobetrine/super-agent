use rusqlite::Connection;
use sa_store::{
    DB_FILE,
    journal::{JournalError, open_journal, restore_snapshot},
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
    assert_eq!(pragma(&conn, "user_version"), 1);
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
        assert_eq!(pragma(&conn, "user_version"), 1);
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
    let error = open_journal(dir.path()).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<JournalError>(),
        Some(JournalError::Corrupt { path: reported, code: 26, latest_snapshot: None })
            if *reported == path
    ));
    assert_eq!(fs::read(&path).unwrap(), garbage);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn invalid_restore_snapshot_keeps_journal_and_cleans_pending_file() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(DB_FILE);
    drop(open_journal(dir.path()).unwrap());
    let original = fs::read(&path).unwrap();
    let snapshot = dir.path().join("invalid-snapshot.sqlite3");
    fs::write(&snapshot, [0xcd; 4096]).unwrap();

    let error = restore_snapshot(dir.path(), &snapshot).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(failure, _))
            if failure.extended_code == rusqlite::ffi::SQLITE_NOTADB
    ));
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(fs::read_dir(dir.path()).unwrap().all(|entry| {
        entry
            .unwrap()
            .path()
            .extension()
            .is_none_or(|ext| ext != "pending")
    }));
}
