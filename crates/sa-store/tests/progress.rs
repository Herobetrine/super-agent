//! Port of `tests-node/progress.test.cjs`, plus snapshot projection checks.

mod common;

use common::{create_ticket, log, writer};
use rusqlite::params;
use sa_store::Store;
use serde_json::{Value, json};

fn tool(n: usize) -> Value {
    json!({"type":"assistant","message":{"content":[
        {"type":"tool_use","id":format!("u-{n}"),"name":"Read","input":{"path":"HIDDEN-PATH"}}]}})
}

#[test]
fn activity_pages_backfill_prior_turns_and_merge_live_receipts_without_gaps() {
    let dir = tempfile::tempdir().unwrap();
    let _studio = Store::open(dir.path()).unwrap();
    let conn = writer(dir.path());
    let job = create_ticket(
        &conn,
        &dir.path().to_string_lossy(),
        "investigate",
        "claude",
    );
    let ticket = job.ticket;
    // A second, read-only observer on the same journal.
    let mut reader = Store::open_read_only(dir.path()).unwrap();
    conn.execute("UPDATE jobs SET state='running' WHERE id=?", [job.id])
        .unwrap();
    log(
        &conn,
        job,
        "native",
        &json!({"type":"system","subtype":"task_started","task_type":"local_agent","task_id":"sub1","tool_use_id":"parent"}),
    );
    for i in 0..210 {
        log(&conn, job, "native", &tool(i));
    }
    let original = reader.snapshot(ticket, 0).unwrap();
    assert_eq!(original.native_activity.tasks[0].state, "running");
    assert!(
        !serde_json::to_string(&original.native_activity)
            .unwrap()
            .contains("HIDDEN-PATH")
    );

    let mut seqs = Vec::new();
    let mut before = None;
    loop {
        let page = reader.activity_page(ticket, before).unwrap();
        assert!(
            page.events
                .iter()
                .all(|e| e.agent.as_deref() == Some("claude")
                    && e.job == Some(job.id)
                    && e.turn == Some(1)
                    && e.kind == "activity")
        );
        assert!(
            !serde_json::to_string(&page.events)
                .unwrap()
                .contains("HIDDEN-PATH")
        );
        seqs.extend(page.events.iter().map(|e| e.seq));
        if !page.more {
            break;
        }
        before = page.next;
    }
    assert_eq!(seqs.len(), 212);
    let unique: std::collections::HashSet<_> = seqs.iter().collect();
    assert_eq!(unique.len(), 212);

    let mut expected = Vec::new();
    for i in 0..240 {
        expected.push(if i % 2 == 1 {
            log(&conn, job, "native", &tool(i))
        } else {
            log(&conn, job, "delta", &json!("text"))
        });
    }
    let first = reader.snapshot(ticket, original.cursor).unwrap();
    let second = reader.snapshot(ticket, first.cursor).unwrap();
    assert_eq!(first.events.len(), 180);
    let got: Vec<i64> = first
        .events
        .iter()
        .chain(&second.events)
        .map(|e| e.seq)
        .collect();
    assert_eq!(got, expected);

    conn.execute("UPDATE jobs SET state='done' WHERE id=?", [job.id])
        .unwrap();
    assert_eq!(
        reader.snapshot(ticket, 0).unwrap().native_activity.tasks[0].state,
        "unknown"
    );

    // studio.planAdd({ticket, body: 'review'})
    conn.execute(
        "INSERT INTO studio_plan(ticket,body) VALUES(?,?)",
        params![ticket, "review"],
    )
    .unwrap();
    let plan_id = conn.last_insert_rowid();
    log(&conn, job, "plan.add", &json!({ "id": plan_id }));
    let page = reader.activity_page(ticket, None).unwrap();
    assert_eq!(page.events[0].payload["label"], "已添加计划项");
    let snap = reader.snapshot(ticket, 0).unwrap();
    assert_eq!(snap.plan.len(), 1);
    assert_eq!(
        (snap.plan[0].body.as_str(), snap.plan[0].done),
        ("review", false)
    );
}

#[test]
fn snapshot_projects_visible_payloads_and_ticket_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let conn = writer(dir.path());
    let job = create_ticket(&conn, "C:/proj_x", "goal 100%", "codex");
    let ticket = job.ticket;
    let long = "长".repeat(70_000);
    let reply = log(
        &conn,
        job,
        "reply",
        &json!({"text": long, "sha256": "x", "session": "PRIVATE"}),
    );
    let mcp = log(
        &conn,
        job,
        "mcp.result",
        &json!({"id":"c1","server":"s","tool":"t","state":"error","result":"PRIVATE"}),
    );
    let follow = log(&conn, job, "user.follow", &json!(long));
    let hidden = log(&conn, job, "turn.admitted", &json!({}));
    conn.execute(
        "INSERT INTO approvals(ticket,job,turn,action) VALUES(?,?,1,?)",
        params![ticket, job.id, r#"{"kind":"shell","command":"ls"}"#],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO continuous_runs(ticket,milestone) VALUES(?,2)",
        [ticket],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO native_routes(job,route) VALUES(?,?)",
        params![job.id, r#"{"provider":"p","model":"m","mode":"cli"}"#],
    )
    .unwrap();

    let snap = store.snapshot(ticket, 0).unwrap();
    assert_eq!(snap.ticket.goal, "goal 100%");
    assert_eq!(snap.ticket.state, "queued");
    assert_eq!(snap.ticket.agent_routes, json!({}));
    assert!(snap.ticket.allow_lead_routing);
    assert_eq!(snap.cursor, hidden);
    assert_eq!(
        snap.jobs[0].route,
        Some(json!({"provider":"p","model":"m","mode":"cli"}))
    );
    let by_seq = |seq: i64| {
        snap.events
            .iter()
            .filter(|e| e.seq == seq)
            .collect::<Vec<_>>()
    };
    // Journal rows for VISIBLE kinds; their progress twins are not merged in.
    let r = by_seq(reply);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].kind, "reply");
    assert_eq!(r[0].payload, json!({"text": "长".repeat(64_000)}));
    assert_eq!(
        by_seq(mcp)[0].payload,
        json!({"id":"c1","server":"s","tool":"t","state":"error"})
    );
    assert_eq!(by_seq(follow)[0].payload, json!("长".repeat(64_000)));
    // Non-visible kinds appear only as derived activity rows.
    let h = by_seq(hidden);
    assert_eq!((h.len(), h[0].kind.as_str()), (1, "activity"));
    assert_eq!(
        h[0].payload,
        json!({"label":"调度器已接单","category":"run","state":"running"})
    );
    assert_eq!(h[0].agent.as_deref(), Some("codex"));
    assert!(snap.events.windows(2).all(|w| w[0].seq < w[1].seq));
    assert_eq!(
        snap.approvals[0].action,
        json!({"kind":"shell","command":"ls"})
    );
    assert_eq!(
        snap.run.as_ref().map(|r| (r.phase.as_str(), r.milestone)),
        Some(("work", 2))
    );
    assert_eq!(snap.native_activity.latest, None);

    // The activity page includes the VISIBLE originals' progress rows.
    let page = store.activity_page(ticket, None).unwrap();
    let labels: Vec<_> = page
        .events
        .iter()
        .map(|e| e.payload["label"].as_str().unwrap())
        .collect();
    assert_eq!(
        labels,
        [
            "调度器已接单",
            "已追加指令",
            "MCP 工具调用未成功",
            "本轮已返回答复",
            "已提交目标"
        ]
    );
    assert!(!serde_json::to_string(&page).unwrap().contains("PRIVATE"));

    // History filters escape LIKE wildcards.
    assert_eq!(store.history("100%").unwrap().len(), 1);
    assert_eq!(store.history("proj_").unwrap().len(), 1);
    assert_eq!(store.history("proj%").unwrap().len(), 0);
    assert_eq!(store.history("").unwrap()[0].state, "queued");

    // An empty incremental read keeps the cursor at the journal end.
    let tail = store.snapshot(ticket, snap.cursor).unwrap();
    assert!(tail.events.is_empty());
    assert_eq!(tail.cursor, snap.cursor);
    assert!(store.snapshot(ticket + 1, 0).is_err());
}

#[test]
fn latest_window_keeps_newest_180_events() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let conn = writer(dir.path());
    let job = create_ticket(&conn, "C:/p", "g", "claude");
    let mut last = 0;
    for i in 0..300 {
        last = log(
            &conn,
            job,
            if i % 3 == 0 { "native" } else { "delta" },
            &tool(i),
        );
    }
    let snap = store.snapshot(job.ticket, 0).unwrap();
    assert_eq!(snap.events.len(), 180);
    assert_eq!(snap.events.last().unwrap().seq, last);
    assert_eq!(snap.cursor, last);
    let seqs: Vec<_> = snap.events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, ((last - 179)..=last).collect::<Vec<_>>());
}

#[test]
fn read_only_open_requires_an_existing_journal() {
    let dir = tempfile::tempdir().unwrap();
    assert!(Store::open_read_only(dir.path()).is_err());
    assert!(!dir.path().join(sa_store::DB_FILE).exists());
}
