//! A long ticket history is replayed within a time budget: the first view
//! carries the visible events without waiting for the activity projection,
//! and repeated budgeted loads end in the same snapshot as a full load.

mod common;

use common::{create_ticket, log, writer};
use sa_store::Store;
use serde_json::{Value, json};
use std::time::Duration;

fn tool(n: usize) -> Value {
    json!({"type":"assistant","message":{"content":[
        {"type":"tool_use","id":format!("u-{n}"),"name":"Read","input":{"path":"p"}}]}})
}

fn usage(n: u64) -> Value {
    json!({"type":"assistant","message":{"content":[],
        "usage":{"input_tokens":n,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":1}}})
}

/// A Claude ticket with `rows` native frames, a few replies and a task card.
fn journal(rows: usize) -> (tempfile::TempDir, i64) {
    let dir = tempfile::tempdir().unwrap();
    let _studio = Store::open(dir.path()).unwrap();
    let conn = writer(dir.path());
    let job = create_ticket(&conn, &dir.path().to_string_lossy(), "long history", "claude");
    conn.execute_batch("BEGIN").unwrap();
    log(
        &conn,
        job,
        "native",
        &json!({"type":"system","subtype":"task_started","task_type":"local_agent","task_id":"sub1","tool_use_id":"parent"}),
    );
    for i in 0..rows {
        let frame = if i % 3 == 0 { usage(10) } else { tool(i) };
        log(&conn, job, "native", &frame);
        if i % 500 == 0 {
            log(&conn, job, "reply", &json!({"text": format!("reply {i}")}));
        }
        log(&conn, job, "delta", &json!("text"));
    }
    log(&conn, job, "reply", &json!({"text": "last reply"}));
    conn.execute_batch("COMMIT").unwrap();
    (dir, job.ticket)
}

#[test]
fn budgeted_loads_show_visible_events_first_and_converge_to_the_full_snapshot() {
    let (dir, ticket) = journal(3000);
    let mut full = Store::open_read_only(dir.path()).unwrap();
    let mut expected = full.snapshot(ticket, 0).unwrap();
    while expected.usage.pending {
        expected = full.snapshot(ticket, 0).unwrap();
    }
    assert!(!expected.loading);
    assert!(expected.events.iter().any(|e| e.kind == "activity"));
    assert_eq!(expected.native_activity.tasks.len(), 1);

    let mut reader = Store::open_read_only(dir.path()).unwrap();
    let first = reader.snapshot_within(ticket, 0, Duration::ZERO).unwrap();
    assert!(first.loading, "a zero budget cannot replay 9000 rows in one batch");
    assert!(first.events.iter().all(|e| e.kind != "activity"));
    assert!(first.native_activity.tasks.is_empty());
    assert_eq!(
        first.events.last().map(|e| e.payload["text"].clone()),
        Some(json!("last reply")),
        "the newest visible events are shown while the replay runs"
    );
    assert_eq!(first.cursor, expected.cursor);

    let mut rounds = 1;
    let mut snap = first;
    while snap.loading || snap.usage.pending {
        snap = reader.snapshot_within(ticket, 0, Duration::ZERO).unwrap();
        rounds += 1;
        assert!(rounds < 100, "every budgeted load must make progress");
    }
    assert!(rounds > 2);
    assert_eq!(snap, expected);
}

#[test]
fn a_budgeted_load_finishes_the_usage_scan_instead_of_one_batch_per_poll() {
    let (dir, ticket) = journal(1200);
    let mut plain = Store::open_read_only(dir.path()).unwrap();
    let mut expected = plain.snapshot(ticket, 0).unwrap();
    assert!(expected.usage.pending, "plain snapshots advance usage by one batch");
    while expected.usage.pending {
        expected = plain.snapshot(ticket, 0).unwrap();
    }

    let mut reader = Store::open_read_only(dir.path()).unwrap();
    let snap = reader
        .snapshot_within(ticket, 0, Duration::from_secs(30))
        .unwrap();
    assert!(!snap.loading);
    assert!(!snap.usage.pending);
    assert_eq!(snap.usage, expected.usage);
}
