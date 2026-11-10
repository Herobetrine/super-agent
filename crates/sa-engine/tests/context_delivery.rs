mod common;

use common::*;
use sa_engine::adapter::{AdapterError, ErrorClass};
use sa_engine::context::delivery::{self, Baseline};
use sa_engine::protocol::{digest, stringify};
use sa_engine::services::{Extensions, InvokeArgs, InvokeResult, NoopExtensions};
use futures::future::BoxFuture;
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};

fn object(value: Value) -> Map<String, Value> { value.as_object().unwrap().clone() }

struct TestExtensions(Mutex<Value>);

impl Extensions for TestExtensions {
    fn prepare(&self, params: Option<Value>, dir: &std::path::Path) -> BoxFuture<'static, anyhow::Result<Value>> {
        NoopExtensions.prepare(params, dir)
    }
    fn bind(&self, conn: &rusqlite::Connection, ticket: i64, prepared: &Value) -> anyhow::Result<()> {
        NoopExtensions.bind(conn, ticket, prepared)
    }
    fn context(&self, _ticket: i64, _profile: &str) -> Value { self.0.lock().unwrap().clone() }
    fn can_invoke(&self) -> bool { false }
    fn invoke(&self, args: InvokeArgs) -> BoxFuture<'static, anyhow::Result<InvokeResult>> {
        NoopExtensions.invoke(args)
    }
}

fn restore(old: &Baseline, view: &Map<String, Value>) -> Map<String, Value> {
    let mut packet = old.packet.clone();
    packet.insert("previous_reply".into(), json!(old.reply));
    let meta = &view["context_delivery"];
    for name in meta["removed_fields"].as_array().unwrap() {
        packet.remove(name.as_str().unwrap());
    }
    for (name, value) in view {
        if matches!(name.as_str(), "context_delivery" | "full_context") { continue; }
        match meta["array_modes"][name].as_str() {
            Some("append") => packet.get_mut(name).unwrap().as_array_mut().unwrap()
                .extend(value.as_array().unwrap().clone()),
            Some("upsert") => {
                let id = if name == "roster" { "job" } else { "id" };
                let rows = packet.get_mut(name).unwrap().as_array_mut().unwrap();
                for row in value.as_array().unwrap() {
                    if let Some(old) = rows.iter_mut().find(|old| old[id] == row[id]) {
                        *old = row.clone();
                    } else {
                        rows.push(row.clone());
                    }
                }
            }
            _ if meta["object_modes"].get(name).is_some() => {
                let fields = packet.get_mut(name).unwrap().as_object_mut().unwrap();
                for key in meta["object_modes"][name]["removed_fields"].as_array().unwrap() {
                    fields.remove(key.as_str().unwrap());
                }
                fields.extend(value.as_object().unwrap().clone());
            }
            _ => { packet.insert(name.clone(), value.clone()); }
        }
    }
    packet
}

#[test]
fn delta_restores_complete_semantics_without_truncating_new_reports() {
    let guide = "guide".repeat(10_000);
    let original = object(json!({
        "job":1,"turn":1,"permission_profile":"full","goal":"goal","previous_reply":"prior",
        "project_extensions":{"skills":["skill".repeat(100_000)]},
        "project_knowledge":["stable"],"notes":[{"seq":1,"body":"a"}],
        "user_updates":["first"],"mcp_results":[{"id":1}],
        "reports":[{"id":2,"state":"running","result":null},{"id":3,"state":"done","result":"old"}],
        "roster":[{"job":1,"state":"running"},{"job":2,"state":"running"}],
        "autonomy":{"phase":"execute","learning":"learning".repeat(20_000),"milestone":"old"},
        "remove_me":"stale"
    }));
    let old = Baseline { packet:original.clone(), guide_sha256:digest(&guide), snapshot_sha256:"sha".into(), reply:"last success".into() };
    let mut next = original;
    next.insert("turn".into(), json!(2));
    next.insert("permission_profile".into(), json!("auto"));
    next.insert("previous_reply".into(), json!("last success"));
    next.insert("reports".into(), json!([
        {"id":2,"state":"done","result":"全文首行\n".repeat(25_000)},
        {"id":3,"state":"done","result":"old"}, {"id":4,"state":"running","result":null}
    ]));
    next.insert("notes".into(), json!([{"seq":1,"body":"a"},{"seq":2,"body":"new"}]));
    next.insert("user_updates".into(), json!(["first","first"]));
    next.insert("mcp_results".into(), json!([{"id":1},{"id":2}]));
    next.insert("roster".into(), json!([{"job":1,"state":"running"},{"job":2,"state":"done"}]));
    next.get_mut("autonomy").unwrap()["milestone"] = json!("new");
    next.remove("remove_me");
    let (view, delivered_guide) = delivery::delta(&next, &guide, &old);
    assert_eq!(view["reports"][0]["result"], next["reports"][0]["result"]);
    assert_eq!(view["reports"].as_array().unwrap().len(), 2);
    assert_eq!(restore(&old, &view), next);
    assert!(delivered_guide.len() < guide.len() / 10);
    assert!(!view.contains_key("project_extensions"));
    assert!(view["autonomy"].get("learning").is_none());
}

#[test]
fn removals_rewrites_and_guide_changes_are_explicit() {
    let old = Baseline {
        packet: object(json!({"reports":[{"id":1},{"id":2}],"notes":["a","b"],"autonomy":{"phase":"execute","obsolete":true}})),
        guide_sha256:digest("old guide"), snapshot_sha256:"sha".into(), reply:"reply".into(),
    };
    let next = object(json!({"reports":[{"id":2}],"notes":["rewritten"],"autonomy":{"phase":"discover"},"previous_reply":"reply"}));
    let (view, guide) = delivery::delta(&next, "new guide", &old);
    assert_eq!(restore(&old, &view), next);
    assert!(view["context_delivery"]["array_modes"].get("reports").is_none());
    assert!(guide.starts_with("new guide\n"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_replay_reduces_bytes_and_survives_restart_with_verified_full_archive() {
    let extensions = Arc::new(TestExtensions(Mutex::new(json!({"skills":["retained skill\n".repeat(30_000)],"mcp":[]}))));
    let options = || sa_engine::EngineOptions { extensions:Some(extensions.clone()), ..quiet() };
    let mut fx = Fx::with(script(|_| async { Ok("successful reply".into()) }), t1_catalog(), options());
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "done").await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    let initial = fx.shared.calls(|c| (c.req.turn == 1).then(|| c.packet.clone())).pop().unwrap();
    assert_eq!(initial["project_extensions"]["skills"], extensions.0.lock().unwrap()["skills"]);
    fx.restart(options()).await;
    fx.engine.follow(ticket, "继续", false).unwrap();
    fx.engine.start().unwrap();
    fx.finish(ticket, "done").await;
    let call = fx.shared.calls(|c| (c.req.turn == 2).then(|| c.packet.clone())).pop().unwrap();
    assert_eq!(call["context_delivery"]["mode"], "delta");
    assert!(call.get("project_extensions").is_none());
    assert!(call.get("delegate_models").is_none());
    assert!(initial["delegate_models"].as_object().is_some_and(|models| !models.is_empty()));
    let full = &call["full_context"];
    let raw = std::fs::read(full["object_path"].as_str().unwrap()).unwrap();
    assert_eq!(sha(&raw), full["sha256"].as_str().unwrap());
    let original: Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(original["user_updates"], json!(["继续"]));
    assert_eq!(original["job"], root);
    assert_eq!(original["project_extensions"]["skills"], initial["project_extensions"]["skills"]);
    assert!(call.get("previous_reply").is_none());
    let checkpoint = fx.engine.db().with(|q| {
        let seq = q.int("SELECT max(seq) FROM events WHERE kind='reply'", &[])?;
        sa_engine::checkpoints::capture(q, ticket, seq)
    }).unwrap();
    assert_eq!(checkpoint["input"]["project_extensions"], original["project_extensions"]);
    assert_eq!(checkpoint["input"]["delegate_models"], original["delegate_models"]);
    let full_input = fx.engine.db().with(|q| delivery::full_input(q, root, i64::MAX)).unwrap().unwrap();
    let failover_context: Value = serde_json::from_str(full_input["payload"].as_str().unwrap()).unwrap();
    assert_eq!(failover_context["delegate_models"], original["delegate_models"]);
    assert!(failover_context["delegate_models"].as_object().is_some_and(|models| !models.is_empty()));
    let metrics = payloads(&fx, "SELECT payload FROM events WHERE kind='running' ORDER BY seq DESC LIMIT 1").pop().unwrap();
    assert!(metrics["sent_prompt_bytes"].as_u64().unwrap() * 100 < metrics["full_prompt_bytes"].as_u64().unwrap());
    eprintln!("engine continuation bytes full={} sent={} saved={}", metrics["full_prompt_bytes"], metrics["sent_prompt_bytes"], metrics["saved_bytes"]);
    *extensions.0.lock().unwrap() = json!({"skills":[],"mcp":[]});
    fx.engine.follow(ticket, "撤销技能后继续", false).unwrap();
    fx.finish(ticket, "done").await;
    let revoked = fx.shared.calls(|c| (c.req.turn == 3).then(|| c.packet.clone())).pop().unwrap();
    assert_eq!(revoked["project_extensions"], json!({"skills":[],"mcp":[]}));
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_request_does_not_consume_incremental_updates() {
    let calls = Arc::new(AtomicUsize::new(0));
    let n = calls.clone();
    let fx = Fx::new(script(move |c| {
        let n = n.clone();
        async move {
            let attempt = n.fetch_add(1, Ordering::SeqCst);
            if attempt == 1 {
                assert_eq!(c.packet["user_updates"], json!(["must replay"]));
                return Err(AdapterError::new(ErrorClass::Protocol, "fixture failure"));
            }
            if attempt == 2 { assert_eq!(c.packet["user_updates"], json!(["must replay"])); }
            Ok("success".into())
        }
    }), t1_catalog());
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "done").await;
    fx.engine.follow(ticket, "must replay", false).unwrap();
    fx.finish(ticket, "failed").await;
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='context.delivered'"), 1);
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    fx.engine.retry(root).unwrap();
    fx.finish(ticket, "done").await;
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='context.delivered'"), 2);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_change_corrupt_snapshot_and_restricted_profile_replay_full_input() {
    for mode in ["session", "route", "corrupt", "restricted"] {
        let fx = Fx::new(script(|_| async { Ok("success".into()) }), t1_catalog());
        let profile = if mode == "restricted" { "readonly" } else { "full" };
        let ticket = fx.begin(fx.params().profile(profile)).await;
        fx.finish(ticket, "done").await;
        fx.engine.db().tx(|q| {
            match mode {
                "session" => { q.run("UPDATE jobs SET session='new-session'", &[])?; }
                "route" => { q.run("UPDATE events SET payload=json_set(payload,'$.route.model','different') WHERE kind='context.delivered'", &[])?; }
                "corrupt" => { q.run("UPDATE events SET payload='bad-json' WHERE kind='context.snapshot'", &[])?; }
                _ => {}
            }
            Ok(())
        }).unwrap();
        fx.engine.follow(ticket, "follow", false).unwrap();
        fx.finish(ticket, "done").await;
        let packet = fx.shared.calls(|c| (c.req.turn == 2).then(|| c.packet.clone())).pop().unwrap();
        assert!(packet.get("context_delivery").is_none(), "{mode}");
        assert_eq!(packet["previous_reply"], "success");
        assert!(packet["harness_capabilities"].is_object());
        fx.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_session_native_compaction_forces_next_complete_replay() {
    for signal in [
        json!({"type":"system","subtype":"compact_boundary"}),
        json!({"method":"item/completed","params":{"item":{"type":"contextCompaction"}}}),
    ] {
        let fx = Fx::new(script(move |c| {
            let signal = signal.clone();
            async move {
                if c.turn() == 2 {
                    assert_eq!(c.packet["context_delivery"]["mode"], "delta");
                    c.emit("native", signal);
                }
                if c.turn() == 3 {
                    assert!(c.req.session_id.is_some());
                    assert!(c.packet.get("context_delivery").is_none());
                    assert!(c.packet["harness_capabilities"].is_object());
                }
                Ok("success".into())
            }
        }), t1_catalog());
        let ticket = fx.begin(fx.params().profile("full")).await;
        fx.finish(ticket, "done").await;
        for update in ["first follow", "second follow"] {
            fx.engine.follow(ticket, update, false).unwrap();
            fx.finish(ticket, "done").await;
        }
        assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='context.compacted'"), 1);
        fx.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_finishing_after_parent_snapshot_is_delivered_next_turn_in_full() {
    let child_release = Gate::new();
    let parent_entered = Gate::new();
    let parent_release = Gate::new();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let (child, entered, release, reports) = (child_release.clone(), parent_entered.clone(), parent_release.clone(), observed.clone());
    let fx = Fx::new(script(move |c| {
        let (child, entered, release, reports) = (child.clone(), entered.clone(), release.clone(), reports.clone());
        async move {
            if c.depth() > 0 {
                child.wait().await;
                return Ok("完整报告首行\n证据末行\n".into());
            }
            if c.turn() == 1 { return Ok(send(json!({}))); }
            if c.turn() == 2 {
                assert_eq!(c.packet["reports"][0]["state"], "running");
                entered.open();
                release.wait().await;
            } else {
                reports.lock().unwrap().push(c.packet["reports"].clone());
            }
            Ok("parent reply".into())
        }
    }), t1_catalog());
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.until(|| fx.shared.count(|c| c.packet["depth"] == 1) == 1).await;
    fx.until(|| root_state(&fx.engine, ticket) == "waiting").await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    fx.engine.note(ticket, root, "progress").unwrap();
    fx.until(|| parent_entered.is_open()).await;
    child_release.open();
    fx.until(|| fx.int("SELECT count(*) FROM jobs WHERE depth=1 AND state='done'") == 1).await;
    parent_release.open();
    fx.finish(ticket, "done").await;
    {
        let rows = observed.lock().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0]["state"], "done");
        assert_eq!(rows[0][0]["result"], "完整报告首行\n证据末行\n");
    }
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE job=(SELECT min(id) FROM jobs) AND kind='context.delivered'"), 3);
    fx.engine.close().await;
}

#[test]
fn large_unchanged_context_has_more_than_ninety_nine_percent_byte_reduction() {
    let guide = "platform guide\n".repeat(2000);
    let packet = object(json!({
        "job":1,"turn":1,"permission_profile":"full","goal":"goal","assignment":"assignment",
        "previous_reply":"reply","project_extensions":{"skills":["native stable skill\n".repeat(20000)]},
        "autonomy":{"phase":"execute","learning":"retained learning\n".repeat(2000)},
        "reports":[{"id":2,"state":"done","result":"retained report\n".repeat(2000)}],
        "notes":[{"seq":1,"body":"retained note\n".repeat(1000)}]
    }));
    let baseline = Baseline { packet:packet.clone(), guide_sha256:digest(&guide), snapshot_sha256:"sha".into(), reply:"reply".into() };
    let (view, guide_view) = delivery::delta(&packet, &guide, &baseline);
    assert_eq!(restore(&baseline, &view), packet);
    let full_bytes = guide.len() + 1 + stringify(&Value::Object(packet)).len();
    let sent_bytes = guide_view.len() + 1 + stringify(&Value::Object(view)).len();
    assert!(sent_bytes * 100 < full_bytes, "full={full_bytes} sent={sent_bytes}");
    eprintln!("synthetic steady-state bytes full={full_bytes} sent={sent_bytes} reduction={:.3}%", (1.0 - sent_bytes as f64 / full_bytes as f64) * 100.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_efficiency_settings_switch_on_off_on_between_turns() {
    let efficiency = sa_store::efficiency::shared(Default::default());
    let opts = sa_engine::EngineOptions { efficiency: efficiency.clone(), ..quiet() };
    let fx = Fx::with(script(|_| async { Ok("success".into()) }), t1_catalog(), opts);
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "done").await;
    assert!(fx.shared.calls(|c| Some(c.req.prompt.clone())).pop().unwrap().contains("Keep routine handoffs concise"));
    for (update, enabled) in [("enabled continuation", true), ("disabled continuation", false), ("enabled again", true)] {
        {
            let mut settings = efficiency.write().unwrap();
            settings.incremental_context = enabled;
            settings.concise_handoffs = enabled;
        }
        fx.engine.follow(ticket, update, false).unwrap();
        fx.finish(ticket, "done").await;
        let (packet, prompt) = fx.shared.calls(|c| Some((c.packet.clone(), c.req.prompt.clone()))).pop().unwrap();
        assert_eq!(packet["context_delivery"]["mode"] == "delta", enabled);
        assert_eq!(packet["collaboration_preferences"]["concise_handoffs"], enabled);
        assert_eq!(prompt.contains("Keep routine handoffs concise"), update == "enabled again");
        if !enabled {
            assert!(packet["harness_capabilities"].is_object());
            assert_eq!(packet["goal"], fx.params().goal);
            assert!(prompt.contains("earlier concise-handoff guidance no longer applies"));
            assert!(prompt.contains("When blocked only on peers, end this turn promptly"));
            assert!(prompt.contains("Keep read-only work read-only"));
            assert!(!prompt.contains("Give peers self-contained scoped assignments"));
        }
    }
    fx.engine.close().await;
}
