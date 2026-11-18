mod common;

use common::*;
use sa_engine::adapter::{AdapterError, ErrorClass, Route};
use sa_engine::db::Who;
use sa_store::Store;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn usage(total: u64, output: u64) -> Value {
    json!({"totalTokens":total,"inputTokens":total-output,"outputTokens":output,
        "cachedInputTokens":0,"cacheWriteInputTokens":0,"reasoningOutputTokens":0})
}

fn sample(total: u64, output: u64) -> Value {
    json!({"method":"thread/tokenUsage/updated","params":{"threadId":"same-native-id",
        "tokenUsage":{"total":usage(total, output),"last":usage(total, output)},
        "private":"PRIVATE_NATIVE_CONTENT"},"private":"PRIVATE_NATIVE_CONTENT"})
}

async fn fixture() -> (Fx, Who) {
    let fx = Fx::new(
        script(|c| async move {
            assert_eq!(c.req.profile, "readonly");
            assert!(!c.req.writable && c.req.bus.is_none() && c.req.session_id.is_none());
            if !c.req.prompt.ends_with("missing") {
                let mut frame = sample(12, 2);
                frame["params"]["threadId"] = json!(format!(
                    "advisor-{}",
                    c.req.prompt.rsplit('\n').next().unwrap()
                ));
                c.emit("native", frame.clone());
                c.emit("native", frame);
            }
            c.emit(
                "native",
                json!({"type":"tool_call","arguments":"PRIVATE_NATIVE_CONTENT"}),
            );
            c.emit("delta", json!("PRIVATE_NATIVE_CONTENT"));
            if c.req.prompt.ends_with("fail") {
                return Err(AdapterError::new(ErrorClass::Network, "synthetic failure"));
            }
            if c.req.prompt.ends_with("cancel") {
                c.cancel.cancel();
                return futures::future::pending().await;
            }
            Ok("checked advice".into())
        }),
        t1_catalog(),
    );
    let ticket = fx.create(fx.params()).await;
    let route = Route {
        provider: "test-local".into(),
        model: "review".into(),
        mode: "cli".into(),
        reasoning_effort: None,
    };
    fx.engine
        .set_advisor(Some(ticket), Some(("codex", &route)))
        .unwrap();
    let job = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    let who = Who {
        ticket,
        id: job,
        turn: 1,
    };
    fx.engine
        .db()
        .with(|q| {
            q.run("UPDATE jobs SET state='running',turn=1 WHERE id=?", &[&job])?;
            q.log(who, "turn.admitted", &json!({}))?;
            q.log(who, "native", &sample(120, 20))?;
            Ok(())
        })
        .unwrap();
    (fx, who)
}

fn projected(fx: &Fx, ticket: i64) -> sa_store::TicketUsage {
    let mut store = Store::open(&fx.data).unwrap();
    for _ in 0..100 {
        let snapshot = store.snapshot(ticket, 0).unwrap();
        if !snapshot.usage.pending {
            return snapshot.usage;
        }
    }
    panic!("usage scan did not complete");
}

#[tokio::test]
async fn advisor_terminal_usage_is_drained_and_calls_have_durable_separate_identities() {
    let (fx, who) = fixture().await;
    let first = fx
        .engine
        .consult(
            who.ticket,
            who.id,
            who.turn,
            "first",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let second = fx
        .engine
        .consult(
            who.ticket,
            who.id,
            who.turn,
            "second",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_ne!(first["attempt_id"], second["attempt_id"]);
    let result = projected(&fx, who.ticket);
    assert_eq!(result.totals.total_tokens, Some(144));
    assert_eq!(result.jobs[0].tokens.total_tokens, Some(120));
    assert_eq!(result.reported_advisor_calls, 2);
    assert_eq!(result.unreported_advisor_calls, 0);
    assert_eq!(result.advisor_calls.len(), 2);
    assert!(
        result
            .advisor_calls
            .iter()
            .all(|call| call.tokens.total_tokens == Some(12))
    );
    let rows = fx.all("SELECT kind,payload FROM events WHERE kind IN ('advice.started','advice.result','usage.observed')");
    for row in rows {
        assert!(
            !row["payload"]
                .as_str()
                .unwrap()
                .contains("PRIVATE_NATIVE_CONTENT")
        );
    }
    assert_eq!(
        fx.int("SELECT count(*) FROM events WHERE kind='usage.observed'"),
        4
    );
    assert_eq!(
        projected(&fx, who.ticket),
        result,
        "fresh projector rebuild is identical"
    );
    fx.engine.close().await;
}

#[tokio::test]
async fn failed_metering_write_keeps_advice_and_records_the_coverage_gap() {
    for retained_frames in [0, 1] {
        let (fx, who) = fixture().await;
        let trigger = format!(
            "CREATE TRIGGER reject_advisor_usage BEFORE INSERT ON events WHEN NEW.kind='usage.observed' AND (SELECT count(*) FROM events WHERE kind='usage.observed')>={retained_frames} BEGIN SELECT RAISE(FAIL,'synthetic metering failure'); END"
        );
        fx.engine
            .db()
            .with(|q| {
                q.run(&trigger, &[])?;
                Ok(())
            })
            .unwrap();
        let result = fx
            .engine
            .consult(
                who.ticket,
                who.id,
                who.turn,
                "first",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result["advice"], "checked advice");
        assert_eq!(result["usage_complete"], false);
        let result = projected(&fx, who.ticket);
        assert_eq!(result.totals.total_tokens, Some(120 + 12 * retained_frames));
        assert_eq!(result.reported_advisor_calls, retained_frames);
        assert_eq!(result.unreported_advisor_calls, 1 - retained_frames);
        assert!(result.partial && result.advisor_calls[0].partial);
        assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='advice.result' AND json_extract(payload,'$.usage_complete')=0"), 1);
        fx.engine.close().await;
    }
}

#[tokio::test]
async fn terminal_journal_failure_is_returned_and_keeps_recorded_usage_partial() {
    let (fx, who) = fixture().await;
    fx.engine.db().with(|q| {
        q.run("CREATE TRIGGER reject_advice_result BEFORE INSERT ON events WHEN NEW.kind='advice.result' BEGIN SELECT RAISE(FAIL,'synthetic terminal failure'); END", &[])?;
        Ok(())
    }).unwrap();
    let caller_cancel = CancellationToken::new();
    assert!(
        fx.engine
            .consult(who.ticket, who.id, who.turn, "first", caller_cancel.clone())
            .await
            .is_err()
    );
    assert!(!caller_cancel.is_cancelled());
    let result = projected(&fx, who.ticket);
    assert_eq!(result.totals.total_tokens, Some(132));
    assert!(result.advisor_calls[0].partial);
    assert_eq!(
        fx.int("SELECT count(*) FROM events WHERE kind='advice.result'"),
        0
    );
    fx.engine.close().await;
}

#[tokio::test]
async fn advisor_failure_and_cancellation_keep_already_reported_usage_and_missing_is_unknown() {
    let (fx, who) = fixture().await;
    for brief in ["fail", "cancel"] {
        assert!(
            fx.engine
                .consult(
                    who.ticket,
                    who.id,
                    who.turn,
                    brief,
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
    }
    fx.engine
        .consult(
            who.ticket,
            who.id,
            who.turn,
            "missing",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let result = projected(&fx, who.ticket);
    assert_eq!(result.totals.total_tokens, Some(144));
    assert_eq!(result.reported_advisor_calls, 2);
    assert_eq!(result.unreported_advisor_calls, 1);
    assert!(result.partial);
    assert!(result.advisor_calls.iter().all(|call| call.partial));
    assert_eq!(
        fx.int("SELECT count(*) FROM events WHERE kind='advice.result'"),
        3
    );
    assert_eq!(
        fx.int("SELECT count(*) FROM events WHERE kind='usage.observed'"),
        4
    );
    fx.engine.close().await;
}
