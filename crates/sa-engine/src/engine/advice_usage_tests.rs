use super::*;
use crate::adapter::{Catalog, EventSink, RunOutcome};

struct UsageThenWait;

impl Adapter for UsageThenWait {
    fn agent(&self) -> &'static str {
        "codex"
    }

    fn catalog(&self) -> BoxFuture<'static, Result<Catalog>> {
        Box::pin(async { Ok(Catalog::default()) })
    }

    fn run(
        &self,
        _request: RunRequest,
        _host: Arc<dyn TurnHost>,
        events: EventSink,
        _cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<RunOutcome, AdapterError>> {
        Box::pin(async move {
            let counts = json!({"totalTokens":12,"inputTokens":10,"outputTokens":2});
            events
                .send(AdapterEvent::Journal {
                    kind: "native".into(),
                    data: json!({
                "method":"thread/tokenUsage/updated","params":{"threadId":"timeout-advisor",
                "tokenUsage":{"total":counts,"last":counts}}}),
                })
                .unwrap();
            futures::future::pending().await
        })
    }
}

#[tokio::test]
async fn timeout_retains_usage_and_records_a_terminal_without_cancelling_the_caller() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let engine = Engine::open(
        dir.path(),
        vec![Arc::new(UsageThenWait)],
        EngineOptions::default(),
    )
    .unwrap();
    let ticket = engine
        .create(CreateParams::new("timeout fixture", &project).lead("codex"))
        .await
        .unwrap();
    let job = engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    engine
        .db()
        .with(|q| {
            q.run("UPDATE jobs SET state='running',turn=1 WHERE id=?", &[&job])?;
            Ok(())
        })
        .unwrap();
    let route = Route {
        provider: "test-local".into(),
        model: "fixture".into(),
        mode: "cli".into(),
        reasoning_effort: None,
    };
    engine
        .set_advisor(Some(ticket), Some(("codex", &route)))
        .unwrap();
    let caller_cancel = CancellationToken::new();
    let outcome = engine
        .consult_limited(
            ticket,
            job,
            1,
            "check timeout",
            caller_cancel.clone(),
            Duration::from_millis(20),
        )
        .await;
    assert!(outcome.is_err());
    assert!(!caller_cancel.is_cancelled());
    let mut store = sa_store::Store::open(dir.path()).unwrap();
    let usage = store.snapshot(ticket, 0).unwrap().usage;
    assert_eq!(usage.reported_advisor_calls, 1);
    assert_eq!(usage.advisor_calls[0].tokens.total_tokens, Some(12));
    assert!(usage.advisor_calls[0].partial);
    assert_eq!(
        engine
            .db()
            .with(|q| q.int(
                "SELECT count(*) FROM events WHERE kind='advice.result'",
                &[]
            ))
            .unwrap(),
        1
    );
    engine.close().await;
}
