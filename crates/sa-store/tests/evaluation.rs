mod common;

use common::*;
use sa_store::{
    Store,
    evaluation::{self, Assertion, Case, Dataset, EvidenceSource, JournalScope, Report},
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

#[test]
fn advisor_exports_redact_custom_agent_names_and_native_session_ids() {
    let (data, _project, conn, job) = fixture();
    let custom_seq = log(
        &conn,
        job,
        "advice.started",
        &json!({
            "advisor":{"agent":"PRIVATE-CUSTOM-ADVISOR"},
            "sessionId":"PRIVATE-START-SESSION",
        }),
    );
    let native_seq = log(
        &conn,
        job,
        "advice.started",
        &json!({
            "advisor":{"agent":"deepseek"},
        }),
    );
    let attempt_id = format!("advice:{native_seq}");
    let sample = sa_store::usage::advisor_usage_sample(
        "deepseek",
        &json!({
            "type":"session", "sessionId":"PRIVATE-ADVISOR-NATIVE-SESSION",
        }),
    )
    .unwrap();
    log(
        &conn,
        job,
        "usage.observed",
        &json!({
            "schema_version":1, "role":"advisor", "attempt_id":attempt_id,
            "agent":"deepseek", "sample":sample,
        }),
    );
    let report = evaluation::evaluate(data.path(), job.ticket, None).unwrap();
    let custom_id = format!("advice:{custom_seq}");
    let custom = report
        .usage
        .advisor_calls
        .iter()
        .find(|call| call.attempt_id == custom_id)
        .unwrap();
    assert_eq!(custom.agent, "other");
    let native = report
        .usage
        .advisor_calls
        .iter()
        .find(|call| call.attempt_id == attempt_id)
        .unwrap();
    assert_eq!(native.agent, "deepseek");
    let exported = serde_json::to_string(&report).unwrap();
    for secret in [
        "PRIVATE-CUSTOM-ADVISOR",
        "PRIVATE-START-SESSION",
        "PRIVATE-ADVISOR-NATIVE-SESSION",
    ] {
        assert!(!exported.contains(secret), "leaked {secret}");
    }
}

fn snapshot(data: &std::path::Path, conn: &rusqlite::Connection, ticket: i64, id: i64) {
    use sa_store::delivery::{self, DeliveryFile, DeliverySnapshot};
    conn.execute_batch(delivery::SCHEMA).unwrap();
    let bytes = b"frozen artifact";
    let digest = hash(bytes);
    fs::create_dir_all(data.join(delivery::OBJECT_DIRECTORY)).unwrap();
    fs::write(data.join(delivery::OBJECT_DIRECTORY).join(&digest), bytes).unwrap();
    let frozen = DeliverySnapshot {
        schema_version: 1,
        id,
        ticket,
        contract_revision: 0,
        created_at: 90.0,
        files: vec![DeliveryFile {
            path: "PRIVATE-ARTIFACT.txt".into(),
            sha256: digest,
            bytes: bytes.len() as u64,
        }],
        requirements: vec![],
        evidence: vec![],
        origin: None,
    };
    let payload = serde_json::to_string(&frozen).unwrap();
    conn.execute(
        "INSERT INTO delivery_snapshots(id,ticket,contract_revision,created,payload,sha256) VALUES(?,?,0,90,?,?)",
        rusqlite::params![id, ticket, payload, hash(payload.as_bytes())],
    ).unwrap();
}

#[test]
fn historical_files_stay_frozen_and_journal_assertions_are_explicitly_current() {
    let (data, project, conn, job) = fixture();
    snapshot(data.path(), &conn, job.ticket, 71);
    fs::write(
        project.path().join("PRIVATE-ARTIFACT.txt"),
        b"changed workspace",
    )
    .unwrap();
    conn.execute(
        "UPDATE jobs SET state='failed', result='new result' WHERE id=?",
        [job.id],
    )
    .unwrap();
    at(&conn, job, "failed", json!({}), 120.0);
    let suite = dataset(vec![
        Assertion::WorkspaceFileSha256 {
            path: "PRIVATE-ARTIFACT.txt".into(),
            equals: hash(b"frozen artifact"),
        },
        Assertion::RootState {
            equals: "failed".into(),
        },
        Assertion::ResultSha256 {
            equals: hash(b"new result"),
        },
        Assertion::MaxFailedJobs { maximum: 1 },
    ]);
    let source = EvidenceSource::DeliverySnapshot { snapshot_id: 71 };
    let historical = evaluation::evaluate_with_source(
        data.path(),
        job.ticket,
        Some((&suite, "case-a")),
        source.clone(),
    )
    .unwrap();
    assert_eq!(historical.schema_version, 2);
    assert_eq!(historical.usage_rules_version, sa_store::ledger::USAGE_RULES_VERSION);
    assert_eq!(
        historical.snapshot_manifest_sha256,
        Some(
            conn.query_row(
                "SELECT sha256 FROM delivery_snapshots WHERE id=71",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
        )
    );
    assert_eq!(historical.file_source, source);
    assert_eq!(
        historical.journal_scope,
        JournalScope::CurrentJournalThroughEvent
    );
    assert_eq!(
        historical.through_event,
        conn.query_row(
            "SELECT max(seq) FROM events WHERE ticket=?",
            [job.ticket],
            |row| row.get::<_, i64>(0)
        )
        .unwrap()
    );
    assert_eq!(historical.score.as_ref().unwrap().fraction, Some(1.0));
    assert_eq!(
        historical.score.as_ref().unwrap().assertions[0].evidence,
        "delivery_snapshot"
    );
    let current = evaluation::evaluate(data.path(), job.ticket, Some((&suite, "case-a"))).unwrap();
    assert_eq!(current.score.as_ref().unwrap().fraction, Some(0.75));
    assert!(evaluation::compare(&historical, &current).is_err());
    snapshot(data.path(), &conn, job.ticket, 72);
    let second = evaluation::evaluate_with_source(
        data.path(),
        job.ticket,
        Some((&suite, "case-a")),
        EvidenceSource::DeliverySnapshot { snapshot_id: 72 },
    )
    .unwrap();
    let comparison = evaluation::compare(&historical, &second).unwrap();
    assert_eq!(comparison.score_delta, Some(0.0));
    assert_eq!(comparison.left_file_source, source);
    assert_eq!(comparison.right_file_source, second.file_source);
    let exported = serde_json::to_string(&historical).unwrap();
    for secret in [
        "PRIVATE-ARTIFACT.txt",
        "frozen artifact",
        "new result",
        project.path().to_str().unwrap(),
    ] {
        assert!(!exported.contains(secret), "leaked {secret}");
    }
    assert!(exported.contains("71"));
}

#[test]
fn snapshot_missing_corrupt_and_unlisted_objects_are_unknown() {
    let (data, _project, conn, job) = fixture();
    snapshot(data.path(), &conn, job.ticket, 81);
    let suite = dataset(vec![
        Assertion::WorkspaceFileSha256 {
            path: "PRIVATE-ARTIFACT.txt".into(),
            equals: hash(b"frozen artifact"),
        },
        Assertion::WorkspaceFileAbsent {
            path: "unlisted.txt".into(),
        },
        Assertion::WorkspaceFileSha256 {
            path: "unlisted.txt".into(),
            equals: hash(b"x"),
        },
        Assertion::WorkspaceFileAbsent {
            path: "PRIVATE-ARTIFACT.txt".into(),
        },
    ]);
    let evaluate = || {
        evaluation::evaluate_with_source(
            data.path(),
            job.ticket,
            Some((&suite, "case-a")),
            EvidenceSource::DeliverySnapshot { snapshot_id: 81 },
        )
        .unwrap()
    };
    let initial = evaluate();
    let score = initial.score.unwrap();
    assert_eq!((score.passed, score.failed, score.unknown), (1, 0, 3));
    let object = data
        .path()
        .join(sa_store::delivery::OBJECT_DIRECTORY)
        .join(hash(b"frozen artifact"));
    fs::write(&object, b"damaged object").unwrap();
    assert_eq!(evaluate().score.unwrap().unknown, 4);
    fs::remove_file(object).unwrap();
    let missing = evaluate();
    assert_eq!(missing.score.as_ref().unwrap().unknown, 4);
    assert_eq!(missing.score.unwrap().fraction, None);
}

#[test]
fn snapshot_selection_rejects_wrong_ticket_and_unused_selection_and_reads_legacy_reports() {
    let (data, project, conn, job) = fixture();
    snapshot(data.path(), &conn, job.ticket, 91);
    let other = create_ticket(&conn, project.path().to_str().unwrap(), "other", "deepseek");
    let suite = dataset(vec![Assertion::WorkspaceFileAbsent {
        path: "unlisted.txt".into(),
    }]);
    let source = EvidenceSource::DeliverySnapshot { snapshot_id: 91 };
    assert!(
        evaluation::evaluate_with_source(
            data.path(),
            other.ticket,
            Some((&suite, "case-a")),
            source.clone()
        )
        .is_err()
    );
    assert!(
        evaluation::evaluate_with_source(data.path(), job.ticket, None, source.clone()).is_err()
    );
    let no_files = dataset(vec![Assertion::RootState {
        equals: "done".into(),
    }]);
    assert!(
        evaluation::evaluate_with_source(
            data.path(),
            job.ticket,
            Some((&no_files, "case-a")),
            source
        )
        .is_err()
    );
    let current = evaluation::evaluate(data.path(), job.ticket, None).unwrap();
    let mut legacy = serde_json::to_value(&current).unwrap();
    legacy.as_object_mut().unwrap().remove("file_source");
    legacy.as_object_mut().unwrap().remove("journal_scope");
    legacy
        .as_object_mut()
        .unwrap()
        .remove("usage_rules_version");
    legacy
        .as_object_mut()
        .unwrap()
        .remove("snapshot_manifest_sha256");
    let restored: Report = serde_json::from_value(legacy).unwrap();
    assert_eq!(restored.file_source, EvidenceSource::CurrentWorkspace);
    assert_eq!(restored.usage_rules_version, 1);
    assert_eq!(restored.snapshot_manifest_sha256, None);
    assert_eq!(
        restored.journal_scope,
        JournalScope::CurrentJournalThroughEvent
    );
    assert!(evaluation::compare(&current, &restored).is_ok());
}

#[test]
fn different_usage_rules_leave_token_deltas_unknown() {
    let (data, _project, conn, job) = fixture();
    at(
        &conn,
        job,
        "native",
        json!({"type":"status","phase":"step_end","turn":0,"step":0,"usage":{
        "inputTokens":10,"outputTokens":7,"totalTokens":17}}),
        102.0,
    );
    let current = evaluation::evaluate(data.path(), job.ticket, None).unwrap();
    assert!(!current.usage.partial);
    let mut old = current.clone();
    old.usage_rules_version = 1;
    old.usage.totals.input_tokens = Some(1);
    old.usage.totals.output_tokens = Some(2);
    let compared = evaluation::compare(&old, &current).unwrap();
    assert_eq!(compared.left_usage_rules_version, 1);
    assert_eq!(compared.right_usage_rules_version, sa_store::ledger::USAGE_RULES_VERSION);
    assert_eq!(compared.input_tokens_delta, None);
    assert_eq!(compared.output_tokens_delta, None);
    assert_eq!(
        evaluation::compare(&current, &current)
            .unwrap()
            .input_tokens_delta,
        Some(0)
    );
}

#[test]
fn missing_schema_snapshot_and_corrupt_manifest_do_not_fall_back_to_workspace() {
    let (data, project, conn, job) = fixture();
    fs::write(
        project.path().join("PRIVATE-ARTIFACT.txt"),
        b"frozen artifact",
    )
    .unwrap();
    let suite = dataset(vec![Assertion::WorkspaceFileSha256 {
        path: "PRIVATE-ARTIFACT.txt".into(),
        equals: hash(b"frozen artifact"),
    }]);
    let evaluate = || {
        evaluation::evaluate_with_source(
            data.path(),
            job.ticket,
            Some((&suite, "case-a")),
            EvidenceSource::DeliverySnapshot { snapshot_id: 101 },
        )
        .unwrap()
    };
    conn.execute_batch("DROP TABLE IF EXISTS delivery_snapshots")
        .unwrap();
    let legacy = evaluate();
    assert_eq!(legacy.score.unwrap().unknown, 1);
    assert_eq!(legacy.snapshot_manifest_sha256, None);
    conn.execute_batch(sa_store::delivery::SCHEMA).unwrap();
    assert_eq!(evaluate().score.unwrap().unknown, 1);
    snapshot(data.path(), &conn, job.ticket, 101);
    let intact = evaluate();
    assert_eq!(intact.score.unwrap().passed, 1);
    assert!(intact.snapshot_manifest_sha256.is_some());
    conn.execute(
        "UPDATE delivery_snapshots SET payload='{}' WHERE id=101",
        [],
    )
    .unwrap();
    let corrupt = evaluate();
    assert_eq!(corrupt.score.unwrap().unknown, 1);
    assert_eq!(corrupt.snapshot_manifest_sha256, None);
}
