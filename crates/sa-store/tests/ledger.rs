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
    assert_eq!((ledger.rules_version, ledger.usage_rules_version), (3, 3));
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
    // No native subagent ran, so no subagent gap is claimed; the retried
    // attempt still makes the ticket partial.
    assert!(!reasons(&ledger).contains(&"native_subagents_excluded"), "{:?}", ledger.gaps);
    assert!(ledger.partial);
    assert_eq!(ledger.cost, None, "legacy field stays null");
    // These events carry no route or model, so nothing is priced.
    assert_eq!(ledger.api_cost.attempts_priced, 0);
    assert_eq!(ledger.api_cost.unpriced.get("model_unknown"), Some(&5));
    assert!(ledger.api_cost.totals.is_empty() && ledger.api_cost.partial);
}

/// 2026-11-21 12:00 UTC, a Saturday after the price table's effective date.
const SATURDAY: f64 = 1_795_262_400.0;
const HOUR: f64 = 3600.0;

fn log_at(conn: &Connection, job: Job, kind: &str, data: &Value, ts: f64) -> i64 {
    let seq = log(conn, job, kind, data);
    conn.execute("UPDATE events SET ts=? WHERE seq=?", params![ts, seq]).unwrap();
    seq
}

fn route(model: &str) -> Value {
    json!({"provider":"gateway","model":model,"mode":"cli"})
}

/// Claude result frame with uncached input, cache read, cache writes split
/// into 5-minute and 1-hour lifetimes, and output.
fn cl_priced(uncached: u64, read: u64, w5: u64, w1: u64, output: u64) -> Value {
    json!({"type":"result","parent_tool_use_id":null,"usage":{"input_tokens":uncached,"output_tokens":output,
        "cache_read_input_tokens":read,"cache_creation_input_tokens":w5 + w1,
        "cache_creation":{"ephemeral_5m_input_tokens":w5,"ephemeral_1h_input_tokens":w1}}})
}

fn claude_run(conn: &Connection, job: Job, ts: f64, init_model: Option<&str>, result: &Value) {
    log_at(conn, job, "turn.admitted", &json!({}), ts);
    log_at(conn, job, "route.requested", &route("claude-opus-5-5"), ts);
    if let Some(model) = init_model {
        log_at(conn, job, "native", &json!({"type":"system","subtype":"init","model":model,"session_id":"s"}), ts);
    }
    log_at(conn, job, "native", result, ts + 60.0);
    log_at(conn, job, "reply", &json!({"text":"ok"}), ts + 61.0);
}

#[test]
fn claude_attempts_are_priced_per_category_at_the_list_price_of_the_reported_model() {
    let (_dir, conn) = open();
    let lead = create_ticket(&conn, "C:/fixture", "ledger", "claude");
    claude_run(&conn, lead, SATURDAY, Some("claude-opus-5-5[1m]"), &cl_priced(1000, 2000, 100, 200, 500));
    let ledger = ledger_of(&conn, lead.ticket);
    let attempt = find(&ledger, lead.id, 1);
    assert_eq!(attempt.attribution.source, "native_init");
    assert_eq!(attempt.attribution.model.as_deref(), Some("claude-opus-5-5[1m]"));
    assert_eq!(attempt.attribution.provider.as_deref(), Some("gateway"));
    // Uncached 1000 × $4 + read 2000 × $0.20 + 5m write 100 × $5
    // + 1h write 200 × $8 + output 500 × $20, per million tokens.
    assert_eq!(attempt.cost.amount.as_deref(), Some("0.0165"));
    assert_eq!(attempt.cost.currency.as_deref(), Some("USD"));
    assert_eq!(attempt.cost.price_model.as_deref(), Some("claude-opus-5-5"));
    assert_eq!(attempt.cost.price_provider.as_deref(), Some("anthropic"));
    let cost = &ledger.api_cost;
    assert_eq!((cost.basis.as_str(), cost.price_table.as_str()), ("api_list_price_equivalent", "2026-11-20"));
    assert_eq!(cost.totals.len(), 1);
    assert_eq!((cost.totals[0].currency.as_str(), cost.totals[0].amount.as_str()), ("USD", "0.0165"));
    assert_eq!((cost.attempts_with_usage, cost.attempts_priced), (1, 1));
    // No subagent evidence: the Claude main loop is the whole reported scope.
    assert!(!reasons(&ledger).contains(&"native_subagents_excluded"), "{:?}", ledger.gaps);
    assert!(!ledger.partial && !cost.partial, "{:?}", ledger.gaps);
}

#[test]
fn unpriced_attempts_name_their_reason_and_make_the_total_partial() {
    let (_dir, conn) = open();
    let lead = create_ticket(&conn, "C:/fixture", "ledger", "claude");
    // Priced: route model, full split.
    claude_run(&conn, lead, SATURDAY, None, &cl_priced(1000, 0, 0, 0, 0));
    let cases: [(&str, f64, Option<&str>, Value); 4] = [
        // Cache writes without a lifetime split cannot choose a write price.
        ("cache_ttl_unknown", SATURDAY, None, {
            let mut v = cl_result(10, 1);
            v["usage"]["cache_creation_input_tokens"] = json!(5);
            v
        }),
        // Before the table vouches for the price.
        ("price_not_effective", SATURDAY - 3.0 * 86_400.0, None, cl_priced(10, 0, 0, 0, 1)),
        // A model the table does not list is never guessed.
        ("price_unknown", SATURDAY, Some("claude-sonnet-5-5-preview"), cl_priced(10, 0, 0, 0, 1)),
        // Fast mode is a different price list.
        ("nonstandard_mode", SATURDAY, None, {
            let mut v = cl_priced(10, 0, 0, 0, 1);
            v["usage"]["speed"] = json!("fast");
            v
        }),
    ];
    for (index, (_, ts, model, result)) in cases.iter().enumerate() {
        claude_run(&conn, at(lead, index as i64 + 2), *ts, *model, result);
    }
    // An interrupted run reports input but no output.
    let incomplete = at(lead, 6);
    log_at(&conn, incomplete, "turn.admitted", &json!({}), SATURDAY);
    log_at(&conn, incomplete, "native", &json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"m","usage":{"input_tokens":5,"output_tokens":1,"cache_read_input_tokens":0,
        "cache_creation_input_tokens":0}}}), SATURDAY);
    log_at(&conn, incomplete, "interrupted", &json!("中断"), SATURDAY + 1.0);
    let ledger = ledger_of(&conn, lead.ticket);
    assert_eq!(find(&ledger, lead.id, 1).cost.amount.as_deref(), Some("0.004"));
    for (index, (reason, ..)) in cases.iter().enumerate() {
        let attempt = find(&ledger, lead.id, index as i64 + 2);
        assert_eq!((attempt.cost.reason.as_deref(), &attempt.cost.amount), (Some(*reason), &None), "{reason}");
    }
    assert_eq!(find(&ledger, lead.id, 6).cost.reason.as_deref(), Some("tokens_incomplete"));
    // Route carried over from the first execution: still its model.
    assert_eq!(find(&ledger, lead.id, 6).attribution.source, "route_carried");
    let cost = &ledger.api_cost;
    assert_eq!((cost.attempts_with_usage, cost.attempts_priced), (6, 1));
    assert_eq!(cost.totals[0].amount, "0.004");
    assert!(cost.partial);
    for reason in ["cache_ttl_unknown", "price_not_effective", "price_unknown", "nonstandard_mode", "tokens_incomplete"] {
        assert_eq!(cost.unpriced.get(reason), Some(&1), "{reason}: {:?}", cost.unpriced);
    }
}

#[test]
fn route_rotation_attributes_each_execution_to_the_route_in_force() {
    let (_dir, conn) = open();
    let lead = create_ticket(&conn, "C:/fixture", "ledger", "codex");
    conn.execute(
        "INSERT INTO native_routes VALUES(?,?)",
        params![lead.id, route("gpt-6.2-astra").to_string()],
    )
    .unwrap();
    log_at(&conn, lead, "turn.admitted", &json!({}), SATURDAY);
    log_at(&conn, lead, "route.requested", &route("gpt-6.2-sol"), SATURDAY);
    log_at(&conn, lead, "native", &cx("a", 1000, 1000), SATURDAY + 1.0);
    log_at(&conn, lead, "route.rotated", &json!({"from":route("gpt-6.2-sol"),"to":route("gpt-6.1-sol"),
        "reason":"quota","attempt":1,"after_ms":0}), SATURDAY + 2.0);
    let second = at(lead, 2);
    log_at(&conn, second, "turn.admitted", &json!({}), SATURDAY + 3.0);
    log_at(&conn, second, "native", &cx("b", 3000, 3000), SATURDAY + 4.0);
    log_at(&conn, second, "reply", &json!({}), SATURDAY + 5.0);
    let ledger = ledger_of(&conn, lead.ticket);
    let first = find(&ledger, lead.id, 1);
    assert_eq!((first.attribution.source.as_str(), first.attribution.model.as_deref()), ("route", Some("gpt-6.2-sol")));
    assert_eq!(first.cost.amount.as_deref(), Some("0.002"));
    let rotated = find(&ledger, lead.id, 2);
    assert_eq!((rotated.attribution.source.as_str(), rotated.attribution.model.as_deref()), ("route_rotated", Some("gpt-6.1-sol")));
    assert_eq!(rotated.cost.amount.as_deref(), Some("0.006"));
    assert_eq!(ledger.api_cost.totals[0].amount, "0.008");

    // A rotated job's current route does not describe executions without
    // their own evidence; a job that never rotated may use it.
    let quiet = child(&conn, lead, "codex");
    conn.execute("INSERT INTO native_routes VALUES(?,?)", params![quiet.id, route("gpt-6.2-sol").to_string()]).unwrap();
    log_at(&conn, quiet, "turn.admitted", &json!({}), SATURDAY);
    log_at(&conn, quiet, "native", &cx("q", 500, 500), SATURDAY + 1.0);
    log_at(&conn, quiet, "reply", &json!({}), SATURDAY + 2.0);
    let rotated_job = child(&conn, lead, "codex");
    conn.execute("INSERT INTO native_routes VALUES(?,?)", params![rotated_job.id, route("gpt-6.2-sol").to_string()]).unwrap();
    log_at(&conn, rotated_job, "turn.admitted", &json!({}), SATURDAY);
    log_at(&conn, rotated_job, "native", &cx("r", 500, 500), SATURDAY + 1.0);
    log_at(&conn, rotated_job, "route.rotated", &json!({"from":{},"to":{},"reason":"auth"}), SATURDAY + 2.0);
    let ledger = ledger_of(&conn, lead.ticket);
    assert_eq!(find(&ledger, quiet.id, 1).attribution.source, "job_route");
    assert_eq!(find(&ledger, quiet.id, 1).cost.amount.as_deref(), Some("0.001"));
    let unknown = find(&ledger, rotated_job.id, 1);
    assert_eq!((unknown.attribution.source.as_str(), unknown.cost.reason.as_deref()), ("unknown", Some("model_unknown")));
}

#[test]
fn long_context_and_time_of_day_tiers_are_priced_only_when_decidable() {
    let (_dir, conn) = open();
    let lead = create_ticket(&conn, "C:/fixture", "ledger", "codex");
    log_at(&conn, lead, "turn.admitted", &json!({}), SATURDAY);
    log_at(&conn, lead, "route.requested", &route("gpt-6.2-sol"), SATURDAY);
    // Two requests of 200K each: 400K in total, but no single request is
    // above the 272K threshold.
    log_at(&conn, lead, "native", &cx("t", 200_000, 200_000), SATURDAY + 1.0);
    log_at(&conn, lead, "native", &cx("t", 400_000, 200_000), SATURDAY + 2.0);
    log_at(&conn, lead, "reply", &json!({}), SATURDAY + 3.0);
    let big = at(lead, 2);
    log_at(&conn, big, "turn.admitted", &json!({}), SATURDAY + 4.0);
    // One frame merging 300K of new input: a single request may exceed it.
    log_at(&conn, big, "native", &cx("t", 700_000, 100_000), SATURDAY + 5.0);
    log_at(&conn, big, "reply", &json!({}), SATURDAY + 6.0);
    let ds = |total: u64| json!({"type":"status","phase":"step_end","turn":1,"step":total,"usage":{
        "totalTokens":total,"outputTokens":0,"inputTokens":total,"cacheReadTokens":0,"cacheWriteTokens":0}});
    let weekend = child(&conn, lead, "deepseek");
    log_at(&conn, weekend, "turn.admitted", &json!({}), SATURDAY);
    log_at(&conn, weekend, "route.requested", &route("deepseek-v4-flash"), SATURDAY);
    log_at(&conn, weekend, "native", &ds(1_000_000), SATURDAY + 1.0);
    log_at(&conn, weekend, "reply", &json!({}), SATURDAY + 2.0);
    let peak = child(&conn, lead, "deepseek");
    // Monday 2026-11-23 02:00 UTC is inside a weekday peak window.
    let monday_peak = SATURDAY + 2.0 * 86_400.0 - 10.0 * HOUR;
    log_at(&conn, peak, "turn.admitted", &json!({}), monday_peak);
    log_at(&conn, peak, "route.requested", &route("deepseek-flash"), monday_peak);
    log_at(&conn, peak, "native", &ds(1_000_000), monday_peak + 1.0);
    log_at(&conn, peak, "reply", &json!({}), monday_peak + 2.0);
    let ledger = ledger_of(&conn, lead.ticket);
    assert_eq!(find(&ledger, lead.id, 1).cost.amount.as_deref(), Some("0.8"));
    assert_eq!(find(&ledger, lead.id, 2).cost.reason.as_deref(), Some("tier_ambiguous"));
    let off_peak = find(&ledger, weekend.id, 1);
    assert_eq!((off_peak.cost.amount.as_deref(), off_peak.cost.tier.as_deref()), (Some("0.15"), Some("off_peak")));
    assert_eq!(off_peak.cost.price_model.as_deref(), Some("deepseek-flash"), "documented alias");
    assert_eq!(find(&ledger, peak.id, 1).cost.reason.as_deref(), Some("tier_ambiguous"));
}

#[test]
fn the_subagent_gap_follows_journal_evidence() {
    let (_dir, conn) = open();
    let lead = create_ticket(&conn, "C:/fixture", "ledger", "claude");
    claude_run(&conn, lead, SATURDAY, None, &cl_priced(10, 0, 0, 0, 1));
    let quiet = child(&conn, lead, "claude");
    claude_run(&conn, quiet, SATURDAY, None, &cl_priced(10, 0, 0, 0, 1));
    let tool = child(&conn, lead, "claude");
    log(&conn, tool, "native", &json!({"type":"assistant","parent_tool_use_id":null,"message":{"id":"t",
        "content":[{"type":"tool_use","id":"toolu_1","name":"Agent","input":{}}]}}));
    claude_run(&conn, tool, SATURDAY, None, &cl_priced(10, 0, 0, 0, 1));
    let framed = child(&conn, lead, "claude");
    claude_run(&conn, framed, SATURDAY, None, &cl_priced(10, 0, 0, 0, 1));
    // An oversized subagent frame is not parsed but still counts as evidence.
    log(&conn, framed, "native", &json!({"type":"user","parent_tool_use_id":"toolu_2",
        "message":{"content":"x".repeat(300_000)}}));
    let ledger = ledger_of(&conn, lead.ticket);
    let flagged: Vec<i64> = ledger
        .gaps
        .iter()
        .filter(|g| g.reason == "native_subagents_excluded")
        .filter_map(|g| g.job)
        .collect();
    assert_eq!(flagged, vec![tool.id, framed.id], "{:?}", ledger.gaps);
    // The subagent gap leaves the total partial; the priced main loop is kept.
    assert!(ledger.partial && ledger.api_cost.partial);
    assert_eq!(ledger.api_cost.attempts_priced, 4);
}

#[test]
fn advisor_calls_are_priced_on_the_recorded_advisor_route() {
    let (_dir, conn) = open();
    let lead = create_ticket(&conn, "C:/fixture", "ledger", "claude");
    claude_run(&conn, lead, SATURDAY, None, &cl_priced(10, 0, 0, 0, 0));
    let seq = log_at(&conn, lead, "advice.started",
        &json!({"advisor":{"agent":"codex","provider":"p","model":"gpt-6.2-sol"}}), SATURDAY + 10.0);
    let id = format!("advice:{seq}");
    let sample = sa_store::usage::advisor_usage_sample("codex", &cx("advisor", 5000, 5000)).unwrap();
    log_at(&conn, lead, "usage.observed",
        &json!({"schema_version":1,"role":"advisor","attempt_id":id,"agent":"codex","sample":sample}), SATURDAY + 11.0);
    log_at(&conn, lead, "advice.result", &json!({"attempt_id":id,"state":"done","usage_complete":true}), SATURDAY + 12.0);
    let ledger = ledger_of(&conn, lead.ticket);
    let advisor = ledger.attempts.iter().find(|a| a.attempt_id.as_deref() == Some(id.as_str())).unwrap();
    assert_eq!(advisor.attribution.source, "advice_route");
    assert_eq!(advisor.attribution.provider.as_deref(), Some("p"));
    assert_eq!(advisor.cost.amount.as_deref(), Some("0.01"));
    assert_eq!(ledger.api_cost.totals[0].amount, "0.01004");
}

#[test]
fn the_status_report_sums_cost_per_currency_across_tickets() {
    let dir = tempfile::tempdir().unwrap();
    Store::open(dir.path()).unwrap();
    let conn = writer(dir.path());
    for _ in 0..2 {
        let lead = create_ticket(&conn, "C:/fixture", "ledger", "claude");
        claude_run(&conn, lead, SATURDAY, None, &cl_priced(1000, 0, 0, 0, 0));
    }
    let report = ledger::status_report(dir.path(), None).unwrap();
    assert_eq!(report.rules_version, 3);
    assert_eq!(report.cost, None);
    assert_eq!(report.api_cost.totals.len(), 1);
    assert_eq!((report.api_cost.totals[0].amount.as_str(), report.api_cost.totals[0].attempts), ("0.008", 2));
    assert_eq!(report.tickets[0].api_cost.totals[0].amount, "0.004");
    assert!(!report.api_cost.partial);
    // Older readers that only know the legacy null `cost` still parse it.
    #[derive(serde::Deserialize)]
    struct Legacy {
        cost: Option<String>,
    }
    let legacy: Legacy = serde_json::from_value(serde_json::to_value(&report).unwrap()).unwrap();
    assert_eq!(legacy.cost, None);
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

/// An operator retry of an interrupted task reuses the turn number but is a
/// second native run. Both runs are listed, and the interrupted run's usage
/// is not replaced by the retry's terminal result.
#[test]
fn an_operator_retry_of_the_same_turn_is_a_separate_execution() {
    let (_dir, conn) = open();
    let lead = create_ticket(&conn, "C:/fixture", "ledger", "claude");
    let message = |id: &str, input: u64, output: u64| {
        json!({"type":"assistant","parent_tool_use_id":null,"message":{"id":id,"usage":{
            "input_tokens":input,"output_tokens":output,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}})
    };
    log(&conn, lead, "turn.admitted", &json!({}));
    log(&conn, lead, "native", &message("m1", 30, 3));
    log(&conn, lead, "interrupted", &json!("上次执行中断"));
    log(&conn, lead, "retry", &json!("重试同一逻辑轮次"));
    log(&conn, lead, "turn.admitted", &json!({}));
    log(&conn, lead, "native", &message("m2", 40, 4));
    log(&conn, lead, "native", &cl_result(40, 4));
    log(&conn, lead, "reply", &json!({"text":"done"}));

    let ledger = ledger_of(&conn, lead.ticket);
    let runs: Vec<_> = ledger.attempts.iter().filter(|a| a.job == lead.id && a.turn == Some(1)).collect();
    assert_eq!(runs.len(), 2, "{:#?}", ledger.attempts);
    let (first, second) = (runs[0], runs[1]);
    assert_eq!((first.execution, first.kind.as_str(), first.outcome.as_str()), (0, "initial", "interrupted"));
    // An unfinished assistant message has no trustworthy output count, so
    // only its input is known; the total says unknown instead of undercounting.
    assert_eq!((first.tokens.input_tokens, first.tokens.total_tokens), (Some(30), None));
    assert_eq!((second.execution, second.kind.as_str(), second.outcome.as_str()), (1, "retry", "replied"));
    assert_eq!(second.tokens.total_tokens, Some(44));
    assert_eq!((ledger.totals.input_tokens, ledger.totals.total_tokens), (Some(70), None));
    assert_eq!(ledger.coverage.attempts, 2);
    assert!(ledger.partial);
}
