//! Port of `tests-node/engine.test.cjs` (spec §8, T1 cases 1-15) and the
//! burst test of `tests-node/engine-burst.test.cjs` (case 24).

mod common;

use common::*;
use futures::future::BoxFuture;
use sa_engine::CreateParams;
use sa_engine::adapter::AdapterEvent;
use sa_engine::services::Snapshotter;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

fn write_image(project: &std::path::Path, name: &str, format: image::ImageFormat, width: u32) -> Vec<u8> {
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::RgbImage::new(width, 2).write_to(&mut encoded, format).unwrap();
    let bytes = encoded.into_inner();
    std::fs::write(project.join(name), &bytes).unwrap();
    bytes
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn image_follow_replaces_images_updates_text_and_survives_reopen() {
    let mut fx = Fx::new(script(|c| async move {
        assert!(!c.req.writable);
        assert_eq!(c.req.images.len(), 1);
        if c.turn() == 1 {
            assert_eq!(c.req.images[0].media_type(), "image/png");
        } else {
            assert_eq!(c.req.images[0].name(), "新图.JPEG");
            assert_eq!(c.req.images[0].media_type(), "image/jpeg");
            assert!(c.req.session_id.is_some());
            let files = c.packet["files"].as_array().unwrap();
            assert_eq!(files.len(), 2);
            let text = files.iter().find(|file| file["name"] == "说明.txt").unwrap();
            assert_eq!(std::fs::read(text["object_path"].as_str().unwrap()).unwrap(), b"updated");
        }
        Ok("seen".into())
    }), t1_catalog());
    write_image(&fx.project, "旧图.PNG", image::ImageFormat::Png, 2);
    std::fs::write(fx.project.join("说明.txt"), b"original").unwrap();
    let ticket = fx.begin(fx.params().profile("readonly").files(&["旧图.PNG", "说明.txt"])).await;
    fx.finish(ticket, "done").await;
    let bytes = write_image(&fx.project, "新图.JPEG", image::ImageFormat::Jpeg, 3);
    std::fs::write(fx.project.join("说明.txt"), b"updated").unwrap();
    fx.engine.follow_with_files(ticket, "请看新图", false, &["新图.JPEG".into(), "说明.txt".into()]).await.unwrap();
    std::fs::write(fx.project.join("新图.JPEG"), b"source changed").unwrap();
    std::fs::write(fx.project.join("说明.txt"), b"source changed").unwrap();
    fx.restart(quiet()).await;
    fx.engine.start().unwrap();
    fx.finish(ticket, "done").await;
    assert_eq!(fx.shared.calls(|c| (c.packet["turn"] == 2).then(|| c.req.images[0].data().to_vec())), vec![bytes.clone()]);
    // A plain follow keeps the last explicitly attached image set.
    fx.engine.follow(ticket, "继续说明", false).unwrap();
    fx.finish(ticket, "done").await;
    assert_eq!(fx.shared.calls(|c| (c.packet["turn"] == 3).then(|| c.req.images[0].data().to_vec())), vec![bytes]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_image_follow_supersedes_pending_images_without_mutating_the_running_request() {
    let entered = Gate::new();
    let release = Gate::new();
    let (started, finish) = (entered.clone(), release.clone());
    let fx = Fx::new(script(move |c| {
        let (started, finish) = (started.clone(), finish.clone());
        async move {
            if c.turn() == 1 {
                let original = c.req.images[0].data().to_vec();
                started.open();
                finish.wait().await;
                assert_eq!(c.req.images[0].name(), "初始.png");
                assert_eq!(c.req.images[0].data(), original);
            } else {
                assert_eq!(c.req.images.len(), 1);
                assert_eq!(c.req.images[0].name(), "最新.PNG");
                assert!(c.req.session_id.is_some());
            }
            Ok("seen".into())
        }
    }), t1_catalog());
    write_image(&fx.project, "初始.png", image::ImageFormat::Png, 2);
    write_image(&fx.project, "中间.jpeg", image::ImageFormat::Jpeg, 3);
    let newest = write_image(&fx.project, "最新.PNG", image::ImageFormat::Png, 4);
    let ticket = fx.begin(fx.params().profile("readonly").files(&["初始.png"])).await;
    fx.until(|| fx.shared.count(|_| true) == 1).await;
    entered.wait().await;
    fx.engine.follow_with_files(ticket, "改看中间图", false, &["中间.jpeg".into()]).await.unwrap();
    fx.engine.follow_with_files(ticket, "改看最新图", false, &["最新.PNG".into()]).await.unwrap();
    release.open();
    fx.finish(ticket, "done").await;
    assert_eq!(fx.shared.count(|_| true), 2);
    assert_eq!(fx.shared.calls(|c| (c.packet["turn"] == 2).then(|| c.req.images[0].data().to_vec())), vec![newest]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_image_follow_leaves_state_manifest_and_journal_unchanged() {
    let fx = Fx::new(script(|_| async { Ok("seen".into()) }), t1_catalog());
    write_image(&fx.project, "原图.png", image::ImageFormat::Png, 2);
    write_image(&fx.project, "伪装.jpeg", image::ImageFormat::Png, 2);
    std::fs::write(fx.project.join("损坏.png"), b"not an image").unwrap();
    let ticket = fx.begin(fx.params().profile("readonly").files(&["原图.png"])).await;
    fx.finish(ticket, "done").await;
    let before = fx.all("SELECT * FROM job_files");
    let root = fx.engine.root(ticket).unwrap();
    let seq = fx.int("SELECT max(seq) FROM events");
    for files in [vec!["../outside.png".into()], vec!["伪装.jpeg".into()], vec!["损坏.png".into()], vec!["原图.png".into(); 5]] {
        assert!(fx.engine.follow_with_files(ticket, "new", false, &files).await.is_err());
        assert_eq!(fx.all("SELECT * FROM job_files"), before);
        assert_eq!(fx.engine.root(ticket).unwrap(), root);
        assert_eq!(fx.int("SELECT max(seq) FROM events"), seq);
    }
    let unsupported = fx.begin(fx.params().lead("deepseek")).await;
    fx.finish(unsupported, "done").await;
    let seq = fx.int("SELECT max(seq) FROM events");
    let err = fx.engine.follow_with_files(unsupported, "看图", false, &["原图.png".into()]).await.unwrap_err();
    assert!(err.to_string().contains("Codex、Claude Code 或 Antigravity"));
    assert_eq!(fx.int("SELECT max(seq) FROM events"), seq);
    assert_eq!(root_state(&fx.engine, unsupported), "done");
    // Snapshot validation may succeed, but a disallowed state transition
    // must still commit neither attachment metadata nor the user instruction.
    let stopped = fx.create(fx.params().profile("auto").files(&["原图.png"])).await;
    fx.engine.cancel(stopped).unwrap();
    let before = fx.all("SELECT * FROM job_files");
    let seq = fx.int("SELECT max(seq) FROM events");
    assert!(fx.engine.follow_with_files(stopped, "resume", false, &["原图.png".into()]).await.is_err());
    assert_eq!(fx.all("SELECT * FROM job_files"), before);
    assert_eq!(fx.int("SELECT max(seq) FROM events"), seq);
    assert_eq!(root_state(&fx.engine, stopped), "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_images_use_snapshots_through_followup_and_context_compaction() {
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::RgbImage::new(3, 2).write_to(&mut encoded, image::ImageFormat::Png).unwrap();
    let bytes = encoded.into_inner();
    let expected = bytes.clone();
    let fx = Fx::new(script(move |c| {
        let expected = expected.clone();
        async move {
            assert_eq!(c.req.images.len(), 1);
            assert_eq!(c.req.images[0].data(), expected);
            assert_eq!(c.req.images[0].media_type(), "image/png");
            assert!(!c.req.writable);
            assert!(c.packet["full_context"]["object_path"].is_string());
            if c.turn() > 1 { assert!(c.req.session_id.is_some()); }
            Ok("image received".into())
        }
    }), t1_catalog());
    project_file(&fx.project, "图片.PNG", &bytes);
    let mut params = fx.params().files(&["图片.PNG"]);
    params.goal = "大".repeat(800_000);
    let id = fx.create(params).await;
    // A later project edit must not change native attachment bytes.
    project_file(&fx.project, "图片.PNG", b"modified after snapshot");
    fx.engine.start().unwrap();
    fx.finish(id, "done").await;
    fx.engine.follow(id, "再检查相同图片", false).unwrap();
    fx.finish(id, "done").await;
    assert_eq!(fx.shared.count(|c| c.req.images.len() == 1), 2);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsupported_image_harness_fails_before_adapter_execution() {
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::RgbImage::new(3, 2).write_to(&mut encoded, image::ImageFormat::Png).unwrap();
    let fx = Fx::new(script(|_| async { panic!("unsupported image turn must not reach an adapter") }), t1_catalog());
    project_file(&fx.project, "image.png", &encoded.into_inner());
    let id = fx.begin(fx.params().lead("deepseek").files(&["image.png"])).await;
    fx.finish(id, "failed").await;
    assert_eq!(fx.shared.count(|_| true), 0);
    let result = fx.all("SELECT result FROM jobs WHERE parent IS NULL");
    assert!(result[0]["result"].as_str().unwrap().contains("Codex、Claude Code 或 Antigravity"));
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn member_progress_arrives_during_execution_and_replay_is_scoped() {
    let release = Gate::new();
    let summary = " 已校验  3 个文件 🔎e\u{301} ";
    let report = wire("SA_PROGRESS", json!({"id": "measure", "summary": summary}));
    let (r, rep) = (release.clone(), report.clone());
    let fx = Fx::new(
        script(move |c| {
            let (r, rep) = (r.clone(), rep.clone());
            async move {
                if c.depth() > 0 {
                    return Ok(wire("SA_PROGRESS", json!({"id": "measure", "summary": "协作者已核对输入。"})));
                }
                if c.turn() > 1 {
                    return Ok(wire("SA_PROGRESS", json!({"id": "measure", "summary": "主控已核对回执。"})));
                }
                c.emit("delta", json!(&rep[..12]));
                c.emit("delta", json!(&rep[12..]));
                c.emit("message.boundary", Value::Null);
                r.wait().await;
                Ok(format!("{rep}\n{}", send(json!({}))))
            }
        }),
        t1_catalog(),
    );
    let id = fx.begin(fx.params()).await;
    let rows = || payloads(&fx, &format!("SELECT payload FROM events WHERE ticket={id} AND kind='job.progress' ORDER BY seq"));
    fx.until(|| rows().len() == 1).await;
    assert_eq!(root_state(&fx.engine, id), "running");
    assert_eq!(rows()[0], json!({"id": "measure", "summary": summary}));
    release.open();
    fx.finish(id, "done").await;
    let summaries: Vec<Value> = rows().iter().map(|r| r["summary"].clone()).collect();
    assert_eq!(summaries, vec![json!(summary), json!("协作者已核对输入。"), json!("主控已核对回执。")]);
    assert_eq!(fx.int("SELECT count(*) FROM progress_receipts"), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_or_conflicting_progress_never_overwrites_or_fails() {
    let p = |v: Value| wire("SA_PROGRESS", v);
    let text = [
        p(json!({"id": "key", "summary": "第一条"})),
        p(json!({"id": "key", "summary": "改写"})),
        "SA_PROGRESS {broken".into(),
        p(json!({"id": "", "summary": "no key"})),
        p(json!({"id": "large", "summary": "字".repeat(321)})),
        p(json!({"id": "line", "summary": "a\nb"})),
        p(json!({"id": "extra", "summary": "ok", "secret": "sensitive"})),
        "checked".into(),
    ]
    .join("\n");
    let fx = Fx::new(
        script(move |_| {
            let t = text.clone();
            async move { Ok(t) }
        }),
        t1_catalog(),
    );
    let id = fx.begin(fx.params()).await;
    fx.finish(id, "done").await;
    assert_eq!(
        payloads(&fx, "SELECT payload FROM events WHERE kind='job.progress'"),
        vec![json!({"id": "key", "summary": "第一条"})]
    );
    let mut expected = vec![json!({"reason": "conflicting_id"}), json!({"reason": "invalid_json"})];
    expected.extend(std::iter::repeat_n(json!({"reason": "invalid_fields"}), 4));
    assert_eq!(
        payloads(&fx, "SELECT payload FROM events WHERE kind='protocol.progress_invalid' ORDER BY seq"),
        expected
    );
}

async fn stale_progress_suppressed(action: &str) {
    let release = Gate::new();
    let r = release.clone();
    let fx = Fx::new(
        script(move |c| {
            let r = r.clone();
            async move {
                if c.depth() == 0 {
                    return Ok(if c.turn() == 1 { send(json!({})) } else { "merged".into() });
                }
                r.wait().await;
                let stale = wire("SA_PROGRESS", json!({"id": "old", "summary": "过期进展"}));
                c.emit("delta", json!(format!("{stale}\n")));
                Ok(stale)
            }
        }),
        t1_catalog(),
    );
    let id = fx.begin(fx.params()).await;
    fx.until(|| fx.shared.count(|c| c.packet["depth"].as_i64() > Some(0)) > 0).await;
    if action == "cancel" {
        fx.engine.cancel(id).unwrap();
    } else {
        fx.engine.follow(id, "新的指令", false).unwrap();
    }
    release.open();
    if action == "follow" {
        fx.finish(id, "done").await;
    }
    fx.until(|| fx.engine.active_count() == 0).await;
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='job.progress'"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_suppresses_late_progress_from_the_old_worker() {
    stale_progress_suppressed("cancel").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn follow_suppresses_late_progress_from_the_old_worker() {
    stale_progress_suppressed("follow").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streaming_starts_a_peer_and_exact_fields_survive_replay() {
    let entered = Gate::new();
    let body = "收到  \r\n😀e\u{301}\u{2028}尾   ";
    let cmd = send(json!({"model": "review", "permission_profile": "readonly"}));
    let e = entered.clone();
    let fx = Fx::new(
        script(move |c| {
            let (e, cmd) = (e.clone(), cmd.clone());
            async move {
                if c.depth() > 0 {
                    e.open();
                    return Ok(body.into());
                }
                if c.turn() == 1 {
                    c.emit("delta", json!(&cmd[..9]));
                    c.emit("delta", json!(&cmd[9..]));
                    c.emit("message.boundary", Value::Null);
                    e.wait().await;
                    return Ok(cmd);
                }
                assert_eq!(c.packet["reports"][0]["result"], json!(body));
                Ok(body.into())
            }
        }),
        t1_catalog(),
    );
    let id = fx.begin(fx.params().profile("full")).await;
    fx.finish(id, "done").await;
    let calls = fx.shared.calls.lock().unwrap();
    let child = calls.iter().find(|c| c.packet["depth"].as_i64() > Some(0)).unwrap();
    assert_eq!(child.req.route.provider, "test-local");
    assert_eq!(child.req.route.model, "review");
    assert_eq!(child.req.route.mode, "cli");
    assert!(!child.req.writable);
    assert_eq!(child.req.profile, "readonly");
    assert_eq!(child.packet["assignment"], json!("核对  \r\n\t🧭"));
    drop(calls);
    assert_eq!(fx.all("SELECT seq FROM events WHERE kind='send'").len(), 1);
    for e in fx.all("SELECT payload,sha256 FROM events ORDER BY seq") {
        assert_eq!(e["sha256"].as_str().unwrap(), sha(e["payload"].as_str().unwrap()));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_flight_notes_wake_a_child_and_host_acceptance_is_idempotent() {
    let (enter, release) = (Gate::new(), Gate::new());
    let (en, re) = (enter.clone(), release.clone());
    let fx = Fx::new(
        script(move |c| {
            let (en, re) = (en.clone(), re.clone());
            async move {
                if c.depth() == 0 {
                    return Ok(if c.turn() == 1 { send(json!({})) } else { "merged".into() });
                }
                if c.turn() == 1 {
                    en.open();
                    re.wait().await;
                    let notes = c.host.pending_notes();
                    assert_eq!(notes.len(), 2);
                    let seq = notes[0]["seq"].as_i64().unwrap();
                    c.host.accept_notes(&[seq]);
                    c.host.accept_notes(&[seq]);
                    let bodies: Vec<Value> = c.host.pending_notes().iter().map(|n| n["body"].clone()).collect();
                    assert_eq!(bodies, vec![json!("after")]);
                    return Ok("first".into());
                }
                let bodies: Vec<Value> = c.packet["notes"].as_array().unwrap().iter().map(|n| n["body"].clone()).collect();
                assert_eq!(bodies, vec![json!("during"), json!("after")]);
                Ok("second".into())
            }
        }),
        t1_catalog(),
    );
    let id = fx.begin(fx.params()).await;
    fx.until(|| enter.is_open()).await;
    let job = fx.int("SELECT id FROM jobs WHERE parent IS NOT NULL");
    fx.engine.note(id, job, "during").unwrap();
    fx.engine.note(id, job, "after").unwrap();
    assert!(fx.engine.note(id + 1, job, "wrong ticket").is_err());
    release.open();
    fx.finish(id, "done").await;
    assert_eq!(fx.all("SELECT seq FROM events WHERE kind='note.accepted'").len(), 1);
    assert_eq!(fx.int(&format!("SELECT turn FROM jobs WHERE id={job}")), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sa_note_exact_body_and_receipt_do_not_duplicate_on_replay() {
    let fx = Fx::new(
        script(|c| async move {
            if c.depth() == 0 {
                return Ok(if c.turn() == 1 { send(json!({})) } else { "read peer note".into() });
            }
            let root = c.packet["roster"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["parent"].is_null())
                .unwrap()["job"]
                .clone();
            let line = wire("SA_NOTE", json!({"id": "n", "job": root, "body": "n\r\n "}));
            c.emit("delta", json!(format!("{line}\n")));
            Ok(line)
        }),
        t1_catalog(),
    );
    let id = fx.begin(fx.params()).await;
    fx.finish(id, "done").await;
    let rows = payloads(&fx, "SELECT payload FROM events WHERE kind='note'");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["sha256"].as_str().unwrap(), sha("n\r\n "));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn approval_belongs_to_its_live_turn_and_cancellation_expires_it() {
    let verdict: Arc<std::sync::Mutex<Option<bool>>> = Arc::default();
    let v = verdict.clone();
    let fx = Fx::new(
        script(move |c| {
            let v = v.clone();
            async move {
                let ok = c.host.approve(json!({"action": "edit", "scope": "workspace"})).await;
                *v.lock().unwrap() = Some(ok);
                c.emit("delta", json!(format!("{}\n", send(json!({})))));
                Ok("finished".into())
            }
        }),
        t1_catalog(),
    );
    let id = fx.begin(fx.params().profile("ask")).await;
    fx.until(|| !fx.all("SELECT id FROM approvals WHERE decision='pending'").is_empty()).await;
    let approval = fx.int("SELECT id FROM approvals WHERE decision='pending'");
    fx.engine.cancel(id).unwrap();
    fx.until(|| verdict.lock().unwrap().is_some()).await;
    assert_eq!(*verdict.lock().unwrap(), Some(false));
    assert!(fx.engine.decide(approval, true).is_err());
    assert_eq!(root_state(&fx.engine, id), "cancelled");
    fx.until(|| fx.engine.active_count() == 0).await;
    assert_eq!(fx.all("SELECT id FROM jobs").len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_writer_approval_is_delivered_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let n = calls.clone();
    let fx = Fx::new(
        script(move |c| {
            let n = n.clone();
            async move {
                assert!(c.host.approve(json!({"cmd": "safe fixture"})).await);
                n.fetch_add(1, Ordering::SeqCst);
                Ok("approved".into())
            }
        }),
        t1_catalog(),
    );
    let id = fx.begin(fx.params().profile("partial")).await;
    fx.until(|| !fx.all("SELECT id FROM approvals WHERE decision='pending'").is_empty()).await;
    let approval = fx.int("SELECT id FROM approvals WHERE decision='pending'");
    fx.engine.decide(approval, true).unwrap();
    assert!(fx.engine.decide(approval, true).is_err());
    fx.finish(id, "done").await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn declared_writers_serialize_across_tickets() {
    let writers = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let first = Gate::new();
    let (w, pk, g) = (writers.clone(), peak.clone(), first.clone());
    let fx = Fx::new(
        script(move |c| {
            let (w, pk, g) = (w.clone(), pk.clone(), g.clone());
            async move {
                assert!(c.req.writable);
                let now = w.fetch_add(1, Ordering::SeqCst) + 1;
                pk.fetch_max(now, Ordering::SeqCst);
                g.wait().await;
                w.fetch_sub(1, Ordering::SeqCst);
                Ok("saved".into())
            }
        }),
        t1_catalog(),
    );
    let a = fx.begin(fx.params().profile("partial")).await;
    let b = fx
        .create(CreateParams::new("second project", &fx.project).lead("deepseek").profile("partial"))
        .await;
    fx.until(|| writers.load(Ordering::SeqCst) == 1).await;
    fx.engine.step();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(fx.shared.count(|_| true), 1);
    first.open();
    fx.finish(a, "done").await;
    fx.finish(b, "done").await;
    assert_eq!(peak.load(Ordering::SeqCst), 1);
}

async fn readers_and_one_writer(profile: &str) {
    let readers = 80;
    let child = |id: String, write: bool| send(json!({"id": id, "to": "deepseek", "body": id, "write": write}));
    let mut cmds: Vec<String> = (0..readers).map(|i| child(format!("reader-{i}"), false)).collect();
    cmds.push(child("writer-a".into(), true));
    cmds.push(child("writer-b".into(), true));
    let text = cmds.join("\n");
    let running_readers = Arc::new(AtomicUsize::new(0));
    let running_writers = Arc::new(AtomicUsize::new(0));
    let aborted = Arc::new(AtomicUsize::new(0));
    let (rr, rw, ab) = (running_readers.clone(), running_writers.clone(), aborted.clone());
    let fx = Fx::new(
        script(move |c| {
            let (rr, rw, ab, text) = (rr.clone(), rw.clone(), ab.clone(), text.clone());
            async move {
                if c.depth() == 0 {
                    return Ok(text);
                }
                if c.packet["assignment"].as_str().unwrap().starts_with("reader-") {
                    rr.fetch_add(1, Ordering::SeqCst);
                } else {
                    rw.fetch_add(1, Ordering::SeqCst);
                }
                c.cancel.cancelled().await;
                ab.fetch_add(1, Ordering::SeqCst);
                Ok("late result".into())
            }
        }),
        t1_catalog(),
    );
    let id = fx.begin(fx.params().profile(profile)).await;
    fx.until(|| running_readers.load(Ordering::SeqCst) == readers && running_writers.load(Ordering::SeqCst) == 1)
        .await;
    assert_eq!(fx.engine.active_count(), 81);
    let states: std::collections::BTreeMap<String, i64> = fx
        .all(&format!("SELECT state,count(*) AS n FROM jobs WHERE ticket={id} GROUP BY state"))
        .iter()
        .map(|r| (r["state"].as_str().unwrap().to_string(), r["n"].as_i64().unwrap()))
        .collect();
    assert_eq!(
        states,
        [("queued".to_string(), 1), ("running".to_string(), 81), ("waiting".to_string(), 1)].into()
    );
    fx.engine.cancel(id).unwrap();
    fx.until(|| fx.engine.active_count() == 0).await;
    assert_eq!(aborted.load(Ordering::SeqCst), 81);
    assert_eq!(fx.int(&format!("SELECT count(*) FROM jobs WHERE ticket={id} AND state='cancelled'")), 83);
    assert_eq!(running_writers.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_ticket_admits_80_readers_and_one_writer() {
    readers_and_one_writer("partial").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_ticket_admits_80_readers_and_one_writer() {
    readers_and_one_writer("auto").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nine_delegated_levels_return_through_the_chain() {
    let fx = Fx::new(
        script(|c| async move {
            let d = c.depth();
            if c.turn() == 1 && d < 9 {
                return Ok(send(json!({"body": format!("inspect level {}", d + 1)})));
            }
            if d < 9 {
                assert_eq!(c.packet["reports"][0]["result"], json!(format!("verified {}", d + 1)));
            }
            Ok(format!("verified {d}"))
        }),
        t1_catalog(),
    );
    let id = fx.begin(fx.params()).await;
    fx.finish(id, "done").await;
    assert_eq!(fx.engine.root(id).unwrap()["result"], json!("verified 0"));
    assert_eq!(fx.int(&format!("SELECT count(*) FROM jobs WHERE ticket={id} AND state='done'")), 10);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_input_is_archived_and_verifiable() {
    let goal = "大".repeat(800_000);
    let observed = Arc::new(AtomicBool::new(false));
    let (g, o) = (goal.clone(), observed.clone());
    let fx = Fx::new(
        script(move |c| {
            let (g, o) = (g.clone(), o.clone());
            async move {
                assert!(c.req.prompt.len() <= 6 * 1024 * 1024);
                let full = &c.packet["full_context"];
                let bytes = std::fs::read(full["object_path"].as_str().unwrap()).unwrap();
                assert_eq!(sha(&bytes), full["sha256"].as_str().unwrap());
                let original: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(original["goal"], json!(g));
                o.store(true, Ordering::SeqCst);
                Ok("verified".into())
            }
        }),
        t1_catalog(),
    );
    let mut params = fx.params();
    params.goal = goal;
    let id = fx.begin(params).await;
    fx.finish(id, "done").await;
    assert!(observed.load(Ordering::SeqCst));
}

/// Snapshotter whose second call blocks until `release` (T1:396-411).
struct SlowSnapshot {
    copied: Arc<AtomicUsize>,
    admitted: Gate,
    release: Gate,
}

impl Snapshotter for SlowSnapshot {
    fn snapshot(&self, project: PathBuf, names: Vec<String>, directory: PathBuf) -> BoxFuture<'static, anyhow::Result<Value>> {
        let (copied, admitted, release) = (self.copied.clone(), self.admitted.clone(), self.release.clone());
        Box::pin(async move {
            if copied.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
                admitted.open();
                release.wait().await;
            }
            let n = &names[0];
            let bin = std::fs::read(project.join(n))?;
            let digest = sha(&bin);
            std::fs::create_dir_all(&directory)?;
            let dest = directory.join(&digest);
            std::fs::write(&dest, &bin)?;
            Ok(json!([{"name": n, "bytes": bin.len(), "sha256": digest, "object_path": dest}]))
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn root_snapshot_and_plan_delivered_and_cancel_during_child_snapshot_is_clean() {
    let (admitted, release) = (Gate::new(), Gate::new());
    let copied = Arc::new(AtomicUsize::new(0));
    let opts = sa_engine::EngineOptions {
        snapshotter: Arc::new(SlowSnapshot {
            copied: copied.clone(),
            admitted: admitted.clone(),
            release: release.clone(),
        }),
        ..quiet()
    };
    let re = release.clone();
    let fx = Fx::with(
        script(move |c| {
            let re = re.clone();
            async move {
                let file = &c.packet["files"][0];
                assert_eq!(file["bytes"], json!(4));
                assert_eq!(std::fs::read(file["object_path"].as_str().unwrap()).unwrap(), vec![0u8, 255, 13, 10]);
                assert_eq!(c.packet["plan"][0]["body"], json!("operator plan"));
                c.emit("delta", json!(format!("{}\n", send(json!({"files": ["image.bin"]})))));
                re.wait().await;
                Ok("no child".into())
            }
        }),
        t1_catalog(),
        opts,
    );
    project_file(&fx.project, "image.bin", &[0, 255, 13, 10]);
    let id = fx
        .create(CreateParams::new("attachments", &fx.project).files(&["image.bin"]))
        .await;
    fx.engine
        .db()
        .with(|q| {
            q.conn().execute_batch(
                "CREATE TABLE studio_plan(id INTEGER PRIMARY KEY,ticket INTEGER NOT NULL,body TEXT NOT NULL,done INTEGER NOT NULL)",
            )?;
            q.run("INSERT INTO studio_plan VALUES(1,?,'operator plan',0)", &[&id])?;
            Ok(())
        })
        .unwrap();
    fx.engine.start().unwrap();
    fx.until(|| copied.load(Ordering::SeqCst) == 2).await;
    admitted.wait().await;
    fx.engine.cancel(id).unwrap();
    release.open();
    fx.until(|| fx.engine.active_count() == 0).await;
    assert_eq!(fx.all("SELECT id FROM jobs").len(), 1);
    assert!(
        fx.engine
            .create(CreateParams::new("escape", &fx.project).files(&["../secret"]))
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_schema_migration_is_non_destructive_and_recovery_waits_for_start() {
    let dir = tempfile::Builder::new().prefix("superagent-old-").tempdir().unwrap();
    {
        let old = rusqlite::Connection::open(dir.path().join("hub.sqlite3")).unwrap();
        old.execute_batch("CREATE TABLE tickets(id INTEGER PRIMARY KEY,goal TEXT,cwd TEXT,writes INTEGER,lead TEXT,created REAL);")
            .unwrap();
        old.execute(
            "INSERT INTO tickets VALUES(1,?,?,0,?,0)",
            rusqlite::params!["retained", dir.path().to_string_lossy(), "codex"],
        )
        .unwrap();
    }
    let fx = Fx::new(script(|_| async { Ok("unused".to_string()) }), t1_catalog());
    let e = sa_engine::Engine::open(dir.path(), adapters(&fx.shared), quiet()).unwrap();
    let row = common::all(&e, "SELECT goal,permission_profile,allow_lead_routing FROM tickets");
    assert_eq!(
        Value::Object(row[0].clone()),
        json!({"goal": "retained", "permission_profile": "legacy", "allow_lead_routing": 0})
    );
    e.db()
        .with(|q| {
            q.run("INSERT INTO jobs(ticket,agent,body,writing,depth,state) VALUES(1,'codex','saved',0,0,'running')", &[])?;
            q.run("INSERT INTO approvals(ticket,job,turn,action) VALUES(1,1,1,'{}')", &[])?;
            Ok(())
        })
        .unwrap();
    assert_eq!(root_state(&e, 1), "running");
    e.start().unwrap();
    assert_eq!(root_state(&e, 1), "interrupted");
    assert_eq!(common::all(&e, "SELECT decision FROM approvals")[0]["decision"], json!("expired"));
    e.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn burst_of_1000_native_and_delta_pairs_stays_byte_exact() {
    let catalog = json!({
        "default": {"provider": "local", "mode": "cli", "model": "benchmark"},
        "providers": [{"id": "local", "models": [{"id": "benchmark"}]}],
        "modes": [{"id": "cli", "available": true}]
    });
    let fx = Fx::new(
        script(|c| async move {
            let started = std::time::Instant::now();
            for i in 0..1000 {
                let _ = c.events.send(AdapterEvent::Journal {
                    kind: "native".into(),
                    data: json!({"type": "fixture", "chunk": format!("字符😀 {i}")}),
                });
                let _ = c.events.send(AdapterEvent::Journal {
                    kind: "delta".into(),
                    data: json!(format!("字符😀 {i}\r\n")),
                });
                if i % 64 == 0 {
                    tokio::task::yield_now().await;
                }
            }
            eprintln!("emitted 2000 events in {:?}", started.elapsed());
            Ok("ok".into())
        }),
        catalog,
    );
    fx.engine.start().unwrap();
    for _ in 0..3 {
        let ticket = fx.create(CreateParams::new("throughput sample", &fx.project)).await;
        fx.finish(ticket, "done").await;
        let records = fx.all(&format!(
            "SELECT kind,payload,sha256 FROM events WHERE ticket={ticket} AND kind IN ('native','delta') ORDER BY seq"
        ));
        assert_eq!(records.len(), 2000);
        for i in 0..1000 {
            let native: Value = serde_json::from_str(records[2 * i]["payload"].as_str().unwrap()).unwrap();
            assert_eq!(native, json!({"type": "fixture", "chunk": format!("字符😀 {i}")}));
            let delta: Value = serde_json::from_str(records[2 * i + 1]["payload"].as_str().unwrap()).unwrap();
            assert_eq!(delta, json!(format!("字符😀 {i}\r\n")));
        }
        for r in &records {
            assert_eq!(r["sha256"].as_str().unwrap(), sha(r["payload"].as_str().unwrap()));
        }
    }
    assert_eq!(fx.int("PRAGMA synchronous"), 2);
    let mode = fx.all("PRAGMA journal_mode");
    assert_eq!(mode[0]["journal_mode"], json!("wal"));
}
