mod common;

use common::{create_ticket, log, writer};
use rusqlite::{Connection, params};
use sa_store::{Snapshot, Store, TicketSummary};
use serde_json::json;

fn tables(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE job_outcomes(job INTEGER PRIMARY KEY,turn INTEGER NOT NULL,payload TEXT NOT NULL);
         CREATE TABLE job_deps(job INTEGER NOT NULL,needs INTEGER NOT NULL,PRIMARY KEY(job,needs));
         CREATE TABLE questions(id INTEGER PRIMARY KEY,ticket INTEGER NOT NULL,job INTEGER NOT NULL,
         turn INTEGER NOT NULL,msgkey TEXT NOT NULL,question TEXT NOT NULL,options TEXT NOT NULL DEFAULT '[]',
         fallback TEXT,origin TEXT NOT NULL DEFAULT 'agent',state TEXT NOT NULL DEFAULT 'pending',
         parked INTEGER NOT NULL DEFAULT 0,answer TEXT,asked REAL NOT NULL,closed REAL);",
    ).unwrap();
}

#[test]
fn old_read_only_journals_and_serialized_views_need_no_collaboration_schema() {
    let dir = tempfile::tempdir().unwrap();
    let _store = Store::open(dir.path()).unwrap();
    let conn = writer(dir.path());
    let root = create_ticket(&conn, "C:/fixture", "legacy collaboration", "codex");
    let mut reader = Store::open_read_only(dir.path()).unwrap();
    let snap = reader.snapshot(root.ticket, 0).unwrap();
    assert!(snap.questions.is_empty());
    assert_eq!(snap.ticket.attention, None);
    assert_eq!(snap.jobs[0].outcome, None);
    assert_eq!(snap.jobs[0].checks, None);
    assert!(snap.jobs[0].needs.is_empty());
    assert_eq!(reader.jobs_page(root.ticket, None).unwrap().jobs, snap.jobs);

    let mut old = serde_json::to_value(&snap).unwrap();
    old.as_object_mut().unwrap().remove("questions");
    old["ticket"].as_object_mut().unwrap().remove("attention");
    for job in old["jobs"].as_array_mut().unwrap() {
        job.as_object_mut().unwrap().remove("outcome");
        job.as_object_mut().unwrap().remove("checks");
        job.as_object_mut().unwrap().remove("needs");
    }
    assert_eq!(serde_json::from_value::<Snapshot>(old).unwrap(), snap);
    let summary = reader.history("").unwrap().remove(0);
    let mut old_summary = serde_json::to_value(&summary).unwrap();
    old_summary.as_object_mut().unwrap().remove("attention");
    assert_eq!(serde_json::from_value::<TicketSummary>(old_summary).unwrap(), summary);
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE name IN ('questions','job_outcomes','job_deps','job_checks','project_checks','check_passes','check_bounces')",
        [], |r| r.get(0),
    ).unwrap();
    assert_eq!(count, 0, "observing an old journal must not migrate it");
}

#[test]
fn latest_outcome_dependencies_and_pending_questions_refresh_without_events() {
    let dir = tempfile::tempdir().unwrap();
    let _store = Store::open(dir.path()).unwrap();
    let conn = writer(dir.path());
    let root = create_ticket(&conn, "C:/fixture", "collaboration projection", "codex");
    let other = create_ticket(&conn, "C:/other", "other ticket", "claude");
    let mut reader = Store::open_read_only(dir.path()).unwrap();
    let before = reader.snapshot(root.ticket, 0).unwrap();
    // Tables may appear after the observer starts, without a journal event.
    tables(&conn);
    let outcome = json!({"status":"partial","summary":"还需复核","verified":["静态检查"]});
    conn.execute("INSERT INTO job_outcomes VALUES(?,1,?)", params![root.id, outcome.to_string()]).unwrap();
    // Admission may increment the turn before another reply exists.
    conn.execute("UPDATE jobs SET turn=2 WHERE id=?", [root.id]).unwrap();
    conn.execute("INSERT INTO job_deps VALUES(?,?)", params![root.id, other.id]).unwrap();
    for (job, key, state, origin) in [
        (root, "scope", "pending", "agent"),
        (root, "old", "answered", "agent"),
        (root, "withdrawn", "withdrawn", "agent"),
        (other, "unrelated", "pending", "platform"),
    ] {
        conn.execute(
            "INSERT INTO questions(ticket,job,turn,msgkey,question,options,fallback,origin,state,asked)
             VALUES(?,?,1,?,'选择范围',?,'保持范围',?,?,12.5)",
            params![job.ticket, job.id, key, json!(["保持", "扩展"]).to_string(), origin, state],
        ).unwrap();
    }
    let snap = reader.snapshot(root.ticket, before.cursor).unwrap();
    assert!(snap.events.is_empty());
    assert_eq!(snap.cursor, before.cursor);
    assert_eq!(snap.jobs[0].turn, 2);
    assert_eq!(snap.jobs[0].outcome, Some(outcome));
    assert_eq!(snap.jobs[0].needs, vec![other.id]);
    assert_eq!(reader.jobs_page(root.ticket, None).unwrap().jobs, snap.jobs);
    assert_eq!(snap.questions.len(), 1);
    let question = &snap.questions[0];
    assert_eq!(question.job, root.id);
    assert_eq!(question.key, "scope");
    assert_eq!(question.question, "选择范围");
    assert_eq!(question.options, vec!["保持", "扩展"]);
    assert_eq!(question.fallback.as_deref(), Some("保持范围"));
    assert_eq!(question.origin, "agent");
    assert_eq!(question.asked, 12.5);
    assert_eq!(reader.snapshot(other.ticket, 0).unwrap().questions[0].origin, "platform");
    conn.execute("UPDATE questions SET state='answered' WHERE id=?", [question.id]).unwrap();
    assert!(reader.snapshot(root.ticket, snap.cursor).unwrap().questions.is_empty());
}

#[test]
fn attention_has_identical_priority_in_history_and_snapshot_with_or_without_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let _store = Store::open(dir.path()).unwrap();
    let conn = writer(dir.path());
    let root = create_ticket(&conn, "C:/fixture", "attention", "codex");
    let mut reader = Store::open_read_only(dir.path()).unwrap();
    tables(&conn);
    for meta in [false, true] {
        if meta {
            conn.execute_batch(sa_store::TICKET_META).unwrap();
        }
        for (state, question, approval, expected) in [
            ("running", false, false, None),
            ("done", false, false, None),
            ("partial", false, false, Some("partial")),
            ("failed", false, false, Some("failed")),
            ("failed", true, false, Some("question")),
            ("failed", true, true, Some("approval")),
        ] {
            conn.execute("UPDATE jobs SET state=? WHERE id=?", params![state, root.id]).unwrap();
            conn.execute("DELETE FROM questions", []).unwrap();
            conn.execute("DELETE FROM approvals", []).unwrap();
            if question {
                conn.execute(
                    "INSERT INTO questions(ticket,job,turn,msgkey,question,asked) VALUES(?,?,1,'scope','范围？',1)",
                    params![root.ticket, root.id],
                ).unwrap();
            }
            if approval {
                conn.execute("INSERT INTO approvals(ticket,job,turn,action) VALUES(?,?,1,'{}')", params![root.ticket, root.id]).unwrap();
            }
            assert_eq!(reader.history("").unwrap()[0].attention.as_deref(), expected);
            assert_eq!(reader.snapshot(root.ticket, 0).unwrap().ticket.attention.as_deref(), expected);
        }
    }
}

#[test]
fn protocol_events_are_visible_once_and_have_sanitized_activity_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let conn = writer(dir.path());
    let root = create_ticket(&conn, "C:/fixture", "protocol events", "codex");
    let kinds = ["ask", "answer", "ask.withdrawn", "report", "continued", "skipped"];
    for kind in kinds {
        log(&conn, root, kind, &json!({"status":"blocked","question":"PRIVATE","summary":"PRIVATE"}));
    }
    let snap = store.snapshot(root.ticket, 0).unwrap();
    for kind in kinds {
        assert_eq!(snap.events.iter().filter(|event| event.kind == kind).count(), 1);
    }
    let page = store.activity_page(root.ticket, None).unwrap();
    assert_eq!(page.events.len(), 7); // Includes the initial user event.
    assert!(!serde_json::to_string(&page.events).unwrap().contains("PRIVATE"));
    let report = page.events.iter().find(|event| event.payload["label"] == "任务受阻，等待处理").unwrap();
    assert_eq!(report.payload["state"], "stopped");
}
