mod common;

use common::*;
use sa_store::{
    Store,
    evaluation::{self, Assertion, Case, Dataset},
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;

fn hash(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}
fn fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    rusqlite::Connection,
    Job,
) {
    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    drop(Store::open(data.path()).unwrap());
    let writer = writer(data.path());
    let job = create_ticket(
        &writer,
        project.path().to_str().unwrap(),
        "PRIVATE-PROMPT-SECRET",
        "deepseek",
    );
    writer
        .execute(
            "UPDATE jobs SET state='done',result='PRIVATE-RESULT-SECRET' WHERE id=?",
            [job.id],
        )
        .unwrap();
    at(&writer, job, "turn.admitted", json!({}), 100.0);
    at(
        &writer,
        job,
        "reply",
        json!({"text":"PRIVATE-RESULT-SECRET"}),
        103.5,
    );
    (data, project, writer, job)
}
fn at(conn: &rusqlite::Connection, job: Job, kind: &str, value: serde_json::Value, time: f64) {
    let seq = log(conn, job, kind, &value);
    conn.execute(
        "UPDATE events SET ts=? WHERE seq=?",
        rusqlite::params![time, seq],
    )
    .unwrap();
}
fn dataset(assertions: Vec<Assertion>) -> Dataset {
    Dataset {
        schema_version: 1,
        id: "offline-tests".into(),
        version: "1".into(),
        cases: vec![Case {
            id: "case-a".into(),
            task: "PRIVATE-TASK-SPEC".into(),
            assertions,
        }],
    }
}

#[test]
fn actual_file_and_output_assertions_score_failures_and_export_no_content() {
    let (data, project, conn, job) = fixture();
    fs::write(project.path().join("answer.txt"), b"correct artifact").unwrap();
    let suite = dataset(vec![
        Assertion::RootState {
            equals: "done".into(),
        },
        Assertion::ResultSha256 {
            equals: hash(b"PRIVATE-RESULT-SECRET"),
        },
        Assertion::WorkspaceFileSha256 {
            path: "answer.txt".into(),
            equals: hash(b"correct artifact"),
        },
        Assertion::WorkspaceFileAbsent {
            path: "should-not-exist.txt".into(),
        },
    ]);
    let events_before: i64 = conn
        .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap();
    let report = evaluation::evaluate(data.path(), job.ticket, Some((&suite, "case-a"))).unwrap();
    assert_eq!(report.score.as_ref().unwrap().fraction, Some(1.0));
    assert_eq!(report.elapsed_ms, Some(3500));
    assert_eq!(report.usage.totals.total_tokens, None);
    assert!(report.usage.partial);
    assert_eq!(report.cost.amount, None);
    let exported = serde_json::to_string(&report).unwrap();
    for secret in [
        "PRIVATE-PROMPT-SECRET",
        "PRIVATE-RESULT-SECRET",
        "PRIVATE-TASK-SPEC",
        "answer.txt",
        project.path().to_str().unwrap(),
    ] {
        assert!(!exported.contains(secret), "leaked {secret}");
    }
    fs::write(project.path().join("answer.txt"), b"incorrect artifact").unwrap();
    let changed = evaluation::evaluate(data.path(), job.ticket, Some((&suite, "case-a"))).unwrap();
    assert_eq!(changed.score.as_ref().unwrap().fraction, Some(0.75));
    let comparison = evaluation::compare(&report, &changed).unwrap();
    assert_eq!(comparison.score_delta, Some(-0.25));
    assert_eq!(comparison.input_tokens_delta, None);
    assert_eq!(
        conn.query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        events_before
    );
}

#[test]
fn usage_reuses_native_deduplication_and_recovery_trace_is_per_turn() {
    let (data, _project, conn, job) = fixture();
    let usage = json!({"type":"status","phase":"step_end","turn":0,"step":0,"usage":{
        "inputTokens":10,"outputTokens":7,"cacheReadTokens":40,"cacheWriteTokens":5,"reasoningTokens":3,"totalTokens":62}});
    at(
        &conn,
        job,
        "native",
        json!({"type":"session","sessionId":"PRIVATE-NATIVE-ID"}),
        101.0,
    );
    at(&conn, job, "native", usage.clone(), 102.0);
    at(&conn, job, "native", usage, 102.1);
    let result = evaluation::evaluate(data.path(), job.ticket, None).unwrap();
    assert_eq!(result.usage.totals.input_tokens, Some(55));
    assert_eq!(result.usage.totals.output_tokens, Some(7));
    assert!(!result.usage.partial);
    assert!(
        !serde_json::to_string(&result)
            .unwrap()
            .contains("PRIVATE-NATIVE-ID")
    );
    let next = Job { turn: 2, ..job };
    at(&conn, next, "turn.admitted", json!({}), 110.0);
    at(
        &conn,
        next,
        "turn.retry_scheduled",
        json!({"secret":"DO-NOT-EXPORT"}),
        112.0,
    );
    let retried = evaluation::evaluate(data.path(), job.ticket, None).unwrap();
    assert_eq!(retried.recoveries, 1);
    assert_eq!(retried.jobs[0].turns.len(), 2);
    assert_eq!(retried.jobs[0].turns[1].elapsed_ms, Some(2000));
    assert!(retried.usage.partial);
    assert_eq!(
        evaluation::compare(&result, &retried)
            .unwrap()
            .output_tokens_delta,
        None
    );
}

#[test]
fn incompatible_versions_case_or_assertions_are_not_comparable() {
    let (data, _project, _conn, job) = fixture();
    let suite = dataset(vec![Assertion::RootState {
        equals: "done".into(),
    }]);
    let baseline = evaluation::evaluate(data.path(), job.ticket, Some((&suite, "case-a"))).unwrap();
    let mut changed = suite.clone();
    changed.version = "2".into();
    let report = evaluation::evaluate(data.path(), job.ticket, Some((&changed, "case-a"))).unwrap();
    assert!(evaluation::compare(&baseline, &report).is_err());
    changed = suite.clone();
    changed.cases[0]
        .assertions
        .push(Assertion::MaxFailedJobs { maximum: 0 });
    let report = evaluation::evaluate(data.path(), job.ticket, Some((&changed, "case-a"))).unwrap();
    assert!(evaluation::compare(&baseline, &report).is_err());
    changed = suite;
    changed.cases[0].id = "different-case".into();
    let report =
        evaluation::evaluate(data.path(), job.ticket, Some((&changed, "different-case"))).unwrap();
    assert!(evaluation::compare(&baseline, &report).is_err());
}

#[test]
fn unsupported_file_evidence_is_unknown_and_export_never_overwrites() {
    let (data, project, conn, job) = fixture();
    fs::create_dir(project.path().join("directory")).unwrap();
    let suite = dataset(vec![Assertion::WorkspaceFileSha256 {
        path: "directory".into(),
        equals: hash(b"x"),
    }]);
    let report = evaluation::evaluate(data.path(), job.ticket, Some((&suite, "case-a"))).unwrap();
    assert_eq!(report.score.as_ref().unwrap().unknown, 1);
    assert_eq!(report.score.as_ref().unwrap().fraction, None);
    let output = data.path().join("report.json");
    evaluation::write_new(&output, &report).unwrap();
    let bytes = fs::read(&output).unwrap();
    assert!(evaluation::write_new(&output, &json!({"overwrite":true})).is_err());
    assert_eq!(fs::read(output).unwrap(), bytes);
    conn.execute("UPDATE jobs SET state='running' WHERE id=?", [job.id])
        .unwrap();
    assert_eq!(
        evaluation::evaluate(data.path(), job.ticket, None)
            .unwrap()
            .elapsed_ms,
        None
    );
    let bad = dataset(vec![Assertion::WorkspaceFileAbsent {
        path: "../escape".into(),
    }]);
    assert!(evaluation::evaluate(data.path(), job.ticket, Some((&bad, "case-a"))).is_err());
}

#[test]
fn custom_agent_names_are_redacted_and_missing_job_timing_stays_unknown() {
    let (data, _project, conn, job) = fixture();
    conn.execute(
        "UPDATE jobs SET agent='PRIVATE-CUSTOM-AGENT' WHERE id=?",
        [job.id],
    )
    .unwrap();
    conn.execute("INSERT INTO jobs(ticket,parent,agent,body,writing,depth,state) VALUES(?,?,'other','PRIVATE-BODY',0,1,'done')",[job.ticket,job.id]).unwrap();
    let report = evaluation::evaluate(data.path(), job.ticket, None).unwrap();
    assert_eq!(report.elapsed_ms, None);
    let text = serde_json::to_string(&report).unwrap();
    assert!(!text.contains("PRIVATE-CUSTOM-AGENT") && !text.contains("PRIVATE-BODY"));
    assert_eq!(report.jobs[0].agent, "other");
}
