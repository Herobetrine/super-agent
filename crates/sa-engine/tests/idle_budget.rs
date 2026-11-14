//! Waiting for a peer must not spend model calls to keep a cache alive.
mod common;

use common::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_parent_spends_no_calls_until_peer_finishes() {
    let release = Gate::new();
    let child_release = release.clone();
    let fx = Fx::new(
        script(move |c| {
            let release = child_release.clone();
            async move {
                if c.depth() == 0 && c.turn() == 1 {
                    return Ok(send(json!({"body":"Bounded check; return the final evidence."})));
                }
                if c.depth() > 0 {
                    release.wait().await;
                    return Ok("peer evidence".into());
                }
                assert!(c.req.session_id.is_some());
                assert_eq!(c.packet["reports"][0]["result"], "peer evidence");
                Ok("reviewed".into())
            }
        }),
        t1_catalog(),
    );
    let ticket = fx.begin(fx.params()).await;
    fx.until(|| fx.shared.count(|_| true) == 2).await;
    assert_eq!(root_state(&fx.engine, ticket), "waiting");
    for _ in 0..256 {
        fx.engine.step();
        tokio::task::yield_now().await;
    }
    assert_eq!(fx.shared.count(|c| c.packet["depth"] == 0), 1);
    release.open();
    fx.finish(ticket, "done").await;
    assert_eq!(fx.shared.count(|c| c.packet["depth"] == 0), 2);
    fx.engine.close().await;
}
