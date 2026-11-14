//! Rejected controls preserve useful work and never conceal engine failures.
mod common;

use common::*;
use sa_engine::adapter::{AdapterError, ErrorClass};
use serde_json::json;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::Duration;

fn denied_send() -> String {
    send(json!({"id": "p06-native-projection-audit", "to": "codex", "permission_profile": "readonly"}))
}

fn child_reply() -> String {
    format!("原样业务成果  \r\n{}\n{}\n{}\n终稿尾部\n",
        denied_send(),
        wire("SA_PROGRESS", json!({"id": "after-rejection", "summary": "拒绝后的合法进展。"})),
        wire("SA_MEMO", json!({"kind": "lesson", "text": "拒绝之后仍保存已经核验的成果。"})))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_child_send_is_visible_while_running_and_keeps_final_work() {
    let release = Gate::new();
    let r = release.clone();
    let parent_notified = Arc::new(AtomicBool::new(false));
    let final_reported = Arc::new(AtomicBool::new(false));
    let (notified, reported) = (parent_notified.clone(), final_reported.clone());
    let fx = Fx::new(script(move |c| {
        let r = r.clone();
        let (notified, reported) = (notified.clone(), reported.clone());
        async move {
            if c.depth() == 0 {
                if c.turn() == 1 {
                    return Ok(send(json!({"id": "worker", "to": "claude"})));
                }
                let packet = c.full_packet();
                if !notified.load(Ordering::SeqCst)
                    && packet["notes"].as_array().unwrap().iter().any(|n|
                        n["body"].as_str().is_some_and(|s| s.contains("该指令未执行，其他工作仍可继续"))) {
                    assert_eq!(packet["reports"][0]["state"], "running");
                    notified.store(true, Ordering::SeqCst);
                }
                if packet["reports"][0]["state"] == "done" {
                    assert_eq!(packet["reports"][0]["result"], json!(child_reply()));
                    reported.store(true, Ordering::SeqCst);
                }
                return Ok("父任务正常收到成果".into());
            }
            assert_eq!(c.turn(), 1, "a correction must not self-wake the child");
            let bad = denied_send();
            c.emit("delta", json!(format!("{bad}\n{bad}\n")));
            r.wait().await;
            let notes = c.host.pending_notes();
            assert_eq!(notes.len(), 1);
            assert!(notes[0]["body"].as_str().unwrap().contains("p06-native-projection-audit"));
            c.host.accept_notes(&[notes[0]["seq"].as_i64().unwrap()]);
            c.emit("delta", json!(format!("{}\n", wire("SA_PROGRESS", json!({"id": "after-rejection", "summary": "拒绝后的合法进展。"})))));
            Ok(child_reply())
        }
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    fx.until(|| parent_notified.load(Ordering::SeqCst)).await;
    assert_eq!(fx.int("SELECT count(*) FROM jobs"), 2);
    assert_eq!(fx.int("SELECT count(*) FROM mailbox m JOIN jobs j ON j.id=m.target WHERE j.parent IS NULL"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM jobs WHERE parent IS NOT NULL AND state='running'"), 1);
    release.open();
    fx.finish(ticket, "done").await;
    assert!(final_reported.load(Ordering::SeqCst));
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='protocol.rejected'"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='job.progress'"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM project_knowledge WHERE kind='lesson'"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM jobs"), 2);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='send'"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM mailbox WHERE consumed_turn IS NULL"), 0);
    assert_eq!(fx.shared.count(|c| c.packet["depth"] == 1), 1);
    assert_eq!(fx.all("SELECT result FROM jobs WHERE parent IS NOT NULL")[0]["result"], json!(child_reply()));
    let receipt = &payloads(&fx, "SELECT payload FROM events WHERE kind='protocol.rejected'")[0];
    assert_eq!(receipt["id"], "p06-native-projection-audit");
    assert_eq!(receipt["executed"], false);
    assert_eq!(receipt["command_sha256"], sha(denied_send()));
    assert!(receipt["reason"].as_str().unwrap().contains("只有当前获授权的主控"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn final_only_rejection_never_schedules_a_corrective_child_turn() {
    let fx = Fx::new(script(|c| async move {
        if c.depth() > 0 {
            assert_eq!(c.turn(), 1);
            return Ok(child_reply());
        }
        Ok(if c.turn() == 1 { send(json!({})) } else { "汇总完成".into() })
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    fx.finish(ticket, "done").await;
    assert_eq!(fx.int("SELECT count(*) FROM jobs"), 2);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='protocol.rejected'"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM mailbox m JOIN jobs j ON j.id=m.target WHERE j.parent IS NOT NULL"), 0);
    assert_eq!(fx.shared.count(|c| c.packet["depth"] == 1), 1);
    assert_eq!(fx.all("SELECT result FROM jobs WHERE parent IS NOT NULL")[0]["result"], json!(child_reply()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unread_stream_hint_expires_without_consuming_business_mail() {
    let gate = Gate::new();
    let g = gate.clone();
    let fx = Fx::new(script(move |c| {
        let g = g.clone();
        async move {
            if c.depth() == 0 {
                return Ok(if c.turn() == 1 { send(json!({})) } else { "汇总".into() });
            }
            if c.turn() == 1 {
                c.emit("delta", json!(format!("{}\n", denied_send())));
                g.wait().await;
                return Ok(child_reply());
            }
            assert_eq!(c.turn(), 2);
            assert!(c.full_packet()["notes"].as_array().unwrap().iter().any(|n| n["body"] == "业务追加"));
            Ok("业务追加处理完成".into())
        }
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    fx.until(|| fx.int("SELECT count(*) FROM events WHERE kind='protocol.rejected'") == 1).await;
    let child = fx.all("SELECT id FROM jobs WHERE parent IS NOT NULL")[0]["id"].as_i64().unwrap();
    fx.engine.note(ticket, child, "业务追加").unwrap();
    gate.open();
    fx.finish(ticket, "done").await;
    assert_eq!(fx.shared.count(|c| c.packet["depth"] == 1), 2);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='protocol.rejected'"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM mailbox WHERE consumed_turn IS NULL"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unread_stream_hint_alone_does_not_self_wake() {
    let fx = Fx::new(script(|c| async move {
        c.emit("delta", json!("SA_ROLE {\"role\":\"\"}\n"));
        // Give the engine an opportunity to record the streaming hint.
        tokio::time::sleep(Duration::from_millis(20)).await;
        Ok("SA_ROLE {\"role\":\"\"}\n完整成果".into())
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    fx.finish(ticket, "done").await;
    assert_eq!(fx.shared.count(|_| true), 1);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='protocol.rejected'"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM mailbox WHERE consumed_turn IS NULL"), 0);
    assert_eq!(fx.engine.root(ticket).unwrap()["result"], "SA_ROLE {\"role\":\"\"}\n完整成果");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn database_control_failure_cancels_immediately_even_for_primary() {
    for profile in ["readonly", "auto"] {
        let cancelled = Arc::new(AtomicBool::new(false));
        let seen = cancelled.clone();
        let fx = Fx::new(script(move |c| {
            let seen = seen.clone();
            async move {
                c.emit("delta", json!(format!("{}\n", wire("SA_PROGRESS", json!({"id": "db-failure", "summary": "数据库故障"})))));
                c.cancel.cancelled().await;
                seen.store(true, Ordering::SeqCst);
                Err(AdapterError::new(ErrorClass::Cancelled, "native cancelled after engine failure"))
            }
        }), t1_catalog());
        fx.engine.db().with(|q| {
            q.conn().execute_batch("CREATE TRIGGER fail_progress BEFORE INSERT ON progress_receipts BEGIN SELECT RAISE(FAIL,'fixture database failure'); END;")?;
            Ok(())
        }).unwrap();
        let ticket = fx.begin(fx.params().profile(profile)).await;
        fx.finish(ticket, "failed").await;
        assert!(cancelled.load(Ordering::SeqCst));
        assert!(fx.engine.root(ticket).unwrap()["result"].as_str().unwrap().contains("fixture database failure"));
        assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind IN ('protocol.rejected','auto.protocol','job.progress','reply')"), 0);
        fx.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejection_receipt_database_failure_is_fatal() {
    let fx = Fx::new(script(|c| async move {
        c.emit("delta", json!("SA_ROLE {\"role\":\"\"}\n"));
        c.cancel.cancelled().await;
        Err(AdapterError::new(ErrorClass::Cancelled, "cancelled"))
    }), t1_catalog());
    fx.engine.db().with(|q| {
        q.conn().execute_batch("CREATE TRIGGER fail_rejection BEFORE INSERT ON events WHEN NEW.kind='protocol.rejected' BEGIN SELECT RAISE(FAIL,'receipt database failure'); END;")?;
        Ok(())
    }).unwrap();
    let ticket = fx.begin(fx.params()).await;
    fx.finish(ticket, "failed").await;
    assert!(fx.engine.root(ticket).unwrap()["result"].as_str().unwrap().contains("receipt database failure"));
    assert_eq!(fx.int("SELECT count(*) FROM mailbox"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrected_send_in_the_same_native_turn_creates_one_child() {
    let corrected = send(json!({"id": "p06-native-projection-audit", "to": "codex"}));
    let expected = format!("修正后保留原业务成果\n{corrected}\n");
    let reply = expected.clone();
    let fx = Fx::new(script(move |c| {
        let (corrected, reply) = (corrected.clone(), reply.clone());
        async move {
            if c.depth() == 0 {
                return Ok(if c.turn() == 1 { send(json!({"to": "claude"})) } else { "父任务汇总".into() });
            }
            if c.depth() == 2 {
                return Ok("合法孙任务完成".into());
            }
            if c.turn() > 1 {
                assert_eq!(c.full_packet()["reports"][0]["result"], "合法孙任务完成");
                return Ok("业务成果和孙任务均已报告".into());
            }
            c.emit("delta", json!(format!("{}\n", denied_send())));
            tokio::time::timeout(Duration::from_secs(2), async {
                while c.host.pending_notes().is_empty() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }).await.expect("current native turn receives a correction");
            let notes = c.host.pending_notes();
            assert_eq!(notes.len(), 1);
            c.host.accept_notes(&[notes[0]["seq"].as_i64().unwrap()]);
            c.emit("delta", json!(format!("{corrected}\n{corrected}\n")));
            Ok(reply)
        }
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    fx.finish(ticket, "done").await;
    assert_eq!(fx.int("SELECT count(*) FROM jobs"), 3);
    assert_eq!(fx.int("SELECT count(*) FROM jobs WHERE depth=2"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='send'"), 2);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='protocol.rejected'"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM jobs WHERE depth=2 AND permission_override IS NOT NULL"), 0);
    let replies = payloads(&fx, "SELECT payload FROM events WHERE kind='reply' AND job=(SELECT id FROM jobs WHERE depth=1) ORDER BY seq");
    assert_eq!(replies[0]["text"], expected);
    assert_eq!(fx.shared.count(|c| c.packet["depth"] == 2), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn physical_retry_keeps_one_rejection_receipt_per_logical_command() {
    let fx = Fx::new(script(|c| async move {
        if c.depth() == 0 {
            return Ok(if c.turn() == 1 { send(json!({})) } else { "父任务汇总".into() });
        }
        c.emit("delta", json!(format!("{}\n", denied_send())));
        if c.turn() == 1 {
            tokio::time::timeout(Duration::from_secs(2), async {
                while c.host.pending_notes().is_empty() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }).await.expect("rejection was durably recorded before retry");
            return Err(AdapterError::new(ErrorClass::Network, "fixture temporary disconnect"));
        }
        assert_eq!(c.turn(), 2);
        Ok(child_reply())
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    fx.finish(ticket, "done").await;
    assert_eq!(fx.int("SELECT count(*) FROM jobs"), 2);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='protocol.rejected'"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='turn.retry_scheduled'"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM mailbox m JOIN jobs j ON j.id=m.target WHERE j.depth=1"), 1);
    assert_eq!(fx.all("SELECT result FROM jobs WHERE depth=1")[0]["result"], child_reply());
}
