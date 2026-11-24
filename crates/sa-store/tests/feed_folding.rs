//! Once the lead resumes, the collaborator rounds it has taken in leave the
//! main conversation feed (and stop using up its 180 rows); the lead's own
//! replies, open rounds and questions for the user stay.

mod common;

use common::{Job, create_ticket, log, writer};
use rusqlite::{Connection, params};
use sa_store::{EventView, Store, folded_from_feed};
use serde_json::json;

fn peer(conn: &Connection, lead: Job, agent: &str, state: &str) -> Job {
    conn.execute(
        "INSERT INTO jobs(ticket,parent,sender,agent,body,writing,depth,turn,state) VALUES(?,?,?,?,'work',1,1,1,?)",
        params![lead.ticket, lead.id, "claude", agent, state],
    )
    .unwrap();
    Job { ticket: lead.ticket, id: conn.last_insert_rowid(), turn: 1 }
}

fn has(events: &[EventView], kind: &str, job: i64) -> bool {
    events.iter().any(|e| e.kind == kind && e.job == Some(job))
}

/// Lead turn 1 dispatches; Codex finishes a round with a stage report, a
/// stream of text, a note, a question and its reply; another collaborator is
/// still running; then the lead resumes (turn 2) and streams its overview.
fn journal() -> (tempfile::TempDir, Job, Job, Job, i64) {
    let dir = tempfile::tempdir().unwrap();
    let _studio = Store::open(dir.path()).unwrap();
    let conn = writer(dir.path());
    let lead = create_ticket(&conn, &dir.path().to_string_lossy(), "推进模组", "claude");
    log(&conn, lead, "turn.admitted", &json!({}));
    log(&conn, lead, "job.progress", &json!({"id": "l1", "summary": "已核对检查"}));
    for _ in 0..40 {
        log(&conn, lead, "delta", &json!("lead text "));
    }
    log(&conn, lead, "reply", &json!({"text": "第 1 轮概览：已派录制"}));
    let codex = peer(&conn, lead, "codex", "done");
    let other = peer(&conn, lead, "antigravity", "running");
    log(&conn, codex, "turn.admitted", &json!({}));
    log(&conn, other, "turn.admitted", &json!({}));
    log(&conn, codex, "job.progress", &json!({"id": "p1", "summary": "已录到点按 E"}));
    log(&conn, other, "job.progress", &json!({"id": "o1", "summary": "仍在审阅"}));
    for _ in 0..300 {
        log(&conn, codex, "delta", &json!("codex text "));
    }
    log(&conn, codex, "note", &json!({"job": lead.id, "body": "实例路径不存在"}));
    log(&conn, codex, "ask", &json!({"id": 1, "question": "有备份吗？"}));
    log(&conn, codex, "reply", &json!({"text": "录制完成"}));
    conn.execute("UPDATE jobs SET turn=2,state='running' WHERE id=?", [lead.id]).unwrap();
    let lead2 = Job { turn: 2, ..lead };
    let resumed = log(&conn, lead2, "turn.admitted", &json!({}));
    log(&conn, lead2, "job.progress", &json!({"id": "l2", "summary": "复核录制"}));
    for _ in 0..5 {
        log(&conn, lead2, "delta", &json!("第 2 轮"));
    }
    (dir, lead, codex, other, resumed)
}

#[test]
fn a_resumed_lead_folds_the_finished_collaborator_round_out_of_the_main_feed() {
    let (dir, lead, codex, other, resumed) = journal();
    let mut store = Store::open_read_only(dir.path()).unwrap();
    let snap = store.snapshot(lead.ticket, 0).unwrap();
    assert_eq!(snap.lead_resumed, resumed);
    let events = &snap.events;

    // The lead's replies and the user's message stay; of the lead's progress
    // only the current turn's remains.
    assert!(events.iter().any(|e| e.kind == "user"));
    assert!(has(events, "reply", lead.id));
    let lead_activity: Vec<_> =
        events.iter().filter(|e| e.kind == "activity" && e.job == Some(lead.id)).map(|e| e.turn).collect();
    assert!(!lead_activity.is_empty() && lead_activity.iter().all(|t| *t == Some(2)), "{lead_activity:?}");
    // Only the lead's in-flight turn streams; turn 1 is already its reply.
    let deltas: Vec<_> = events.iter().filter(|e| e.kind == "delta").collect();
    assert_eq!(deltas.len(), 5);
    assert!(deltas.iter().all(|e| e.job == Some(lead.id) && e.turn == Some(2)));

    // The finished Codex round is gone except the question for the user.
    let codex_kinds: Vec<_> = events.iter().filter(|e| e.job == Some(codex.id)).map(|e| e.kind.as_str()).collect();
    assert_eq!(codex_kinds, ["ask"]);

    // A collaborator still at work keeps its progress.
    assert!(has(events, "activity", other.id));
}

#[test]
fn before_the_lead_resumes_the_collaborator_round_is_still_shown() {
    let dir = tempfile::tempdir().unwrap();
    let _studio = Store::open(dir.path()).unwrap();
    let conn = writer(dir.path());
    let lead = create_ticket(&conn, &dir.path().to_string_lossy(), "推进模组", "claude");
    log(&conn, lead, "turn.admitted", &json!({}));
    log(&conn, lead, "reply", &json!({"text": "已派录制"}));
    conn.execute("UPDATE jobs SET state='waiting' WHERE id=?", [lead.id]).unwrap();
    let codex = peer(&conn, lead, "codex", "done");
    log(&conn, codex, "job.progress", &json!({"id": "p1", "summary": "已录到点按 E"}));
    log(&conn, codex, "reply", &json!({"text": "录制完成"}));

    let mut store = Store::open_read_only(dir.path()).unwrap();
    let snap = store.snapshot(lead.ticket, 0).unwrap();
    assert!(has(&snap.events, "reply", codex.id));
    assert!(has(&snap.events, "activity", codex.id));
    assert!(!snap.events.iter().any(|e| e.kind == "delta"));
}

#[test]
fn collaborator_streams_do_not_push_the_lead_out_of_the_180_row_window() {
    let (dir, lead, ..) = journal();
    let conn = writer(dir.path());
    // A long collaborator stream after the lead resumed (an open round).
    let busy = peer(&conn, lead, "codex", "running");
    let busy = Job { ticket: lead.ticket, ..busy };
    for _ in 0..400 {
        log(&conn, busy, "delta", &json!("x"));
    }
    let mut store = Store::open_read_only(dir.path()).unwrap();
    let snap = store.snapshot(lead.ticket, 0).unwrap();
    assert!(has(&snap.events, "reply", lead.id), "lead reply pushed out of the window");
    assert!(!snap.events.iter().any(|e| e.kind == "delta" && e.job == Some(busy.id)));
}

#[test]
fn incremental_loads_never_skip_events_behind_a_truncated_window() {
    let (dir, lead, ..) = journal();
    let mut store = Store::open_read_only(dir.path()).unwrap();
    let full = store.snapshot(lead.ticket, 0).unwrap();
    let conn = writer(dir.path());
    let lead2 = Job { turn: 2, ..lead };
    // More new lead events than one incremental page holds, interleaved with deltas.
    for i in 0..200 {
        log(&conn, lead2, "send", &json!({"to": "codex", "body": format!("任务 {i}")}));
        log(&conn, lead2, "delta", &json!("…"));
    }
    let mut cursor = full.cursor;
    let mut sends = 0;
    let mut last = 0;
    for _ in 0..10 {
        let page = store.snapshot(lead.ticket, cursor).unwrap();
        for e in &page.events {
            assert!(e.seq > last, "events must arrive in order without repeats");
            last = e.seq;
        }
        sends += page.events.iter().filter(|e| e.kind == "send").count();
        if page.cursor == cursor {
            break;
        }
        cursor = page.cursor;
    }
    assert_eq!(sends, 200);
}

#[test]
fn the_folding_rule_keeps_lead_events_decisions_and_open_rounds() {
    let lead = Some(1);
    let fold = |kind, seq, job, turn, round| folded_from_feed(kind, seq, job, turn, lead, 100, round);
    assert!(!fold("reply", 10, Some(1), Some(1), Some(("waiting", 2))), "lead replies stay");
    assert!(fold("activity", 10, Some(1), Some(1), Some(("running", 2))), "earlier lead progress folds");
    assert!(!fold("activity", 110, Some(1), Some(2), Some(("running", 2))), "current lead progress stays");
    assert!(!fold("send", 10, Some(1), Some(1), Some(("running", 2))), "lead dispatches stay");
    assert!(fold("reply", 10, Some(2), Some(1), Some(("done", 1))), "finished round folds");
    assert!(!fold("reply", 110, Some(2), Some(1), Some(("done", 1))), "after the resume it stays");
    assert!(!fold("activity", 10, Some(2), Some(1), Some(("running", 1))), "open round stays");
    assert!(fold("activity", 10, Some(2), Some(1), Some(("running", 2))), "earlier turn of a busy job folds");
    assert!(!fold("activity", 10, Some(2), None, Some(("running", 2))), "unknown turn of an open job stays");
    assert!(fold("note", 10, Some(9), Some(1), None), "job outside the preview counts as ended");
    assert!(!fold("ask", 10, Some(2), Some(1), Some(("done", 1))), "questions stay");
    assert!(fold("delta", 500, Some(2), Some(3), Some(("running", 3))), "collaborator deltas never show");
    assert!(!folded_from_feed("reply", 10, Some(2), Some(1), None, 100, Some(("done", 1))), "no root, no folding");
}
