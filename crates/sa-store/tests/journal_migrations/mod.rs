use super::{JournalError, Migration, backup, inspect, open_journal, open_with_migrations};
use crate::DB_FILE;
use anyhow::Result;
use rusqlite::Connection;
use std::{fs, path::Path};

fn migration(to: u32, apply: fn(&Connection) -> Result<()>) -> Migration {
    Migration {
        to,
        needs_snapshot: true,
        apply,
    }
}

fn increment(conn: &Connection) -> Result<()> {
    conn.execute_batch("UPDATE recovery_fixture SET value = value + 1")?;
    Ok(())
}

fn fixture(dir: &Path) -> Connection {
    let conn = open_journal(dir).unwrap();
    conn.execute_batch(
        "PRAGMA wal_autocheckpoint=0;
         CREATE TABLE recovery_fixture(value INTEGER);
         INSERT INTO recovery_fixture VALUES(42);",
    )
    .unwrap();
    conn
}

fn value(conn: &Connection) -> i64 {
    conn.query_row("SELECT value FROM recovery_fixture", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn snapshot_migrations_keep_only_the_last_two_pre_step_states() {
    let dir = tempfile::tempdir().unwrap();
    let writer = fixture(dir.path());
    let steps = [
        migration(2, increment),
        migration(3, increment),
        migration(4, increment),
    ];
    let conn = open_with_migrations(dir.path(), &steps).unwrap();
    assert_eq!(value(&conn), 45);
    let files = fs::read_dir(dir.path().join("backups"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 2);
    let mut values = files
        .iter()
        .map(|path| {
            let snapshot =
                Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .unwrap();
            value(&snapshot)
        })
        .collect::<Vec<_>>();
    values.sort_unstable();
    assert_eq!(values, [43, 44]);
    drop(writer);
}

#[test]
fn failed_step_rolls_back_and_leaves_the_pre_step_snapshot_and_marker() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DB_FILE);
    drop(fixture(dir.path()));
    let error = open_with_migrations(
        dir.path(),
        &[migration(2, |conn| {
            increment(conn)?;
            conn.execute_batch("ALTER TABLE missing_table ADD COLUMN absent TEXT")?;
            Ok(())
        })],
    )
    .unwrap_err();
    assert!(error.downcast_ref::<rusqlite::Error>().is_some());
    let marker = backup::marker_path(dir.path());
    let marker_bytes = fs::read(&marker).unwrap();
    let error = open_journal(dir.path()).unwrap_err();
    let Some(JournalError::MigrationInterrupted {
        snapshot,
        from: 1,
        to: 2,
        ..
    }) = error.downcast_ref::<JournalError>()
    else {
        panic!("{error:#}");
    };

    let raw = Connection::open(&path).unwrap();
    assert_eq!(value(&raw), 42);
    assert_eq!(
        raw.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        1
    );
    let saved =
        Connection::open_with_flags(snapshot, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(value(&saved), 42);
    assert_eq!(
        saved
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        1
    );
    assert_eq!(fs::read(&marker).unwrap(), marker_bytes);
}

#[test]
fn marker_removal_syncs_backups_after_removing_the_active_name() {
    let dir = tempfile::tempdir().unwrap();
    let marker = backup::marker_path(dir.path());
    let backups = marker.parent().unwrap();
    fs::create_dir(backups).unwrap();
    fs::write(&marker, b"marker").unwrap();
    let error = backup::remove_marker_with_sync(dir.path(), |directory| {
        assert_eq!(directory, backups);
        assert!(!marker.exists());
        assert_eq!(fs::read_dir(directory).unwrap().count(), 0);
        Err(std::io::Error::other("injected directory sync failure"))
    })
    .unwrap_err();
    assert_eq!(
        error.root_cause().to_string(),
        "injected directory sync failure"
    );
}

#[test]
fn next_snapshot_migration_cleans_only_private_pending_snapshot_files() {
    let dir = tempfile::tempdir().unwrap();
    drop(fixture(dir.path()));
    let backups = dir.path().join("backups");
    fs::create_dir(&backups).unwrap();
    let stale = backups.join("hub-00000000000000000001.sqlite3.pending");
    let unrelated = backups.join("hub-not-a-timestamp.sqlite3.pending");
    let marker_temp = backups.join("migration.json-00000000000000000001.pending");
    let directory = backups.join("hub-00000000000000000002.sqlite3.pending");
    for path in [&stale, &unrelated, &marker_temp] {
        fs::write(path, b"leftover").unwrap();
    }
    fs::create_dir(&directory).unwrap();

    drop(open_journal(dir.path()).unwrap());
    assert!(
        stale.is_file(),
        "opening a current journal does not clean backups"
    );
    let conn = open_with_migrations(dir.path(), &[migration(2, increment)]).unwrap();
    assert!(!stale.exists());
    assert_eq!(fs::read(unrelated).unwrap().as_slice(), b"leftover");
    assert_eq!(fs::read(marker_temp).unwrap().as_slice(), b"leftover");
    assert!(directory.is_dir());
    assert_eq!(value(&conn), 43);
    assert_eq!(
        value(&Connection::open(backup::latest_snapshot(dir.path()).unwrap().0).unwrap()),
        42
    );
}

#[test]
fn backup_failure_drops_an_unpublished_partial_file() {
    let dir = tempfile::tempdir().unwrap();
    let snapshot = dir.path().join("hub-00000000000000000001.sqlite3");
    let error = backup::publish_snapshot(&snapshot, |pending| {
        fs::write(pending, b"partial snapshot")?;
        anyhow::bail!("injected backup failure");
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "injected backup failure");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn relocated_migration_marker_resolves_new_and_legacy_snapshot_names() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("original");
    drop(fixture(&dir));
    let failing = migration(2, |_| anyhow::bail!("stop after marker"));
    open_with_migrations(&dir, &[failing]).unwrap_err();
    let marker = backup::marker_path(&dir);
    let bytes = fs::read(&marker).unwrap();
    let mut payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let name = payload["snapshot"].as_str().unwrap().to_owned();
    assert_eq!(Path::new(&name).file_name().unwrap(), name.as_str());
    let moved = root.path().join("moved");
    fs::rename(&dir, &moved).unwrap();
    for snapshot in [name.clone().into(), dir.join("backups").join(&name)] {
        payload["snapshot"] = serde_json::to_value(snapshot).unwrap();
        fs::write(
            backup::marker_path(&moved),
            serde_json::to_vec(&payload).unwrap(),
        )
        .unwrap();
        let error = inspect(&moved).unwrap();
        assert!(matches!(
            &error,
            JournalError::MigrationInterrupted { snapshot, from: 1, to: 2, .. }
                if *snapshot == moved.join("backups").join(&name) && snapshot.is_file()
        ));
    }
}

#[test]
fn corruption_reports_the_latest_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    drop(fixture(dir.path()));
    let steps = [migration(2, increment), migration(3, increment)];
    drop(open_with_migrations(dir.path(), &steps).unwrap());
    let latest = backup::latest_snapshot(dir.path()).unwrap();
    fs::write(dir.path().join(DB_FILE), [0xff; 4096]).unwrap();
    let error = open_journal(dir.path()).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<JournalError>(),
        Some(JournalError::Corrupt { code: 26, latest_snapshot: Some(reported), .. })
            if *reported == latest
    ));
}
