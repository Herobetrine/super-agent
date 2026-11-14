mod common;
use common::*;
use serde_json::json;

#[test]
fn compaction_retains_branch_provenance_and_scm_delivery() {
    let data = tempfile::tempdir().unwrap();
    let packet = json!({"execution_directory":"project-worktree","isolated_writers":true,"checkpoint_context":{"source_ticket":1,"source_seq":42,"source_sha256":"abc","notice":"historical evidence","input":{"goal":"x".repeat(15000)}},"scm_feedback":[{"id":17,"summary":{"ci":"failing"}}],"scm_notice":"untrusted","goal":"z".repeat(15000)});
    let out = sa_engine::context::compact(packet.as_object().unwrap(), "guide", data.path(), 2048)
        .unwrap();
    assert_eq!(out["execution_directory"], "project-worktree");
    assert_eq!(out["checkpoint_context_ref"]["source_seq"], 42);
    assert_eq!(out["scm_feedback"][0]["id"], 17);
    let archived: serde_json::Value = serde_json::from_slice(
        &std::fs::read(out["full_context"]["object_path"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(archived, packet);
    assert!(serde_json::to_vec(&out).unwrap().len() + 6 <= 2048);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fork_uses_immutable_evidence_and_a_fresh_native_session_without_replaying_effects() {
    let mut fx = Fx::new(
        script(|c| async move {
            if c.packet.get("checkpoint_context").is_some() {
                assert_eq!(c.req.session_id, None);
                assert!(!c.req.prompt.contains("session-1"));
                assert!(c.packet["checkpoint_context"]["reply"].get("session").is_none());
                assert_eq!(
                    c.packet["checkpoint_context"]["reply"]["text"],
                    "old result"
                );
                assert_eq!(c.packet["checkpoint_context"]["input"]["goal"], "fixture");
                assert_eq!(c.packet["reports"], json!([]));
                Ok("alternative result".into())
            } else {
                Ok("old result".into())
            }
        }),
        t1_catalog(),
    );
    let source = fx.begin(fx.params()).await;
    fx.finish(source, "done").await;
    let checkpoints = fx.engine.checkpoints(source).unwrap();
    let seq = checkpoints[0]["seq"].as_i64().unwrap();
    let fork = fx
        .engine
        .fork_checkpoint(source, seq, "try a different approach".into())
        .await
        .unwrap();
    assert_ne!(fork, source);
    fx.restart(quiet()).await;
    fx.engine.start().unwrap();
    fx.finish(fork, "done").await;
    assert_eq!(fx.engine.root(source).unwrap()["result"], "old result");
    assert_eq!(
        fx.engine.root(fork).unwrap()["result"],
        "alternative result"
    );
    assert!(
        fx.engine
            .fork_checkpoint(fork, seq, "invalid cross-ticket".into())
            .await
            .is_err()
    );
    let before = fx.int("SELECT count(*) FROM tickets");
    fx.engine
        .db()
        .with(|q| {
            q.run(
                "UPDATE events SET payload='\"tampered\"' WHERE seq=?",
                &[&seq],
            )?;
            Ok(())
        })
        .unwrap();
    assert!(
        fx.engine
            .fork_checkpoint(source, seq, "tampered".into())
            .await
            .is_err()
    );
    assert_eq!(before, fx.int("SELECT count(*) FROM tickets"));
}
