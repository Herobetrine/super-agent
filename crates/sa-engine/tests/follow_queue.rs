mod common;

use common::*;
use sa_engine::db::who_of;
use sa_engine::resilience::classify;
use sa_engine::{FollowOutcome, HoldReason, NotQueued, Promotion};
use sa_store::{Snapshot, Store};
use serde_json::{Value, json};

fn fixture() -> Fx {
    Fx::new(script(|_| async { Ok("reply".into()) }), t1_catalog())
}

fn snapshot(fx: &Fx, ticket: i64) -> Snapshot {
    Store::open_read_only(&fx.data)
        .unwrap()
        .snapshot(ticket, 0)
        .unwrap()
}

fn enqueue(fx: &Fx, ticket: i64, body: &str) -> i64 {
    match fx.engine.follow(ticket, body, false).unwrap() {
        FollowOutcome::Queued { id } => id,
        other => panic!("expected queued follow, got {other:?}"),
    }
}

fn updates(fx: &Fx) -> Vec<Value> {
    payloads(
        fx,
        "SELECT payload FROM events WHERE kind='user.follow' ORDER BY seq",
    )
}

// Construct the persisted state of a worker without starting a scheduler.
// This lets restart and CAS tests choose an exact crash/acknowledgment boundary.
fn state(fx: &Fx, ticket: i64, next: &str) {
    fx.engine
        .db()
        .tx(|q| {
            q.run(
                "UPDATE jobs SET state=? WHERE ticket=? AND parent IS NULL",
                &[&next, &ticket],
            )
            .map(|_| ())
        })
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn running_follows_are_private_until_each_committed_reply_delivers_one() {
    let gates = [Gate::new(), Gate::new()];
    let fx = Fx::new(
        script({
            let gates = gates.clone();
            move |c| {
                let gates = gates.clone();
                async move {
                    let expected = match c.turn() {
                        1 => json!([]),
                        2 => json!(["first private instruction"]),
                        3 => json!(["first private instruction", "second private instruction"]),
                        turn => panic!("unexpected turn {turn}"),
                    };
                    assert_eq!(c.full_packet()["user_updates"], expected);
                    if let Some(gate) = gates.get((c.turn() - 1) as usize) {
                        gate.wait().await;
                    }
                    Ok("reply".into())
                }
            }
        }),
        t1_catalog(),
    );
    let t = fx.begin(fx.params()).await;
    fx.until(|| fx.shared.count(|_| true) == 1).await;
    let first = enqueue(&fx, t, "first private instruction");
    let second = enqueue(&fx, t, "second private instruction");
    let snap = snapshot(&fx, t);
    assert_eq!(
        snap.queue.iter().map(|i| i.id).collect::<Vec<_>>(),
        [first, second]
    );
    assert!(snap.queue_hold.is_none());
    assert!(
        !serde_json::to_string(&snap.events)
            .unwrap()
            .contains("private instruction")
    );
    fx.engine
        .db()
        .with(|q| {
            let job = sa_engine::job::JobCtx::from_row(&q.root(t)?);
            let packet = sa_engine::job::envelope(q, &job, json!({}))?;
            assert!(!serde_json::to_string(&packet)?.contains("private instruction"));
            Ok(())
        })
        .unwrap();
    gates[0].open();
    fx.until(|| fx.shared.count(|_| true) == 2).await;
    let snap = snapshot(&fx, t);
    assert_eq!(snap.queue.len(), 1);
    assert_eq!(snap.queue[0].id, second);
    let visible = serde_json::to_string(&snap.events).unwrap();
    assert!(visible.contains("first private instruction"));
    assert!(!visible.contains("second private instruction"));
    gates[1].open();
    fx.finish(t, "done").await;
    assert!(snapshot(&fx, t).queue.is_empty());
    let kinds: Vec<_> = fx
        .all("SELECT kind FROM events WHERE kind IN ('reply','user.follow') ORDER BY seq")
        .into_iter()
        .map(|r| r["kind"].clone())
        .collect();
    assert_eq!(
        json!(kinds),
        json!(["reply", "user.follow", "reply", "user.follow", "reply"])
    );
}

#[tokio::test]
async fn compare_and_set_and_native_note_ack_are_scoped_to_the_item() {
    for agent in ["codex", "claude"] {
        let fx = fixture();
        let t = fx.create(fx.params().lead(agent)).await;
        state(&fx, t, "running");
        let first = enqueue(&fx, t, "one");
        let second = enqueue(&fx, t, "two");
        let extra = enqueue(&fx, t, "delete");
        fx.engine.edit_queued(t, second, "edited").unwrap();
        fx.engine.move_queued(t, second, Some(first)).unwrap();
        fx.engine.move_queued(t, first, None).unwrap();
        let ids = |fx: &Fx| {
            snapshot(fx, t)
                .queue
                .into_iter()
                .map(|i| i.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&fx), [second, extra, first]);
        fx.engine.delete_queued(t, extra).unwrap();
        assert_eq!(
            fx.engine.promote_queued(t, second).unwrap(),
            Promotion::Steered
        );
        let snap = snapshot(&fx, t);
        assert!(snap.queue[0].steering);
        assert!(
            !serde_json::to_string(&snap.events)
                .unwrap()
                .contains("edited")
        );
        let before = snap.queue;
        for err in [
            fx.engine.delete_queued(t, second).unwrap_err(),
            fx.engine.move_queued(t, second, None).unwrap_err(),
            fx.engine.edit_queued(t, second, "late").unwrap_err(),
            fx.engine.move_queued(t, first, Some(second)).unwrap_err(),
            fx.engine.edit_queued(t + 1, first, "foreign").unwrap_err(),
        ] {
            assert!(err.is::<NotQueued>());
            assert_eq!(err.to_string(), "这条已开始发送，无法删除");
        }
        assert_eq!(snapshot(&fx, t).queue, before);
        fx.engine
            .db()
            .tx(|q| {
                let who = who_of(&q.root(t)?);
                let notes = q.notes(who.id, true)?;
                assert_eq!(notes.len(), 1);
                assert_eq!(notes[0]["body"], "edited");
                let job = sa_engine::job::JobCtx::from_row(&q.root(t)?);
                assert_eq!(
                    sa_engine::job::envelope(q, &job, json!({}))?["notes"],
                    json!([])
                );
                q.accept(who, &[notes[0]["seq"].as_i64().unwrap()])
            })
            .unwrap();
        assert_eq!(ids(&fx), [first]);
        assert!(
            serde_json::to_string(&snapshot(&fx, t).events)
                .unwrap()
                .contains("edited")
        );
        assert!(
            fx.engine
                .delete_queued(t, second)
                .unwrap_err()
                .is::<NotQueued>()
        );
        assert_eq!(
            fx.int("SELECT count(*) FROM events WHERE kind='note.accepted'"),
            1
        );
        assert!(updates(&fx).is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_or_waiting_reply_delivers_head_and_withdraws_the_question() {
    for ending in ["partial", "waiting"] {
        let gate = Gate::new();
        let fx = Fx::new(
            script({
                let gate = gate.clone();
                move |c| {
                    let gate = gate.clone();
                    async move {
                        if c.turn() == 1 {
                            gate.wait().await;
                            return Ok(if ending == "waiting" {
                                wire(
                                    "SA_ASK",
                                    json!({"id":"scope","question":"范围？","options":["A","B"]}),
                                )
                            } else {
                                wire(
                                    "SA_REPORT",
                                    json!({"status":"partial","summary":"first check"}),
                                )
                            });
                        }
                        let packet = c.full_packet();
                        assert_eq!(packet["user_updates"], json!(["new scope"]));
                        assert!(packet.get("open_question").is_none());
                        Ok("done".into())
                    }
                }
            }),
            t1_catalog(),
        );
        let t = fx.begin(fx.params()).await;
        fx.until(|| fx.shared.count(|_| true) == 1).await;
        enqueue(&fx, t, "new scope");
        gate.open();
        fx.finish(t, "done").await;
        let replies = payloads(
            &fx,
            "SELECT payload FROM events WHERE kind='reply' ORDER BY seq",
        );
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0]["state"], ending);
        assert_eq!(
            fx.int("SELECT count(*) FROM questions WHERE state='withdrawn'"),
            i64::from(ending == "waiting")
        );
        assert!(snapshot(&fx, t).queue.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_delivery_failure_rolls_back_the_reply_transaction() {
    let gate = Gate::new();
    let fx = Fx::new(
        script({
            let gate = gate.clone();
            move |_| {
                let gate = gate.clone();
                async move {
                    gate.wait().await;
                    Ok("final reply".into())
                }
            }
        }),
        t1_catalog(),
    );
    let t = fx.begin(fx.params()).await;
    fx.until(|| fx.shared.count(|_| true) == 1).await;
    let id = enqueue(&fx, t, "head");
    fx.engine
        .db()
        .tx(|q| {
            q.conn().execute_batch(
                "CREATE TRIGGER refuse_consume BEFORE DELETE ON follow_queue
            BEGIN SELECT RAISE(ABORT,'injected queue failure'); END",
            )?;
            Ok(())
        })
        .unwrap();
    gate.open();
    fx.finish(t, "failed").await;
    assert_eq!(
        fx.int("SELECT count(*) FROM events WHERE kind IN ('reply','user.follow')"),
        0
    );
    assert_eq!(snapshot(&fx, t).queue[0].id, id);
    assert_eq!(snapshot(&fx, t).queue_hold, Some(HoldReason::Failed));
}

#[tokio::test]
async fn unsupported_steering_moves_to_head_and_queued_root_preserves_fifo() {
    for agent in ["deepseek", "antigravity", "acp"] {
        let fx = fixture();
        let t = fx.create(fx.params().lead(agent)).await;
        assert!(matches!(
            fx.engine.follow(t, "upcoming", false).unwrap(),
            FollowOutcome::Sent { .. }
        ));
        state(&fx, t, "running");
        let one = enqueue(&fx, t, "one");
        let two = enqueue(&fx, t, "two");
        assert_eq!(
            fx.engine.promote_queued(t, two).unwrap(),
            Promotion::NextTurn
        );
        state(&fx, t, "queued");
        let three = enqueue(&fx, t, "three");
        assert_eq!(
            snapshot(&fx, t)
                .queue
                .into_iter()
                .map(|i| i.id)
                .collect::<Vec<_>>(),
            [two, one, three]
        );
        assert_eq!(fx.int("SELECT count(*) FROM mailbox"), 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_note_delivers_the_immutable_attachment_and_is_not_replayed() {
    let gate = Gate::new();
    let mut fx = Fx::new(
        script({
            let gate = gate.clone();
            move |c| {
                let gate = gate.clone();
                async move {
                    gate.wait().await;
                    let notes = c.host.pending_notes();
                    assert_eq!(notes.len(), 1);
                    assert_eq!(notes[0]["body"], "inspect file");
                    let file = &notes[0]["files"][0];
                    assert_eq!(file["name"], "input.txt");
                    assert_eq!(
                        std::fs::read(file["object_path"].as_str().unwrap()).unwrap(),
                        b"original"
                    );
                    c.host.accept_notes(&[notes[0]["seq"].as_i64().unwrap()]);
                    Ok("inspected".into())
                }
            }
        }),
        t1_catalog(),
    );
    project_file(&fx.project, "input.txt", b"original");
    let t = fx.begin(fx.params()).await;
    fx.until(|| fx.shared.count(|_| true) == 1).await;
    let FollowOutcome::Queued { id } = fx
        .engine
        .follow_with_files(t, "inspect file", false, &["input.txt".into()])
        .await
        .unwrap()
    else {
        panic!("expected queued attachment")
    };
    assert_eq!(snapshot(&fx, t).queue[0].files, ["input.txt"]);
    project_file(&fx.project, "input.txt", b"changed");
    assert_eq!(fx.engine.promote_queued(t, id).unwrap(), Promotion::Steered);
    gate.open();
    fx.finish(t, "done").await;
    fx.restart(quiet()).await;
    assert!(snapshot(&fx, t).queue.is_empty());
    assert!(updates(&fx).is_empty());
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='note'"), 1);
    assert_eq!(
        fx.int("SELECT count(*) FROM mailbox WHERE consumed_turn=1"),
        1
    );
}

#[tokio::test]
async fn stopped_continuous_queue_persists_and_resume_is_explicit_authorization() {
    let mut fx = fixture();
    let t = fx.create(fx.params().profile("auto")).await;
    state(&fx, t, "running");
    enqueue(&fx, t, "one");
    let two = enqueue(&fx, t, "two");
    fx.engine.cancel(t).unwrap();
    fx.restart(quiet()).await;
    let snap = snapshot(&fx, t);
    assert_eq!(snap.queue_hold, Some(HoldReason::Stopped));
    assert_eq!(snap.queue.len(), 2);
    assert_eq!(
        fx.engine
            .follow(t, "ordinary", false)
            .unwrap_err()
            .to_string(),
        "停止的工单需要明确恢复"
    );
    fx.engine.resume_queue(t).unwrap();
    assert_eq!(updates(&fx), [json!("one")]);
    let snap = snapshot(&fx, t);
    assert_eq!(snap.ticket.state, "queued");
    assert_eq!(snap.queue_hold, None);
    assert_eq!(snap.queue.len(), 1);
    assert_eq!(snap.queue[0].id, two);
    fx.restart(quiet()).await;
    assert_eq!(updates(&fx), [json!("one")]);
}

#[tokio::test]
async fn plain_follow_on_held_backlog_does_not_resume_it() {
    let fx = fixture();
    let t = fx.create(fx.params()).await;
    state(&fx, t, "running");
    enqueue(&fx, t, "held");
    fx.engine.cancel(t).unwrap();
    for body in ["restart", "fold"] {
        assert!(matches!(
            fx.engine.follow(t, body, true).unwrap(),
            FollowOutcome::Sent { .. }
        ));
        assert_eq!(snapshot(&fx, t).queue_hold, Some(HoldReason::Stopped));
    }
    state(&fx, t, "running");
    enqueue(&fx, t, "also held");
    assert_eq!(snapshot(&fx, t).queue.len(), 2);
    let before = updates(&fx);
    fx.engine.resume_queue(t).unwrap();
    assert_eq!(snapshot(&fx, t).queue_hold, None);
    assert_eq!(updates(&fx), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permanent_failure_holds_and_resume_sends_the_first_item() {
    let gate = Gate::new();
    let fx = Fx::new(
        script({
            let gate = gate.clone();
            move |_| {
                let gate = gate.clone();
                async move {
                    gate.wait().await;
                    Err(classify::from_http_status(401, None))
                }
            }
        }),
        t1_catalog(),
    );
    let t = fx.begin(fx.params()).await;
    fx.until(|| fx.shared.count(|_| true) == 1).await;
    enqueue(&fx, t, "new work");
    gate.open();
    fx.finish(t, "failed").await;
    assert_eq!(snapshot(&fx, t).queue_hold, Some(HoldReason::Failed));
    assert!(updates(&fx).is_empty());
    fx.engine.resume_queue(t).unwrap();
    assert_eq!(updates(&fx), [json!("new work")]);
    assert!(snapshot(&fx, t).queue.is_empty());
    assert_eq!(root_state(&fx.engine, t), "queued");
}

#[tokio::test]
async fn interrupted_root_retries_the_same_turn_and_refusal_rolls_back_release() {
    let mut fx = fixture();
    let t = fx.create(fx.params()).await;
    state(&fx, t, "running");
    enqueue(&fx, t, "retry input");
    let later = enqueue(&fx, t, "later");
    fx.engine
        .db()
        .tx(|q| {
            let who = who_of(&q.root(t)?);
            let seq = q.log(who, "job.progress", &json!({"summary":"checked"}))?;
            q.run(
                "INSERT INTO progress_receipts(job,source_turn,msgkey,seq) VALUES(?,1,'receipt',?)",
                &[&who.id, &seq],
            )?;
            q.recover()
        })
        .unwrap();
    fx.restart(quiet()).await;
    let snap = snapshot(&fx, t);
    assert_eq!(snap.queue_hold, Some(HoldReason::Interrupted));
    assert_eq!(snap.ticket.state, "interrupted");
    assert_eq!(
        fx.engine
            .follow(t, "ordinary", true)
            .unwrap_err()
            .to_string(),
        "请先处理上级中断或失败"
    );
    fx.engine
        .db()
        .tx(|q| {
            q.run(
                "INSERT INTO job_workspaces(job,ticket,source,path,branch,state)
        SELECT id,ticket,'src','path','branch','merged' FROM jobs WHERE ticket=?",
                &[&t],
            )
        })
        .unwrap();
    let before = fx.int("SELECT max(seq) FROM events");
    assert_eq!(
        fx.engine.resume_queue(t).unwrap_err().to_string(),
        fx.engine.retry(snap.queue[0].job).unwrap_err().to_string()
    );
    assert_eq!(snapshot(&fx, t), snap);
    assert_eq!(fx.int("SELECT max(seq) FROM events"), before);
    fx.engine
        .db()
        .tx(|q| q.run("DELETE FROM job_workspaces WHERE ticket=?", &[&t]))
        .unwrap();
    fx.engine.resume_queue(t).unwrap();
    assert_eq!(fx.int("SELECT turn FROM jobs"), 1);
    assert_eq!(
        fx.int("SELECT count(*) FROM progress_receipts WHERE source_turn=1 AND msgkey='receipt'"),
        1
    );
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='retry'"), 1);
    assert_eq!(updates(&fx), [json!("retry input")]);
    let snap = snapshot(&fx, t);
    assert_eq!(snap.ticket.state, "queued");
    assert_eq!(snap.queue_hold, None);
    assert_eq!(snap.queue.len(), 1);
    assert_eq!(snap.queue[0].id, later);
}

#[tokio::test]
async fn resume_during_drain_waits_for_settlement() {
    let mut fx = fixture();
    let t = fx.create(fx.params()).await;
    state(&fx, t, "running");
    enqueue(&fx, t, "after children");
    fx.engine.cancel(t).unwrap();
    state(&fx, t, "draining");
    fx.engine.resume_queue(t).unwrap();
    assert_eq!(snapshot(&fx, t).queue_hold, None);
    assert_eq!(root_state(&fx.engine, t), "draining");
    fx.restart(quiet()).await;
    assert!(updates(&fx).is_empty());
    fx.engine.db().tx(|q| q.wake()).unwrap();
    assert_eq!(updates(&fx), [json!("after children")]);
    assert_eq!(root_state(&fx.engine, t), "queued");
}

#[tokio::test]
async fn open_restores_orphaned_notes_in_place_and_idle_recovery_is_idempotent() {
    let mut fx = fixture();
    let t = fx.create(fx.params()).await;
    state(&fx, t, "running");
    enqueue(&fx, t, "one");
    let second = enqueue(&fx, t, "two");
    let before = snapshot(&fx, t).queue;
    assert_eq!(
        fx.engine.promote_queued(t, second).unwrap(),
        Promotion::Steered
    );
    fx.restart(quiet()).await;
    assert_eq!(snapshot(&fx, t).queue, before);
    assert!(fx.engine.notes(before[0].job).unwrap().is_empty());
    state(&fx, t, "done");
    fx.restart(quiet()).await;
    assert_eq!(updates(&fx), [json!("one")]);
    assert_eq!(snapshot(&fx, t).queue, before[1..]);
    fx.restart(quiet()).await;
    assert_eq!(updates(&fx), [json!("one")]);
    assert_eq!(snapshot(&fx, t).queue, before[1..]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quota_and_route_waits_keep_the_queue_across_reopen() {
    use sa_engine::resilience::ResilienceConfig;
    use sa_engine::runner::ResilientRunner;
    use std::{sync::Arc, time::Duration};
    for kind in ["turn.quota_wait", "turn.route_wait"] {
        let gate = Gate::new();
        let opts = if kind == "turn.route_wait" {
            sa_engine::EngineOptions {
                runner: Arc::new(ResilientRunner::new(ResilienceConfig {
                    breaker_threshold: 1,
                    ..Default::default()
                })),
                ..quiet()
            }
        } else {
            quiet()
        };
        let mut fx = Fx::with(
            script({
                let gate = gate.clone();
                move |_| {
                    let gate = gate.clone();
                    async move {
                        gate.wait().await;
                        Err(if kind == "turn.quota_wait" {
                            classify::quota("five_hour", Some(Duration::from_secs(60)))
                        } else {
                            classify::from_http_status(503, None)
                        })
                    }
                }
            }),
            t1_catalog(),
            opts,
        );
        let t = fx.begin(fx.params()).await;
        fx.until(|| fx.shared.count(|_| true) == 1).await;
        let head = enqueue(&fx, t, "after wait");
        gate.open();
        fx.until(|| fx.int(&format!("SELECT count(*) FROM events WHERE kind='{kind}'")) == 1)
            .await;
        fx.restart(quiet()).await;
        let snap = snapshot(&fx, t);
        assert_eq!(snap.ticket.state, "queued");
        assert_eq!(snap.queue.len(), 1);
        assert_eq!(snap.queue[0].id, head);
        assert_eq!(snap.queue_hold, None);
        assert!(updates(&fx).is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_and_reopen_do_not_consume_or_hold_the_head() {
    let gate = Gate::new();
    let mut fx = Fx::new(
        script({
            let gate = gate.clone();
            move |c| {
                let gate = gate.clone();
                async move {
                    if c.turn() == 1 {
                        gate.wait().await;
                        return Err(classify::from_http_status(503, None));
                    }
                    Ok("recovered".into())
                }
            }
        }),
        t1_catalog(),
    );
    let t = fx.begin(fx.params()).await;
    fx.until(|| fx.shared.count(|_| true) == 1).await;
    enqueue(&fx, t, "after success");
    gate.open();
    fx.until(|| fx.int("SELECT count(*) FROM events WHERE kind='turn.retry_scheduled'") == 1)
        .await;
    fx.restart(quiet()).await;
    let snap = snapshot(&fx, t);
    assert_eq!(snap.queue_hold, None);
    assert_eq!(snap.queue.len(), 1);
    assert_eq!(snap.ticket.state, "queued");
    assert!(updates(&fx).is_empty());
    fx.engine
        .db()
        .tx(|q| q.run("UPDATE jobs SET ready_after=0 WHERE ticket=?", &[&t]))
        .unwrap();
    fx.engine.start().unwrap();
    fx.finish(t, "done").await;
    assert_eq!(updates(&fx), [json!("after success")]);
    assert_eq!(fx.shared.count(|_| true), 3);
}
