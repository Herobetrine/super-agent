mod common;

use common::{create_ticket, now, writer};
use rusqlite::{Connection, params};
use sa_store::quota::{Window, upsert};
use sa_store::{Snapshot, Store};

fn quota_table(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE quota_windows(agent TEXT NOT NULL,provider TEXT NOT NULL,kind TEXT NOT NULL,
         used REAL,resets_at REAL,exhausted INTEGER NOT NULL DEFAULT 0,minutes INTEGER,observed_at REAL NOT NULL,
         PRIMARY KEY(agent,provider,kind));",
    )
    .unwrap();
}

fn window(kind: &str, reset: Option<f64>) -> Window {
    Window {
        kind: kind.into(),
        used: Some(0.85),
        resets_at: reset,
        exhausted: false,
        minutes: Some(300),
    }
}

#[test]
fn old_read_only_journal_and_old_snapshot_json_need_no_quota_schema() {
    let dir = tempfile::tempdir().unwrap();
    let _store = Store::open(dir.path()).unwrap();
    let conn = writer(dir.path());
    let root = create_ticket(&conn, "C:/fixture", "legacy quota", "codex");
    let mut reader = Store::open_read_only(dir.path()).unwrap();
    let snap = reader.snapshot(root.ticket, 0).unwrap();
    assert!(snap.quota.is_empty());
    let mut old = serde_json::to_value(&snap).unwrap();
    old.as_object_mut().unwrap().remove("quota");
    old.as_object_mut().unwrap().remove("queued_peers");
    let decoded: Snapshot = serde_json::from_value(old).unwrap();
    assert!(decoded.quota.is_empty());
    assert_eq!(decoded, snap);
    let tables: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE name='quota_windows'", [], |r| r.get(0),
    ).unwrap();
    assert_eq!(tables, 0, "observing a legacy journal must not migrate it");
}

#[test]
fn read_only_snapshot_filters_current_windows_by_all_ticket_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let _store = Store::open(dir.path()).unwrap();
    let conn = writer(dir.path());
    let root = create_ticket(&conn, "C:/fixture", "quota scope", "codex");
    let other = create_ticket(&conn, "C:/other", "unrelated harness", "deepseek");
    conn.execute(
        "INSERT INTO jobs(ticket,parent,sender,agent,body,writing,depth,state)
         VALUES(?,?,'codex','claude','older collaborator',0,1,'done')",
        params![root.ticket, root.id],
    ).unwrap();
    let hidden = conn.last_insert_rowid();
    for _ in 0..90 {
        conn.execute(
            "INSERT INTO jobs(ticket,parent,sender,agent,body,writing,depth,state)
             VALUES(?,?,'codex','codex','newer collaborator',0,1,'done')",
            params![root.ticket, root.id],
        ).unwrap();
    }
    quota_table(&conn);
    let at = now();
    upsert(&conn, "codex", "account-a", &window("primary", Some(at + 3600.0)), at).unwrap();
    upsert(&conn, "codex", "account-b", &window("primary", Some(at + 3600.0)), at).unwrap();
    upsert(&conn, "claude", "account-c", &window("five_hour", Some(at + 3600.0)), at).unwrap();
    upsert(&conn, "deepseek", "other", &window("unknown", Some(at + 3600.0)), at).unwrap();
    upsert(&conn, "codex", "expired", &window("secondary", Some(at - 1.0)), at).unwrap();
    upsert(&conn, "codex", "stale", &window("unknown", None), at - 21601.0).unwrap();
    upsert(&conn, "codex", "recent", &window("unknown", None), at).unwrap();
    let mut reader = Store::open_read_only(dir.path()).unwrap();
    let snap = reader.snapshot(root.ticket, 0).unwrap();
    assert!(!snap.jobs.iter().any(|job| job.id == hidden));
    assert_eq!(snap.quota.len(), 4);
    assert_eq!(snap.quota.iter().map(|row| row.provider.as_str()).collect::<Vec<_>>(),
        vec!["account-c", "account-a", "account-b", "recent"]);
    assert!(snap.quota.iter().any(|row| row.agent == "claude"));
    let unrelated = reader.snapshot(other.ticket, 0).unwrap();
    assert_eq!(unrelated.quota.len(), 1);
    assert_eq!(unrelated.quota[0].agent, "deepseek");
    let stored: i64 = conn.query_row("SELECT count(*) FROM quota_windows", [], |r| r.get(0)).unwrap();
    assert_eq!(stored, 7, "snapshot expiry is a read-only filter");
    let restored: Snapshot = serde_json::from_value(serde_json::to_value(&snap).unwrap()).unwrap();
    assert_eq!(restored.quota, snap.quota);
}

#[test]
fn quota_changes_and_expiry_are_loaded_without_new_journal_events() {
    let dir = tempfile::tempdir().unwrap();
    let _store = Store::open(dir.path()).unwrap();
    let conn = writer(dir.path());
    let root = create_ticket(&conn, "C:/fixture", "quota refresh", "codex");
    quota_table(&conn);
    let at = now();
    let mut limit = window("primary", Some(at + 3600.0));
    upsert(&conn, "codex", "account", &limit, at).unwrap();
    let mut reader = Store::open_read_only(dir.path()).unwrap();
    let first = reader.snapshot(root.ticket, 0).unwrap();
    limit.used = Some(0.95);
    upsert(&conn, "codex", "account", &limit, at).unwrap();
    let changed = reader.snapshot(root.ticket, first.cursor).unwrap();
    assert!(changed.events.is_empty());
    assert_eq!(changed.cursor, first.cursor);
    assert_eq!(changed.quota[0].window.used, Some(0.95));
    assert_ne!(changed.quota, first.quota);
    assert_eq!(reader.snapshot(root.ticket, first.cursor).unwrap().quota, changed.quota);
    limit.resets_at = Some(at - 1.0);
    upsert(&conn, "codex", "account", &limit, at).unwrap();
    let expired = reader.snapshot(root.ticket, first.cursor).unwrap();
    assert!(expired.events.is_empty());
    assert_eq!(expired.cursor, first.cursor);
    assert!(expired.quota.is_empty());
}

#[test]
fn queued_collaborator_count_is_not_limited_to_preview_or_root_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let conn = writer(dir.path());
    let root = create_ticket(&conn, "C:/fixture", "queued slots", "codex");
    for _ in 0..100 {
        conn.execute(
            "INSERT INTO jobs(ticket,parent,sender,agent,body,writing,depth,state)
             VALUES(?,?,'codex','claude','queued collaborator',0,1,'queued')",
            params![root.ticket, root.id],
        ).unwrap();
    }
    conn.execute(
        "INSERT INTO jobs(ticket,parent,sender,agent,body,writing,depth,state)
         VALUES(?,?,'codex','claude','running collaborator',0,1,'running')",
        params![root.ticket, root.id],
    ).unwrap();
    let snap = store.snapshot(root.ticket, 0).unwrap();
    assert_eq!(snap.jobs.len(), 80);
    assert_eq!(snap.queued_peers, 100);
    assert_eq!(snap.running_peers, 1);
    assert_eq!(snap.total_jobs, 102);
    assert_eq!(snap.jobs[0].state, "queued", "the queued root does not count as a collaborator");
}
