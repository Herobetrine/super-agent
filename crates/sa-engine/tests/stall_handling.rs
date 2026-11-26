//! Repeated autonomous continuations require a durable operator decision.

mod common;

use common::*;
use serde_json::{Value, json};

fn continuation() -> String {
    wire(
        "SA_NEXT",
        json!({"body": "repeat the same unchanged next step"}),
    )
}

fn ask() -> String {
    wire(
        "SA_ASK",
        json!({
            "id": "after-resume", "question": "next scope?", "options": ["continue", "stop"], "default": "wait",
        }),
    )
}

async fn replied(fx: &Fx, ticket: i64, turn: i64) {
    // Queued jobs already carry their next turn number. Only a committed
    // reply proves that its backoff and platform question have been recorded.
    fx.until(|| {
        fx.int(&format!(
            "SELECT count(*) FROM events WHERE ticket={ticket} AND turn={turn} AND kind='reply'"
        )) == 1
            && fx.engine.active_count() == 0
    })
    .await;
}

async fn stalled(fx: &Fx, ticket: i64) -> Value {
    // Skip only the old backoff clock; the native turns and repeated-signal
    // accounting still execute through the real scheduler and commit path.
    for turn in 1..=4 {
        replied(fx, ticket, turn).await;
        if turn < 4 {
            fx.engine
                .db()
                .with(|q| q.run("UPDATE jobs SET ready_after=0 WHERE ticket=?", &[&ticket]))
                .unwrap();
        }
    }
    let questions = fx.engine.questions(ticket).unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0]["origin"], "platform");
    assert_eq!(questions[0]["parked"], 1);
    assert_eq!(root_state(&fx.engine, ticket), "waiting");
    assert_eq!(
        fx.int("SELECT count(*) FROM events WHERE kind='auto.stalled'"),
        1
    );
    assert_eq!(fx.shared.count(|_| true), 4);
    questions[0].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_follow_cannot_answer_the_platform_continue_or_stop_question() {
    let gate = Gate::new();
    let mut fx = Fx::new(
        script({
            let gate = gate.clone();
            move |c| {
                let gate = gate.clone();
                async move {
                    if c.turn() == 4 {
                        gate.wait().await;
                    }
                    Ok(continuation())
                }
            }
        }),
        t1_catalog(),
    );
    let ticket = fx.begin(fx.params().profile("auto")).await;
    for turn in 1..4 {
        replied(&fx, ticket, turn).await;
        fx.engine
            .db()
            .tx(|q| q.run("UPDATE jobs SET ready_after=0 WHERE ticket=?", &[&ticket]))
            .unwrap();
    }
    fx.until(|| fx.shared.count(|_| true) == 4).await;
    assert!(matches!(
        fx.engine.follow(ticket, "later work", false).unwrap(),
        sa_engine::FollowOutcome::Queued { .. }
    ));
    gate.open();
    replied(&fx, ticket, 4).await;
    let questions = fx.engine.questions(ticket).unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0]["origin"], "platform");
    fx.restart(quiet()).await;
    assert_eq!(fx.engine.questions(ticket).unwrap(), questions);
    assert_eq!(root_state(&fx.engine, ticket), "waiting");
    assert_eq!(fx.int("SELECT count(*) FROM follow_queue"), 1);
    assert_eq!(
        fx.int("SELECT count(*) FROM events WHERE kind='user.follow'"),
        0
    );
    fx.engine
        .answer(questions[0]["id"].as_i64().unwrap(), "停止")
        .unwrap();
    assert_eq!(root_state(&fx.engine, ticket), "cancelled");
    assert_eq!(fx.int("SELECT count(*) FROM follow_queue"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_unchanged_continuation_parks_across_scheduler_passes_and_restart() {
    let mut fx = Fx::new(script(|_| async { Ok(continuation()) }), t1_catalog());
    let ticket = fx.begin(fx.params().profile("auto")).await;
    let question = stalled(&fx, ticket).await;
    for _ in 0..32 {
        assert_eq!(fx.engine.step(), 0);
    }
    assert_eq!(fx.shared.count(|_| true), 4);
    fx.restart(quiet()).await;
    fx.engine.start().unwrap();
    for _ in 0..32 {
        assert_eq!(fx.engine.step(), 0);
    }
    assert_eq!(fx.shared.count(|_| true), 4);
    assert_eq!(
        fx.engine.questions(ticket).unwrap()[0]["id"],
        question["id"]
    );
    assert_eq!(fx.int("SELECT stalls FROM continuous_runs"), 3);
    fx.engine.cancel(ticket).unwrap();
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continue_answer_and_operator_follow_reset_stalls_and_wake_the_native_session() {
    for answer in [true, false] {
        let fx = Fx::new(
            script(move |c| async move {
                if c.turn() <= 4 {
                    return Ok(continuation());
                }
                assert_eq!(c.turn(), 5);
                assert_eq!(
                    c.req.session_id.as_deref(),
                    Some(format!("session-{}", c.job()).as_str())
                );
                let packet = c.full_packet();
                if answer {
                    assert!(packet["notes"].as_array().unwrap().iter().any(|note| {
                        note["body"]
                            .as_str()
                            .is_some_and(|body| body.contains("继续"))
                    }));
                } else {
                    assert_eq!(packet["user_updates"], json!(["new operator evidence"]));
                }
                Ok(ask())
            }),
            t1_catalog(),
        );
        let ticket = fx.begin(fx.params().profile("auto")).await;
        let question = stalled(&fx, ticket).await;
        let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
        // Only a platform answer resets the independent completion bounce
        // budget. Ordinary operator follow-up preserves that evidence.
        fx.engine
            .db()
            .with(|q| {
                q.run(
                    "INSERT OR REPLACE INTO check_bounces(job,attempt) VALUES(?,2)",
                    &[&root],
                )
            })
            .unwrap();
        if answer {
            fx.engine
                .answer(question["id"].as_i64().unwrap(), "继续")
                .unwrap();
        } else {
            fx.engine
                .follow(ticket, "new operator evidence", false)
                .unwrap();
        }
        assert_eq!(fx.int("SELECT stalls FROM continuous_runs"), 0);
        assert_eq!(
            fx.all("SELECT last_signal FROM continuous_runs")[0]["last_signal"],
            ""
        );
        assert_eq!(
            fx.int("SELECT count(*) FROM check_bounces"),
            i64::from(!answer)
        );
        if !answer {
            assert_eq!(fx.int("SELECT attempt FROM check_bounces"), 2);
        }
        replied(&fx, ticket, 5).await;
        let questions = fx.engine.questions(ticket).unwrap();
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0]["key"], "after-resume");
        assert_eq!(questions[0]["origin"], "agent");
        assert_eq!(fx.shared.count(|_| true), 5);
        fx.engine.cancel(ticket).unwrap();
        fx.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn automated_mail_cannot_unpark_but_operator_note_replaces_the_platform_question() {
    let fx = Fx::new(
        script(|c| async move {
            if c.turn() <= 4 {
                return Ok(continuation());
            }
            assert_eq!(c.turn(), 5);
            let packet = c.full_packet();
            assert!(
                packet["notes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|note| note["body"] == "operator supplied evidence")
            );
            Ok(ask())
        }),
        t1_catalog(),
    );
    let ticket = fx.begin(fx.params().profile("auto")).await;
    let question = stalled(&fx, ticket).await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    fx.engine
        .db()
        .tx(|q| {
            let seq = q.log(
                sa_engine::db::Who {
                    ticket,
                    id: root,
                    turn: 4,
                },
                "note",
                &json!({
                    "job": root, "sender": "platform", "body": "automated reminder",
                }),
            )?;
            q.run(
                "INSERT INTO mailbox(seq,target,source) VALUES(?,?,NULL)",
                &[&seq, &root],
            )?;
            Ok(())
        })
        .unwrap();
    for _ in 0..32 {
        assert_eq!(fx.engine.step(), 0);
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    assert_eq!(fx.shared.count(|_| true), 4);
    assert_eq!(
        fx.engine.questions(ticket).unwrap()[0]["id"],
        question["id"]
    );
    fx.engine
        .note(ticket, root, "operator supplied evidence")
        .unwrap();
    // A note provides new evidence without changing the existing stall count.
    assert_eq!(fx.int("SELECT stalls FROM continuous_runs"), 3);
    replied(&fx, ticket, 5).await;
    let questions = fx.engine.questions(ticket).unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0]["origin"], "agent");
    assert_eq!(questions[0]["key"], "after-resume");
    assert_ne!(questions[0]["id"], question["id"]);
    assert_eq!(
        fx.all(&format!(
            "SELECT state FROM questions WHERE id={}",
            question["id"]
        ))[0]["state"],
        "withdrawn"
    );
    assert_eq!(fx.shared.count(|_| true), 5);
    fx.engine.cancel(ticket).unwrap();
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_answer_cancels_the_stalled_ticket_without_another_native_turn() {
    let fx = Fx::new(script(|_| async { Ok(continuation()) }), t1_catalog());
    let ticket = fx.begin(fx.params().profile("auto")).await;
    let question = stalled(&fx, ticket).await;
    fx.engine
        .answer(question["id"].as_i64().unwrap(), " 停止 \n")
        .unwrap();
    assert_eq!(root_state(&fx.engine, ticket), "cancelled");
    assert!(fx.engine.questions(ticket).unwrap().is_empty());
    for _ in 0..32 {
        assert_eq!(fx.engine.step(), 0);
    }
    assert_eq!(fx.shared.count(|_| true), 4);
    fx.engine.close().await;
}
