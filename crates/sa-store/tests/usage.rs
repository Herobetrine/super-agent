mod common;

use common::*;
use rusqlite::Connection;
use sa_store::{Snapshot, Store, UsageScope};
use serde_json::{Value, json};
use tempfile::TempDir;

fn fixture(agent: &str) -> (TempDir, Store, Connection, Job) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let writer = writer(dir.path());
    let job = create_ticket(&writer, "C:/fixture", "synthetic usage", agent);
    log(&writer, job, "turn.admitted", &json!({}));
    (dir, store, writer, job)
}

fn native(conn: &Connection, job: Job, event: Value) {
    log(conn, job, "native", &event);
}

fn done(conn: &Connection, job: Job) {
    log(conn, job, "reply", &json!({"text":"done"}));
}

fn snapshot(store: &mut Store, job: Job) -> Snapshot {
    for _ in 0..100 {
        let snapshot = store.snapshot(job.ticket, 0).unwrap();
        if !snapshot.usage.pending {
            return snapshot;
        }
    }
    panic!("usage scan did not catch up");
}

fn cx_counts(input: u64, output: u64, cached: u64) -> Value {
    json!({"inputTokens":input,"outputTokens":output,"cachedInputTokens":cached,
        "cacheWriteInputTokens":0,"reasoningOutputTokens":output/2,"totalTokens":input+output})
}

fn cx(thread: &str, total: Value, last: Value) -> Value {
    json!({"method":"thread/tokenUsage/updated","params":{
        "threadId":thread,"turnId":"turn-a","tokenUsage":{"total":total,"last":last}}})
}

fn cl_counts(input: u64, output: u64) -> Value {
    json!({"input_tokens":input,"output_tokens":output,"cache_read_input_tokens":20,
        "cache_creation_input_tokens":10})
}

fn ds(session_turn: i64, step: i64, usage: Value) -> Value {
    json!({"type":"status","phase":"step_end","turn":session_turn,"step":step,"usage":usage})
}

fn ag_counts(input: u64, output: u64, cached: u64) -> Value {
    json!({"input_tokens":input,"output_tokens":output,"total_tokens":input+output,
        "thinking_tokens":output/2,"cache_read_tokens":cached})
}

fn ag_step(step: i64, usage: Value) -> Value {
    json!({"event":"step_update","step_update":{"conversation_id":"ag-session",
        "step_index":step,"state":"DONE","step_type":"agent_response","usage":usage}})
}

fn ag_result(usage: Value) -> Value {
    json!({"event":"result","result":{"conversation_id":"ag-session","usage":usage,"status":"SUCCESS"}})
}

#[test]
fn codex_cumulative_frames_resume_and_new_sessions_are_counted_once() {
    let (_dir, mut store, writer, job) = fixture("codex");
    let first = cx("thread-a", cx_counts(100, 20, 60), cx_counts(100, 20, 60));
    native(&writer, job, first.clone());
    native(&writer, job, first.clone());
    done(&writer, job);
    assert_eq!(
        snapshot(&mut store, job).usage.totals.total_tokens,
        Some(120)
    );

    let next = Job { turn: 2, ..job };
    log(&writer, next, "turn.admitted", &json!({}));
    native(&writer, next, first);
    let update = cx("thread-a", cx_counts(180, 50, 100), cx_counts(80, 30, 40));
    native(&writer, next, update.clone());
    native(&writer, next, update);
    done(&writer, next);
    let third = Job { turn: 3, ..job };
    log(&writer, third, "turn.admitted", &json!({}));
    native(
        &writer,
        third,
        cx("thread-b", cx_counts(10, 4, 0), cx_counts(10, 4, 0)),
    );
    done(&writer, third);
    let result = snapshot(&mut store, job).usage;
    assert_eq!(result.totals.total_tokens, Some(244));
    assert_eq!(result.totals.input_tokens, Some(190));
    assert_eq!(result.totals.output_tokens, Some(54));
    assert_eq!(result.totals.cache_read_input_tokens, Some(100));
    assert_eq!(result.jobs[0].reported_attempts, 3);
    assert!(!result.partial);
    assert_eq!(
        snapshot(&mut store, job).usage,
        result,
        "unchanged refresh must be idempotent"
    );
}

#[test]
fn codex_missing_baseline_and_failed_attempts_are_partial() {
    let (_dir, mut store, writer, job) = fixture("codex");
    native(
        &writer,
        job,
        cx(
            "old-thread",
            cx_counts(1000, 200, 600),
            cx_counts(40, 10, 20),
        ),
    );
    log(&writer, job, "turn.retry_scheduled", &json!({"attempt":1}));
    let failed = Job { turn: 2, ..job };
    log(&writer, failed, "turn.admitted", &json!({}));
    log(&writer, failed, "turn.gave_up", &json!({}));
    let result = snapshot(&mut store, job).usage;
    assert_eq!(
        result.totals.total_tokens,
        Some(50),
        "unobserved upstream history is excluded"
    );
    assert_eq!(result.jobs[0].reported_attempts, 1);
    assert_eq!(result.jobs[0].unreported_attempts, 1);
    assert!(result.partial);
}

#[test]
fn codex_out_of_order_cumulative_frames_do_not_lower_the_baseline() {
    let (_dir, mut store, writer, job) = fixture("codex");
    let first = cx("a", cx_counts(10, 2, 0), cx_counts(10, 2, 0));
    let second = cx("a", cx_counts(30, 5, 0), cx_counts(20, 3, 0));
    native(&writer, job, first.clone());
    native(&writer, job, second.clone());
    native(&writer, job, first);
    native(&writer, job, second);
    done(&writer, job);
    let result = snapshot(&mut store, job).usage;
    assert_eq!(result.totals.total_tokens, Some(35));
    assert!(result.partial);
}

#[test]
fn claude_result_replaces_main_loop_placeholders_and_resume_counts_per_turn() {
    let (_dir, mut store, writer, job) = fixture("claude");
    let assistant = json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg-a","usage":cl_counts(50, 1)}});
    native(&writer, job, assistant.clone());
    native(&writer, job, assistant);
    native(
        &writer,
        job,
        json!({"type":"assistant","parent_tool_use_id":"native-child",
        "message":{"id":"child-message","usage":cl_counts(9999, 1)}}),
    );
    let before = snapshot(&mut store, job).usage;
    assert_eq!(before.totals.input_tokens, Some(80));
    assert_eq!(
        before.totals.output_tokens, None,
        "assistant output is a placeholder"
    );
    let result = json!({"type":"result","subtype":"success","usage":cl_counts(70, 25),
        "modelUsage":{"model":{"inputTokens":9999,"outputTokens":9999}}});
    native(&writer, job, result.clone());
    native(&writer, job, result);
    done(&writer, job);
    let resumed = Job { turn: 2, ..job };
    log(&writer, resumed, "turn.admitted", &json!({}));
    native(
        &writer,
        resumed,
        json!({"type":"result","subtype":"success","usage":cl_counts(30, 5)}),
    );
    done(&writer, resumed);
    let result = snapshot(&mut store, job).usage;
    assert_eq!(result.totals.total_tokens, Some(190));
    assert_eq!(result.totals.input_tokens, Some(160));
    assert_eq!(result.totals.output_tokens, Some(30));
    assert_eq!(result.jobs[0].scope, UsageScope::MainLoop);
    assert!(
        result.partial,
        "native subagents are explicitly outside the reported scope"
    );
}

#[test]
fn claude_crash_zero_does_not_erase_reported_input() {
    let (_dir, mut store, writer, job) = fixture("claude");
    native(
        &writer,
        job,
        json!({"type":"assistant","message":{"id":"m","usage":cl_counts(50, 1)}}),
    );
    native(
        &writer,
        job,
        json!({"type":"result","is_error":true,"subtype":"error_during_execution",
        "usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}),
    );
    log(&writer, job, "failed", &json!("synthetic failure"));
    let result = snapshot(&mut store, job).usage;
    assert_eq!(result.totals.input_tokens, Some(80));
    assert_eq!(result.totals.output_tokens, None);
    assert_eq!(result.totals.total_tokens, None);
    assert!(result.partial);
}

#[test]
fn deepseek_steps_use_disjoint_cache_buckets_and_deduplicate_resume() {
    let (_dir, mut store, writer, job) = fixture("deepseek");
    native(&writer, job, json!({"type":"session","sessionId":"ds-a"}));
    let step = ds(
        0,
        0,
        json!({"inputTokens":10,"outputTokens":7,"cacheReadTokens":40,
        "cacheWriteTokens":5,"reasoningTokens":3,"totalTokens":62}),
    );
    native(&writer, job, step.clone());
    native(&writer, job, step.clone());
    done(&writer, job);
    let resumed = Job { turn: 2, ..job };
    log(&writer, resumed, "turn.admitted", &json!({}));
    native(
        &writer,
        resumed,
        json!({"type":"session","sessionId":"ds-a"}),
    );
    native(&writer, resumed, step);
    native(
        &writer,
        resumed,
        ds(
            1,
            0,
            json!({"inputTokens":20,"outputTokens":8,"totalTokens":28}),
        ),
    );
    done(&writer, resumed);
    let result = snapshot(&mut store, job).usage;
    assert_eq!(result.totals.total_tokens, Some(90));
    assert_eq!(result.totals.input_tokens, Some(75));
    assert_eq!(result.totals.output_tokens, Some(15));
    assert_eq!(
        result.totals.cache_read_input_tokens, None,
        "the resumed step omitted this bucket"
    );
    assert_eq!(result.totals.cache_creation_input_tokens, None);
    assert!(!result.partial);
}

#[test]
fn deepseek_missing_native_retry_usage_stays_unknown() {
    let (_dir, mut store, writer, job) = fixture("deepseek");
    native(&writer, job, ds(0, 0, Value::Null));
    done(&writer, job);
    let result = snapshot(&mut store, job).usage;
    assert_eq!(result.totals.total_tokens, None);
    assert_eq!(result.unreported_jobs, 1);
    assert_eq!(result.jobs[0].unreported_attempts, 1);
    assert!(result.partial);
}

#[test]
fn antigravity_result_supersedes_steps_and_survives_resume() {
    let (_dir, mut store, writer, job) = fixture("antigravity");
    let step = ag_step(1, ag_counts(100, 10, 30));
    native(&writer, job, step.clone());
    native(&writer, job, step.clone());
    let result = ag_result(ag_counts(120, 14, 30));
    native(&writer, job, result.clone());
    native(&writer, job, result.clone());
    native(&writer, job, step);
    done(&writer, job);
    assert_eq!(
        snapshot(&mut store, job).usage.totals.total_tokens,
        Some(134)
    );

    let next = Job { turn: 2, ..job };
    log(&writer, next, "turn.admitted", &json!({}));
    native(&writer, next, ag_step(3, ag_counts(20, 5, 0)));
    native(&writer, next, result); // Duplicate old result must not erase pending steps.
    assert_eq!(
        snapshot(&mut store, job).usage.totals.total_tokens,
        Some(159)
    );
    native(&writer, next, ag_result(ag_counts(140, 19, 30)));
    done(&writer, next);
    let result = snapshot(&mut store, job).usage;
    assert_eq!(result.totals.total_tokens, Some(159));
    assert_eq!(result.totals.output_tokens, Some(19));
    assert_eq!(
        result.totals.input_tokens, None,
        "backend cache overlap is unspecified"
    );
    assert!(!result.partial);
}

#[test]
fn readonly_scan_is_bounded_and_legacy_snapshot_json_defaults_usage() {
    let (dir, store, writer, job) = fixture("codex");
    drop(store);
    let mut store = Store::open_read_only(dir.path()).unwrap();
    for _ in 0..sa_store::usage::USAGE_SCAN_BATCH + 8 {
        native(
            &writer,
            job,
            cx("a", cx_counts(10, 2, 0), cx_counts(10, 2, 0)),
        );
    }
    done(&writer, job);
    let first = store.snapshot(job.ticket, 0).unwrap();
    assert!(first.usage.pending);
    assert!(first.usage.partial);
    let finished = snapshot(&mut store, job);
    assert!(!finished.usage.pending);
    assert_eq!(finished.usage.totals.total_tokens, Some(12));
    let mut legacy = serde_json::to_value(&finished).unwrap();
    legacy.as_object_mut().unwrap().remove("usage");
    let decoded: Snapshot = serde_json::from_value(legacy).unwrap();
    assert_eq!(decoded.usage, Default::default());
}

#[test]
fn oversized_and_invalid_samples_are_not_reported_as_zero() {
    let (_dir, mut store, writer, job) = fixture("codex");
    let mut oversized = cx("a", cx_counts(10, 2, 0), cx_counts(10, 2, 0));
    oversized["padding"] = json!("x".repeat(sa_store::usage::USAGE_PAYLOAD_LIMIT));
    native(&writer, job, oversized);
    native(
        &writer,
        job,
        cx(
            "a",
            json!({"totalTokens":-1,"inputTokens":"10","outputTokens":2.5}),
            json!({}),
        ),
    );
    done(&writer, job);
    let result = snapshot(&mut store, job).usage;
    assert!(result.partial);
    assert_eq!(result.totals.total_tokens, None);
    assert_eq!(result.jobs[0].reported_attempts, 0);
}

#[test]
fn ticket_totals_include_jobs_outside_the_visible_page_and_unknown_jobs() {
    let (_dir, mut store, writer, root) = fixture("codex");
    native(
        &writer,
        root,
        cx("root", cx_counts(1, 1, 0), cx_counts(1, 1, 0)),
    );
    done(&writer, root);
    for i in 0..85 {
        writer.execute(
            "INSERT INTO jobs(ticket,parent,agent,body,writing,depth,state) VALUES(?1,?2,'deepseek','child',0,1,'done')",
            rusqlite::params![root.ticket, root.id],
        ).unwrap();
        let child = Job {
            id: writer.last_insert_rowid(),
            ..root
        };
        log(&writer, child, "turn.admitted", &json!({}));
        if i != 84 {
            native(
                &writer,
                child,
                ds(
                    0,
                    0,
                    json!({"inputTokens":1,"outputTokens":1,"totalTokens":2}),
                ),
            );
        }
        done(&writer, child);
    }
    let result = snapshot(&mut store, root);
    assert_eq!(result.jobs.len(), 80);
    assert_eq!(result.usage.jobs.len(), 80);
    assert_eq!(result.usage.totals.total_tokens, Some(170));
    assert_eq!(result.usage.unreported_jobs, 1);
    assert!(result.usage.partial);
}
