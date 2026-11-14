//! Feedback is a durable input fact, not an effect to execute exactly once.
//! Failed native turns must not lose it before a successful reply commits.
mod common;

use common::*;
use sa_engine::{
    adapter::{AdapterError, ErrorClass},
    scm,
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_turn_keeps_feedback_for_retry_and_successful_reply_consumes_it() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let release = Gate::new();
    let seen = attempts.clone();
    let wait = release.clone();
    let mut fx = Fx::new(
        script(move |c| {
            let attempt = seen.fetch_add(1, Ordering::SeqCst);
            let wait = wait.clone();
            async move {
                assert_eq!(c.packet["scm_feedback"][0]["summary"]["ci"], "failing");
                assert_eq!(
                    c.packet["scm_feedback"][0]["summary"]["head"],
                    "a".repeat(40)
                );
                if attempt == 0 {
                    return Err(AdapterError::new(
                        ErrorClass::Protocol,
                        "fixture failed before producing a reply",
                    ));
                }
                wait.wait().await;
                Ok("feedback reviewed".into())
            }
        }),
        t1_catalog(),
    );
    let ticket = fx.create(fx.params()).await;
    let job = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    let id=fx.engine.db().tx(|q|{
        scm::bind(q,job,"https://github.com/example/project/pull/42")?;
        assert!(scm::apply(q,job,&json!({
            "url":"https://github.com/example/project/pull/42","headRefOid":"a".repeat(40),
            "state":"OPEN","statusCheckRollup":[{"name":"test","conclusion":"FAILURE"}],"latestReviews":[]
        }))?);
        Ok(scm::context(q,job)?[0]["id"].as_i64().unwrap())
    }).unwrap();
    fx.engine.start().unwrap();
    fx.finish(ticket, "failed").await;
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        fx.int("SELECT count(*) FROM scm_observations WHERE consumed_turn IS NULL"),
        1
    );
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='input'"), 1);
    // Restart verifies the retry uses the pending journal observation, not
    // an in-memory prompt retained from the failed native invocation.
    fx.restart(quiet()).await;
    fx.engine.start().unwrap();
    fx.engine.retry(job).unwrap();
    fx.until(|| attempts.load(Ordering::SeqCst) == 2).await;
    assert_eq!(
        fx.int("SELECT count(*) FROM scm_observations WHERE consumed_turn IS NULL"),
        1
    );
    let packets = fx.shared.calls(|c| Some(c.packet.clone()));
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0]["scm_feedback"][0]["id"], id);
    assert_eq!(packets[1]["scm_feedback"], packets[0]["scm_feedback"]);
    release.open();
    fx.finish(ticket, "done").await;
    assert_eq!(
        fx.int("SELECT count(*) FROM scm_observations WHERE consumed_turn IS NULL"),
        0
    );
    assert!(
        fx.engine
            .db()
            .with(|q| scm::context(q, job))
            .unwrap()
            .is_empty()
    );
    assert_eq!(fx.int("SELECT count(*) FROM scm_observations"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='reply'"), 1);
}
