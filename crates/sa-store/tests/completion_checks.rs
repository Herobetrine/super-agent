mod common;

use common::{create_ticket, log, writer};
use rusqlite::params;
use sa_store::Store;
use serde_json::json;

#[test]
fn checks_refresh_in_both_job_views_and_clear_without_events() {
    let dir = tempfile::tempdir().unwrap();
    let _store = Store::open(dir.path()).unwrap();
    let conn = writer(dir.path());
    let root = create_ticket(&conn, "C:/fixture", "completion checks", "codex");
    let other = create_ticket(&conn, "C:/other", "unrelated", "claude");
    let mut reader = Store::open_read_only(dir.path()).unwrap();
    let before = reader.snapshot(root.ticket, 0).unwrap();
    assert_eq!(before.jobs[0].checks, None);
    // The writer may add the table after a read-only observer starts.
    conn.execute_batch(
        "CREATE TABLE job_checks(job INTEGER PRIMARY KEY,turn INTEGER NOT NULL,
         attempt INTEGER NOT NULL,payload TEXT NOT NULL);",
    ).unwrap();
    for (ok, cached, attempt) in [(false, false, 1), (true, false, 2), (true, true, 1)] {
        let payload = json!({"ok":ok,"cached":cached,"attempt":attempt,"runs":[{
            "argv":["fixture","--check"],"ok":ok,"exit":if ok {0} else {1},"seconds":0.125
        }]});
        conn.execute(
            "INSERT OR REPLACE INTO job_checks VALUES(?,1,?,?)",
            params![root.id, attempt, payload.to_string()],
        ).unwrap();
        // A new admission does not erase the most recent reply's check evidence.
        conn.execute("UPDATE jobs SET turn=2 WHERE id=?", [root.id]).unwrap();
        let snap = reader.snapshot(root.ticket, before.cursor).unwrap();
        assert_eq!(snap.jobs[0].checks, Some(payload));
        assert!(snap.events.is_empty());
        assert_eq!(reader.jobs_page(root.ticket, None).unwrap().jobs, snap.jobs);
        assert_eq!(reader.snapshot(other.ticket, 0).unwrap().jobs[0].checks, None);
    }
    conn.execute("DELETE FROM job_checks WHERE job=?", [root.id]).unwrap();
    assert_eq!(reader.snapshot(root.ticket, before.cursor).unwrap().jobs[0].checks, None);
    assert_eq!(reader.jobs_page(root.ticket, None).unwrap().jobs[0].checks, None);
}

#[test]
fn check_events_are_visible_and_activity_keeps_only_operational_labels() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let conn = writer(dir.path());
    let root = create_ticket(&conn, "C:/fixture", "check progress", "codex");
    for (kind, payload) in [
        ("check.started", json!({"argv":["PRIVATE"]})),
        ("check.result", json!({"argv":["PRIVATE"],"ok":true,"tail":"PRIVATE"})),
        ("check.result", json!({"argv":["PRIVATE"],"ok":false,"tail":"PRIVATE"})),
        ("check.bounced", json!({"attempt":1})),
        ("auto.stalled", json!({"stalls":3,"signal":"PRIVATE"})),
    ] {
        log(&conn, root, kind, &payload);
    }
    let snap = store.snapshot(root.ticket, 0).unwrap();
    for (kind, count) in [("check.started",1), ("check.result",2), ("check.bounced",1), ("auto.stalled",1)] {
        assert_eq!(snap.events.iter().filter(|event| event.kind == kind).count(), count);
    }
    let page = store.activity_page(root.ticket, None).unwrap();
    assert_eq!(page.events.len(), 6); // Initial user event plus five gate events.
    assert!(!serde_json::to_string(&page.events).unwrap().contains("PRIVATE"));
    for (seq, state) in [(2,"running"), (3,"completed"), (4,"failed"), (5,"running"), (6,"waiting")] {
        let event = page.events.iter().find(|event| event.seq == seq).unwrap();
        assert_eq!(event.payload["state"], state);
    }
}
