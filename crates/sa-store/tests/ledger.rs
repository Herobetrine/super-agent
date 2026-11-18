//! Spending ledger (architecture §7, P0A exit fixtures): duplicate frames,
//! cumulative corrections, missing usage, parent/child overlap, retries,
//! repairs, advisors and abandoned work are each attributed exactly once.
mod common;

use common::*;
use rusqlite::{Connection, params};
use sa_store::{Store, ledger::{self, Ledger}};
use serde_json::{Value, json};

fn open() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    Store::open(dir.path()).unwrap();
    let conn = writer(dir.path());
    (dir, conn)
}

fn child(conn: &Connection, parent: Job, agent: &str) -> Job {
    conn.execute(
        "INSERT INTO jobs(ticket,parent,sender,agent,body,writing,depth,state) VALUES(?,?,?,?,'work',1,1,'running')",
        params![parent.ticket, parent.id, "lead", agent],
    )
    .unwrap();
    Job { ticket: parent.ticket, id: conn.last_insert_rowid(), turn: 1 }
}

fn at(job: Job, turn: i64) -> Job {
    Job { turn, ..job }
}

fn cx(thread: &str, total: u64, last: u64) -> Value {
    let counts = |n: u64| json!({"inputTokens":n,"outputTokens":0,"cachedInputTokens":0,
        "cacheWriteInputTokens":0,"reasoningOutputTokens":0,"totalTokens":n});
    json!({"method":"thread/tokenUsage/updated","params":{"threadId":thread,
        "tokenUsage":{"total":counts(total),"last":counts(last)}}})
}

fn cl_result(input: u64, output: u64) -> Value {
    json!({"type":"result","parent_tool_use_id":null,"usage":{"input_tokens":input,"output_tokens":output,
        "cache_read_input_tokens":0,"cache_creation_input_tokens":0}})
}

fn ledger_of(conn: &Connection, ticket: i64) -> Ledger {
    ledger::ticket_ledger(conn, ticket).unwrap()
}

fn find(ledger: &Ledger, job: i64, turn: i64) -> &ledger::LedgerAttempt {
    ledger
        .attempts
        .iter()
        .find(|a| a.job == job && a.turn == Some(turn) && a.attempt_id.is_none())
        .unwrap_or_else(|| panic!("attempt {job}/{turn} missing: {:#?}", ledger.attempts))
}

fn reasons(ledger: &Ledger) -> Vec<&str> {
    ledger.gaps.iter().map(|g| g.reason.as_str()).collect()
}

#[test]
fn retries_repairs_and_advisors_are_separate_and_sum_to_the_ticket_total() {
    let (_dir, conn) = open();
    let lead = create_ticket(&conn, "C:/fixture", "ledger", "codex");
    log(&conn, lead, "turn.admitted", &json!({}));
    log(&conn, lead, "native", &cx("lead", 100, 100));
    // A replayed frame after reconnect is not counted again.
    log(&conn, lead, "native", &cx("lead", 100, 100));
    log(&conn, lead, "turn.retry_scheduled", &json!({}));
    let retry = at(lead, 2);
    log(&conn, retry, "turn.admitted", &json!({}));
    log(&conn, retry, "native", &cx("lead", 150, 50));
    log(&conn, retry, "reply", &json!({"text":"ok"}));
    let worker = child(&conn, lead, "claude");
    log(&conn, worker, "turn.admitted", &json!({}));
    log(&conn, worker, "native", &cl_result(40, 10));
    log(&conn, worker, "reply", &json!({"text":"done"}));
    log(&conn, worker, "check.bounced", &json!({"attempt":1}));
    let repair = at(worker, 2);
    log(&conn, repair, "turn.admitted", &json!({}));
    log(&conn, repair, "native", &cl_result(20, 5));
    log(&conn, repair, "reply", &json!({"text":"fixed"}));
    let seq = log(&conn, retry, "advice.started", &json!({"advisor":{"agent":"codex"}}));
    let id = format!("advice:{seq}");
    let sample = sa_store::usage::advisor_usage_sample("codex", &cx("advisor", 7, 7)).unwrap();
    log(&conn, retry, "usage.observed",
        &json!({"schema_version":1,"role":"advisor","attempt_id":id,"agent":"codex","sample":sample}));
    log(&conn, retry, "advice.result", &json!({"attempt_id":id,"state":"done","usage_complete":true}));

    let ledger = ledger_of(&conn, lead.ticket);
    assert_eq!((ledger.rules_version, ledger.usage_rules_version), (1, 2));
    let first = find(&ledger, lead.id, 1);
    assert_eq!((first.role.as_str(), first.kind.as_str(), first.outcome.as_str()), ("lead", "initial", "retried"));
    assert_eq!(first.tokens.total_tokens, Some(100));
    let second = find(&ledger, lead.id, 2);
    assert_eq!((second.kind.as_str(), second.outcome.as_str()), ("retry", "replied"));
    assert_eq!(second.tokens.total_tokens, Some(50));
    let bounced = find(&ledger, worker.id, 1);
    assert_eq!((bounced.role.as_str(), bounced.kind.as_str()), ("collaborator", "initial"));
    let fixed = find(&ledger, worker.id, 2);
    assert_eq!((fixed.kind.as_str(), fixed.outcome.as_str()), ("repair", "replied"));
    assert_eq!(fixed.tokens.total_tokens, Some(25));
    let advisor = ledger.attempts.iter().find(|a| a.attempt_id.as_deref() == Some(id.as_str())).unwrap();
    assert_eq!((advisor.role.as_str(), advisor.kind.as_str(), advisor.job), ("advisor", "consult", lead.id));
    assert_eq!(advisor.tokens.total_tokens, Some(7));
    // 100 + 50 (lead) + 50 + 25 (collaborator) + 7 (advisor).
    assert_eq!(ledger.totals.total_tokens, Some(232));
    let bucket_sum: u64 = ledger.buckets.iter().map(|b| b.tokens.total_tokens.unwrap()).sum();
    assert_eq!(bucket_sum, 232);
    // Claude covers only its main loop: the gap is stated, not hidden.
    assert!(reasons(&ledger).contains(&"native_subagents_excluded"), "{:?}", ledger.gaps);
    assert!(ledger.partial);
    assert_eq!(ledger.cost, None);
}

#[test]
fn failed_and_cancelled_attempts_keep_reported_usage() {
    let (_dir, conn) = open();
    let lead = create_ticket(&conn, "C:/fixture", "ledger", "codex");
    log(&conn, lead, "turn.admitted", &json!({}));
    log(&conn, lead, "native", &cx("t", 30, 30));
    log(&conn, lead, "failed", &json!({"error":"boom"}));
    let worker = child(&conn, lead, "codex");
    log(&conn, worker, "turn.admitted", &json!({}));
    log(&conn, worker, "native", &cx("w", 12, 12));
    log(&conn, worker, "cancelled", &json!({}));
    let ledger = ledger_of(&conn, lead.ticket);
    let failed = find(&ledger, lead.id, 1);
    assert_eq!((failed.outcome.as_str(), failed.tokens.total_tokens), ("failed", Some(30)));
    let abandoned = find(&ledger, worker.id, 1);
    assert_eq!((abandoned.outcome.as_str(), abandoned.tokens.total_tokens), ("abandoned", Some(12)));
    assert_eq!(ledger.totals.total_tokens, Some(42));
}

#[test]
fn a_native_session_shared_by_parent_and_child_is_partial_not_double_counted_silently() {
    let (dir, conn) = open();
    let lead = create_ticket(&conn, "C:/fixture", "ledger", "codex");
    log(&conn, lead, "turn.admitted", &json!({}));
    log(&conn, lead, "native", &cx("same-thread", 100, 100));
    log(&conn, lead, "reply", &json!({}));
    let worker = child(&conn, lead, "codex");
    log(&conn, worker, "turn.admitted", &json!({}));
    log(&conn, worker, "native", &cx("same-thread", 160, 60));
    log(&conn, worker, "reply", &json!({}));
    let ledger = ledger_of(&conn, lead.ticket);
    assert!(reasons(&ledger).contains(&"shared_native_session"), "{:?}", ledger.gaps);
    assert!(ledger.partial);
    let mut store = Store::open(dir.path()).unwrap();
    let mut snapshot = store.snapshot(lead.ticket, 0).unwrap();
    while snapshot.usage.pending {
        snapshot = store.snapshot(lead.ticket, 0).unwrap();
    }
    assert_eq!(snapshot.usage.shared_native_sessions, 1);
    assert!(snapshot.usage.partial);
}

#[test]
fn missing_corrected_unmetered_and_cumulative_usage_are_named_gaps() {
    let (_dir, conn) = open();
    let lead = create_ticket(&conn, "C:/fixture", "ledger", "codex");
    log(&conn, lead, "turn.admitted", &json!({}));
    log(&conn, lead, "native", &cx("t", 100, 100));
    // An older cumulative frame arriving late is a correction, not usage.
    log(&conn, lead, "native", &cx("t", 80, 80));
    log(&conn, lead, "reply", &json!({}));
    let silent = child(&conn, lead, "codex");
    log(&conn, silent, "turn.admitted", &json!({}));
    log(&conn, silent, "reply", &json!({}));
    let acp = child(&conn, lead, "acp");
    log(&conn, acp, "turn.admitted", &json!({}));
    log(&conn, acp, "reply", &json!({}));
    let agy = child(&conn, lead, "antigravity");
    log(&conn, agy, "turn.admitted", &json!({}));
    log(&conn, agy, "native", &json!({"event":"result","result":{"conversation_id":"c","usage":
        {"total_tokens":9,"output_tokens":3,"cache_read_tokens":0,"thinking_tokens":0}}}));
    log(&conn, agy, "reply", &json!({}));
    let never = child(&conn, lead, "codex");
    let ledger = ledger_of(&conn, lead.ticket);
    let gaps = reasons(&ledger);
    for reason in ["partial_samples", "unreported_attempt", "no_usage_protocol", "cumulative_unallocated", "no_usage_events"] {
        assert!(gaps.contains(&reason), "{reason}: {:?}", ledger.gaps);
    }
    assert_eq!(find(&ledger, lead.id, 1).tokens.total_tokens, Some(100));
    let unallocated = ledger.attempts.iter().find(|a| a.job == agy.id && a.turn.is_none()).unwrap();
    assert_eq!((unallocated.kind.as_str(), unallocated.tokens.total_tokens), ("unallocated", Some(9)));
    assert!(ledger.attempts.iter().any(|a| a.job == never.id && a.outcome == "unknown"));
    assert_eq!(ledger.totals.total_tokens, Some(109));
    assert!(ledger.coverage.attempts > ledger.coverage.reported_attempts);
    assert!(ledger.partial);
}
