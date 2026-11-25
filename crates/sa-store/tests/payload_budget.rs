mod common;

use common::{Job, create_ticket, log, writer};
use rusqlite::params;
use sa_store::Store;
use serde_json::json;

#[test]
fn large_ticket_snapshot_stays_within_its_payload_budget() {
    // Offline reconstruction from the snapshot fields serialized to 1,293,070
    // bytes (including 18 near-cap replies). Budget about 10% above that model.
    const BUDGET_BYTES: usize = 1_423_000;
    const LEAD_TURNS: i64 = 260;
    const LONG_REPLY_CHARS: usize = 63_900;

    let dir = tempfile::tempdir().unwrap();
    drop(Store::open(dir.path()).unwrap());
    let conn = writer(dir.path());
    let lead = create_ticket(
        &conn,
        "C:/synthetic-project",
        "Check a long ticket",
        "claude",
    );
    let long_reply = json!({"text": "x".repeat(LONG_REPLY_CHARS)});
    let native_frame = json!({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": "read", "name": "Read", "input": {"path": "fixture.rs"}}
    ]}});
    let peer_body = "Investigate the fixture. ".repeat(20);
    let peer_result = "Checked the fixture. ".repeat(20);

    conn.execute_batch("BEGIN").unwrap();
    for turn in 1..=LEAD_TURNS {
        let lead = Job { turn, ..lead };
        log(&conn, lead, "turn.admitted", &json!({}));
        log(&conn, lead, "native", &native_frame);
        log(&conn, lead, "delta", &json!("Working on the ticket"));
        if turn % 10 == 0 {
            log(&conn, lead, "reply", &long_reply);
        } else {
            log(
                &conn,
                lead,
                "reply",
                &json!({"text": format!("Lead round {turn} closed.")}),
            );
        }
        if turn % 2 == 0 {
            conn.execute(
                "INSERT INTO jobs(ticket,parent,sender,agent,body,result,writing,depth,turn,state)
                 VALUES(?,?,'claude','claude',?,?,0,1,1,'done')",
                params![lead.ticket, lead.id, peer_body, peer_result],
            )
            .unwrap();
            let peer = Job {
                id: conn.last_insert_rowid(),
                turn: 1,
                ..lead
            };
            log(&conn, peer, "turn.admitted", &json!({}));
            log(&conn, peer, "native", &native_frame);
            log(&conn, peer, "reply", &json!({"text": peer_result}));
        }
    }
    let lead = Job {
        turn: LEAD_TURNS + 1,
        ..lead
    };
    conn.execute(
        "UPDATE jobs SET turn=?,state='running' WHERE id=?",
        params![lead.turn, lead.id],
    )
    .unwrap();
    log(&conn, lead, "turn.admitted", &json!({}));
    log(&conn, lead, "native", &native_frame);
    log(
        &conn,
        lead,
        "delta",
        &json!("Current turn is still streaming"),
    );
    conn.execute_batch("COMMIT").unwrap();

    let mut store = Store::open_read_only(dir.path()).unwrap();
    let snapshot = store.snapshot(lead.ticket, 0).unwrap();
    let bytes = serde_json::to_vec(&snapshot).unwrap().len();
    eprintln!("large ticket snapshot: {bytes} bytes (budget {BUDGET_BYTES})");
    let non_delta = snapshot
        .events
        .iter()
        .filter(|event| event.kind != "delta")
        .count();
    assert!(
        non_delta <= 180,
        "snapshot has {non_delta} non-delta events, budget 180"
    );
    assert!(
        snapshot.jobs.len() <= 80,
        "snapshot has {} jobs, budget 80",
        snapshot.jobs.len()
    );
    assert!(
        bytes <= BUDGET_BYTES,
        "snapshot measured {bytes} JSON bytes, budget {BUDGET_BYTES}"
    );
    assert!(
        snapshot.total_jobs > 80,
        "fixture must exceed the job preview limit"
    );
    assert!(
        snapshot
            .events
            .iter()
            .any(|event| event.kind == "reply" && event.payload == long_reply),
        "fixture must expose long replies at the snapshot boundary"
    );
}
