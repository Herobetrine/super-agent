//! Port of `tests-node/engine-autonomy.test.cjs` (spec §8, T2 cases 16-23).

mod common;

use common::*;
use futures::FutureExt;
use futures::future::BoxFuture;
use rusqlite::Connection;
use sa_engine::services::{Extensions, InvokeArgs, InvokeResult};
use sa_engine::{CreateParams, EngineOptions};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn cmd(tag: &str, v: Value) -> String {
    wire(tag, v)
}

fn ended() -> String {
    cmd(
        "SA_EXHAUSTED",
        json!({"status": "done", "summary": "project checked", "checks": ["inspected files"], "alternatives": ["no open request"]}),
    )
}

fn done() -> String {
    cmd("SA_FINISH", json!({"status": "done", "summary": "verified milestone"}))
}

/// `setup(t, fn)`: the project and the journal share one directory.
fn setup(s: Script) -> Fx {
    setup_with(s, quiet())
}

fn setup_with(s: Script, opts: EngineOptions) -> Fx {
    let mut fx = Fx::with(s, t2_catalog(), opts);
    fx.project = fx.data.clone();
    fx
}

async fn create(fx: &Fx, profile: &str) -> i64 {
    fx.create(CreateParams::new("fixture", &fx.data).profile(profile).lead("deepseek"))
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_continuous_goal_must_follow_a_real_inspection() {
    let phase = Arc::new(AtomicUsize::new(0));
    let premature = Arc::new(AtomicUsize::new(0));
    let missing = Arc::new(AtomicUsize::new(0));
    let later = format!(
        "{}\n{}",
        cmd("SA_GOAL", json!({"objective": "second milestone", "evidence": "local requirement exists"})),
        cmd("SA_NEXT", json!({"body": "execute it"}))
    );
    let (ph, pr, mi) = (phase.clone(), premature.clone(), missing.clone());
    let mut fx = setup(script(move |c| {
        let (ph, pr, mi, later) = (ph.clone(), pr.clone(), mi.clone(), later.clone());
        async move {
            if c.phase() == Some("work") {
                if ph.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Ok(format!("{}\n{}", cmd("SA_LEARN", json!({"lesson": "remember exact bytes"})), done()));
                }
                assert_eq!(c.packet["objective"], json!("second milestone"));
                c.prove();
                return Ok(done());
            }
            let full_packet = c.full_packet();
            assert_eq!(full_packet["autonomy"]["learning"], json!("remember exact bytes"));
            if full_packet["autonomy"]["milestone"] == json!(1) {
                if pr.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Ok(format!(
                        "{}\n{later}",
                        cmd("SA_PROGRESS", json!({"id": "inspection", "summary": "已检查项目。"}))
                    ));
                }
                c.prove();
                return Ok(later);
            }
            if mi.fetch_add(1, Ordering::SeqCst) == 0 {
                return Ok(ended());
            }
            c.prove();
            Ok(ended())
        }
    }));
    let tid = create(&fx, "auto").await;
    fx.engine.start().unwrap();
    fx.until(|| !fx.all("SELECT seq FROM events WHERE kind='auto.milestone'").is_empty()).await;
    fx.restart(quiet()).await;
    fx.engine.start().unwrap();
    fx.finish(tid, "done").await;
    let counts = fx.all(&format!(
        "SELECT sum(kind='auto.milestone') milestones,sum(kind='auto.exhausted') exhausted,
    sum(kind='auto.discovered') discovered FROM events WHERE ticket={tid}"
    ));
    assert_eq!(
        Value::Object(counts[0].clone()),
        json!({"milestones": 2, "exhausted": 1, "discovered": 1})
    );
    assert_eq!(
        fx.int(&format!("SELECT count(*) FROM events WHERE ticket={tid} AND kind='job.progress'")),
        1
    );
    assert!(premature.load(Ordering::SeqCst) >= 2 && missing.load(Ordering::SeqCst) >= 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continuous_runs_pass_old_budget_and_stop_requires_explicit_resume() {
    let entered = Arc::new(AtomicBool::new(false));
    let en = entered.clone();
    let mut fx = setup(script(move |c| {
        let en = en.clone();
        async move {
            if c.turn() == 1 {
                en.store(true, Ordering::SeqCst);
                c.cancel.cancelled().await;
            }
            Ok(done())
        }
    }));
    let id = create(&fx, "auto").await;
    fx.engine
        .db()
        .with(|q| q.run("UPDATE tickets SET auto_cycles=200 WHERE id=?", &[&id]))
        .unwrap();
    fx.engine.start().unwrap();
    fx.until(|| entered.load(Ordering::SeqCst)).await;
    fx.engine.cancel(id).unwrap();
    fx.until(|| fx.engine.active_count() == 0).await;
    fx.restart(quiet()).await;
    fx.engine.start().unwrap();
    assert_eq!(root_state(&fx.engine, id), "cancelled");
    assert!(fx.engine.follow(id, "inadvertent follow", false).is_err());
    fx.engine.follow(id, "repair explicitly", true).unwrap();
    assert_eq!(root_state(&fx.engine, id), "queued");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupted_turn_cannot_replay_a_writer_or_a_delivered_receipt() {
    let stopping = Arc::new(AtomicBool::new(true));
    let child_id = Arc::new(Mutex::new(None::<i64>));
    let root_id = Arc::new(Mutex::new(None::<i64>));
    let text = "原样\r\n  ";
    let send = cmd(
        "SA_SEND",
        json!({"id": "once", "to": "claude", "body": "isolated", "permission_profile": "readonly"}),
    );
    let progress = cmd("SA_PROGRESS", json!({"id": "start-check", "summary": "开始核对记录。"}));
    let (st, ch, ro) = (stopping.clone(), child_id.clone(), root_id.clone());
    let mut fx = setup(script(move |c| {
        let (st, ch, ro, send, progress) = (st.clone(), ch.clone(), ro.clone(), send.clone(), progress.clone());
        async move {
            if c.depth() > 0 {
                *ch.lock().unwrap() = Some(c.job());
                return Ok("actual report".into());
            }
            *ro.lock().unwrap() = Some(c.job());
            if c.phase() == Some("discover") {
                c.prove();
                return Ok(ended());
            }
            if st.load(Ordering::SeqCst) {
                c.emit("delta", json!(format!("{progress}\n{send}\n")));
                c.cancel.cancelled().await;
                return Ok("interrupted".into());
            }
            Ok(format!("{progress}\n{send}\n{}", done()))
        }
    }));
    let id = create(&fx, "auto").await;
    fx.engine.start().unwrap();
    fx.until(|| {
        child_id.lock().unwrap().is_some_and(|c| {
            fx.all(&format!("SELECT state FROM jobs WHERE id={c}"))
                .first()
                .is_some_and(|r| r["state"] == json!("done"))
        })
    })
    .await;
    let root = root_id.lock().unwrap().unwrap();
    let seq = fx.engine.note(id, root, text).unwrap();
    stopping.store(false, Ordering::SeqCst);
    fx.restart(quiet()).await;
    fx.engine.start().unwrap();
    fx.finish(id, "done").await;
    assert_eq!(fx.shared.count(|c| c.packet["depth"].as_i64() > Some(0)), 1);
    assert_eq!(fx.all("SELECT seq FROM events WHERE kind='send'").len(), 1);
    assert_eq!(fx.all("SELECT seq FROM events WHERE kind='job.progress'").len(), 1);
    let notes = fx.engine.notes(root).unwrap();
    let note = notes.iter().find(|n| n["seq"] == json!(seq)).unwrap();
    assert_eq!(note["body"], json!(text));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabled_models_provider_injection_and_escalation_are_rejected() {
    let commands: Arc<Mutex<std::collections::HashMap<i64, String>>> = Arc::default();
    let c2 = commands.clone();
    let fx = setup(script(move |c| {
        let c2 = c2.clone();
        async move {
            let ticket = c.packet["ticket"].as_i64().unwrap();
            Ok(c2.lock().unwrap().get(&ticket).cloned().unwrap_or_default())
        }
    }));
    for overrides in [
        json!({"model": "removed"}),
        json!({"model": "elsewhere"}),
        json!({"permission_profile": "full"}),
        json!({"provider": "foreign"}),
    ] {
        let id = create(&fx, "ask").await;
        let mut v = json!({"id": "x", "to": "claude", "body": "deny"});
        for (k, val) in overrides.as_object().unwrap() {
            v[k] = val.clone();
        }
        commands.lock().unwrap().insert(id, cmd("SA_SEND", v));
    }
    fx.engine.start().unwrap();
    let ids: Vec<i64> = commands.lock().unwrap().keys().copied().collect();
    fx.until(|| ids.iter().all(|i| root_state(&fx.engine, *i) == "done")).await;
    assert_eq!(fx.shared.count(|c| c.packet["depth"].as_i64() > Some(0)), 0);
    for id in &ids {
        assert_eq!(fx.engine.root(*id).unwrap()["result"], json!(commands.lock().unwrap()[id]));
    }
    let receipts = payloads(&fx, "SELECT payload FROM events WHERE kind='protocol.rejected'");
    assert_eq!(receipts.len(), ids.len());
    assert!(receipts.iter().all(|r| r["executed"] == false));
    let mut reasons: Vec<String> = receipts
        .iter()
        .map(|r| r["reason"].as_str().unwrap().to_string())
        .collect();
    reasons.sort();
    let mut expected = vec![
        "所选模型不在此任务的供应商和执行端目录".to_string(),
        "所选模型不在此任务的供应商和执行端目录".to_string(),
        "子任务权限不能超出上级范围".to_string(),
        "不可通过派工更换供应商或执行端".to_string(),
    ];
    expected.sort();
    assert_eq!(reasons, expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_job_cannot_hang_on_delegated_manual_approval() {
    let noted = Arc::new(AtomicBool::new(false));
    let verdict: Arc<Mutex<Option<bool>>> = Arc::default();
    let child_entered = Arc::new(AtomicBool::new(false));
    let gate = Gate::new();
    let (no, ve, ce, g) = (noted.clone(), verdict.clone(), child_entered.clone(), gate.clone());
    let fx = setup(script(move |c| {
        let (no, ve, ce, g) = (no.clone(), ve.clone(), ce.clone(), g.clone());
        async move {
            if c.depth() > 0 {
                ce.store(true, Ordering::SeqCst);
                let ok = c.host.approve(json!({"operation": "edit"})).await;
                *ve.lock().unwrap() = Some(ok);
                g.wait().await;
                return Ok("checked".into());
            }
            if c.phase() == Some("discover") {
                c.prove();
                return Ok(ended());
            }
            if c.turn() == 1 {
                return Ok(cmd(
                    "SA_SEND",
                    json!({"id": "limited", "to": "deepseek", "body": "read", "permission_profile": "ask"}),
                ));
            }
            if c.packet["notes"].as_array().is_some_and(|n| !n.is_empty()) && !no.load(Ordering::SeqCst) {
                no.store(true, Ordering::SeqCst);
                g.open();
            }
            Ok(done())
        }
    }));
    let id = create(&fx, "auto").await;
    fx.engine.start().unwrap();
    fx.until(|| {
        child_entered.load(Ordering::SeqCst)
            && root_state(&fx.engine, id) == "waiting"
            && verdict.lock().unwrap().is_some()
    })
    .await;
    let root = fx.engine.root(id).unwrap()["id"].as_i64().unwrap();
    fx.engine.note(id, root, "check progress now").unwrap();
    fx.finish(id, "done").await;
    assert_eq!(*verdict.lock().unwrap(), Some(false));
    assert!(noted.load(Ordering::SeqCst));
    assert_eq!(fx.all("SELECT seq FROM events WHERE kind='approval.unattended_decline'").len(), 1);
}

/// Extension service of T2:245-261.
struct FixtureExtensions {
    invokes: Arc<AtomicUsize>,
    receipts: Arc<Mutex<Vec<Value>>>,
}

impl Extensions for FixtureExtensions {
    fn prepare(&self, params: Option<Value>, _dir: &Path) -> BoxFuture<'static, anyhow::Result<Value>> {
        async move { Ok(params.unwrap_or(Value::Null)) }.boxed()
    }
    fn bind(&self, _conn: &Connection, _ticket: i64, _prepared: &Value) -> anyhow::Result<()> {
        Ok(())
    }
    fn context(&self, _ticket: i64, _profile: &str) -> Value {
        json!({"skills": [{"name": "fixture", "body": "test"}], "mcp": [{"server": "local", "tools": [{"name": "sum"}]}]})
    }
    fn results(&self, _job: i64) -> Vec<Value> {
        self.receipts.lock().unwrap().clone()
    }
    fn invoke(&self, args: InvokeArgs) -> BoxFuture<'static, anyhow::Result<InvokeResult>> {
        let (invokes, receipts) = (self.invokes.clone(), self.receipts.clone());
        async move {
            assert_eq!(args.command["id"], json!("calc"));
            assert_eq!(args.profile, "full");
            assert!(args.admission > 0);
            invokes.fetch_add(1, Ordering::SeqCst);
            receipts.lock().unwrap().push(json!({"id": "calc", "result": 2}));
            Ok(InvokeResult { new_receipt: true })
        }
        .boxed()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_effects_are_final_only_and_need_another_turn_for_receipts() {
    let invokes = Arc::new(AtomicUsize::new(0));
    let receipts: Arc<Mutex<Vec<Value>>> = Arc::default();
    let opts = EngineOptions {
        extensions: Some(Arc::new(FixtureExtensions {
            invokes: invokes.clone(),
            receipts: receipts.clone(),
        })),
        ..quiet()
    };
    let (inv, rec) = (invokes.clone(), receipts.clone());
    let fx = setup_with(
        script(move |c| {
            let (inv, rec) = (inv.clone(), rec.clone());
            async move {
                assert_eq!(c.full_packet()["project_extensions"]["skills"][0]["name"], json!("fixture"));
                if c.turn() == 1 {
                    let command = cmd(
                        "SA_TOOL",
                        json!({"id": "calc", "server": "local", "tool": "sum", "arguments": {"x": 1}}),
                    );
                    c.emit("delta", json!(format!("{command}\n")));
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    assert_eq!(inv.load(Ordering::SeqCst), 0);
                    return Ok(command);
                }
                assert_eq!(c.packet["mcp_results"], Value::Array(rec.lock().unwrap().clone()));
                Ok("tool returned 2".into())
            }
        }),
        opts,
    );
    let id = create(&fx, "full").await;
    fx.engine.start().unwrap();
    fx.finish(id, "done").await;
    assert_eq!(invokes.load(Ordering::SeqCst), 1);
    assert_eq!(fx.engine.root(id).unwrap()["turn"], json!(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readonly_child_cannot_launder_the_grant_through_a_grandchild() {
    let fx = setup(script(|c| async move {
        if c.depth() == 0 && c.turn() == 1 {
            return Ok(cmd(
                "SA_SEND",
                json!({"id": "narrow", "to": "claude", "body": "inspect", "permission_profile": "readonly"}),
            ));
        }
        if c.depth() == 1 && c.turn() == 1 {
            return Ok(cmd("SA_SEND", json!({"id": "pass", "to": "codex", "body": "second opinion"})));
        }
        if c.depth() == 2 {
            assert_eq!(c.req.profile, "readonly");
            assert!(!c.req.writable);
            assert_eq!(c.packet["allow_lead_routing"], json!(false));
        }
        Ok("verified".into())
    }));
    let id = create(&fx, "full").await;
    fx.engine.start().unwrap();
    fx.finish(id, "done").await;
    assert_eq!(fx.shared.count(|c| c.packet["depth"] == json!(2)), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn human_redirect_during_discovery_suppresses_old_sends_and_completion() {
    let fx = setup(script(|c| async move {
        if c.turn() == 1 {
            return Ok(format!(
                "{}\n{}",
                cmd("SA_SEND", json!({"id": "stale", "to": "claude", "body": "must not run"})),
                done()
            ));
        }
        if c.phase() == Some("discover") {
            c.prove();
            return Ok(ended());
        }
        assert_eq!(c.packet["user_updates"], json!(["new objective"]));
        Ok(done())
    }));
    let (entered, gate) = (Gate::new(), Gate::new());
    let (en, g) = (entered.clone(), gate.clone());
    fx.shared.set_catalog(Arc::new(move || {
        let (en, g) = (en.clone(), g.clone());
        async move {
            en.open();
            g.wait().await;
            Ok(t2_catalog())
        }
        .boxed()
    }));
    let id = create(&fx, "auto").await;
    fx.engine.start().unwrap();
    fx.engine.step();
    entered.wait().await;
    fx.engine.follow(id, "new objective", false).unwrap();
    gate.open();
    fx.finish(id, "done").await;
    assert!(fx.all("SELECT id FROM jobs WHERE parent IS NOT NULL").is_empty());
    let turns: Vec<Value> = fx
        .all(&format!("SELECT turn FROM events WHERE ticket={id} AND kind='auto.milestone'"))
        .iter()
        .map(|r| r["turn"].clone())
        .collect();
    assert_eq!(turns, vec![json!(2)]);
}
