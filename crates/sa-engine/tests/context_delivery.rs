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
    fn can_invoke(&self) -> bool { true }
    fn invoke(&self, args: InvokeArgs) -> BoxFuture<'static, anyhow::Result<InvokeResult>> {
        let mut context = self.0.lock().unwrap();
        let authorized = context["mcp"].as_array().into_iter().flatten()
            .filter(|server| server["server"] == args.command["server"])
            .flat_map(|server| server["tools"].as_array().into_iter().flatten())
            .any(|tool| tool["name"] == args.command["tool"] && tool.get("inputSchema").is_some());
        if authorized {
            context["invoke_count"] = json!(context["invoke_count"].as_u64().unwrap_or_default() + 1);
        }
        Box::pin(async move {
            anyhow::ensure!(authorized, "fixture requires exact authorized schema");
            Ok(InvokeResult { new_receipt: false })
        })
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
    let old = Baseline { packet:original.clone(), guide_sha256:digest(&guide), snapshot_sha256:"sha".into(), reply:"last success".into(), lazy_mcp_schemas:false, needs_recovery:false };
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
        lazy_mcp_schemas:false, needs_recovery:false,
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
async fn same_session_native_compaction_establishes_self_contained_recovery_epoch() {
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
                    assert_eq!(c.packet["context_delivery"]["mode"], "recovery");
                    assert!(c.packet["harness_capabilities"].is_object());
                    assert_eq!(c.packet["history_index"]["source"], c.packet["full_context"]);
                    read_archive(&c.packet);
                }
                if c.turn() == 4 {
                    assert_eq!(c.packet["context_delivery"]["mode"], "delta");
                }
                Ok("success".into())
            }
        }), t1_catalog());
        let ticket = fx.begin(fx.params().profile("full")).await;
        fx.finish(ticket, "done").await;
        for update in ["first follow", "second follow", "third follow"] {
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
    let baseline = Baseline { packet:packet.clone(), guide_sha256:digest(&guide), snapshot_sha256:"sha".into(), reply:"reply".into(), lazy_mcp_schemas:false, needs_recovery:false };
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

fn schema_fixture() -> Value {
    let tools: Vec<_> = (0..24).map(|index| json!({
        "name":format!("tool_{index}"),
        "description":format!("{}完整操作约束末行", "工具说明🧭e\u{301}".repeat(100)),
        "inputSchema":{
            "type":"object","required":["choice"],"additionalProperties":false,
            "properties":{"choice":{"type":"string","enum":[format!("精确选项_{index}_{}", "x".repeat(1500))]}},
        },
    })).collect();
    json!({"skills":[],"mcp":[
        {"server":"catalog","name":"large fixture","tools":tools},
        {"server":"small","name":"small fixture","tools":[{"name":"inspect","description":"short","inputSchema":{"type":"object"}}]},
    ]})
}

fn read_archive(packet: &Value) -> Value {
    let reference = &packet["full_context"];
    let raw = std::fs::read(reference["object_path"].as_str().unwrap()).unwrap();
    assert_eq!(raw.len() as u64, reference["bytes"].as_u64().unwrap());
    assert_eq!(sha(&raw), reference["sha256"].as_str().unwrap());
    serde_json::from_slice(&raw).unwrap()
}

#[test]
fn recovery_is_self_contained_with_bounded_exact_history_and_full_current_work() {
    let mut reports: Vec<_> = (1..=80).map(|id| json!({
        "id":id,"sender":"codex","state":"done","assignment":"assignment".repeat(500),
        "result":format!("report {id} {}", "全文".repeat(3000)),
    })).collect();
    reports.push(json!({"id":81,"sender":"claude","state":"running","assignment":"active assignment","result":null}));
    let mut packet = object(json!({
        "permission_profile":"full","goal":"完整目标","objective":"完整当前目标","assignment":"完整分工",
        "harness_capabilities":{"codex":{"write_project":true}},"project_extensions":{"skills":["完整技能"]},
        "reports":reports,"notes":[
            {"seq":1,"sender":"codex (#1)","body":"old child note"},
            {"seq":2,"sender":"user","body":"仍须遵守的用户要求"},
            {"seq":3,"sender":"platform","body":"未解决审批"},
            {"seq":4,"sender":"unknown","body":"归属不明，保留全文"}
        ],"previous_reply":"old successful reply","user_updates":["first requirement","最新要求全文"],
        "plan":[{"id":1,"done":false,"body":"未解决决策全文"}],"decisions":[{"resolved":false,"body":"未决事项"}]
    }));
    let old = Baseline { packet:packet.clone(), guide_sha256:digest("guide"), snapshot_sha256:"old-sha".into(), reply:"old successful reply".into(), lazy_mcp_schemas:false, needs_recovery:true };
    packet["reports"][79]["result"] = json!("changed report must remain complete");
    packet["reports"].as_array_mut().unwrap().push(json!({"id":82,"sender":"codex","state":"done","assignment":"new assignment","result":"new complete report"}));
    packet["notes"].as_array_mut().unwrap().push(json!({"seq":5,"sender":"codex (#1)","body":"new report note"}));
    let dir = tempfile::tempdir().unwrap();
    let archive = sa_engine::context::archive(&packet, dir.path()).unwrap();
    let (view, guide) = delivery::recovery(&packet, "guide", &old, &archive);
    assert!(guide.starts_with("guide\n"));
    for field in ["permission_profile","goal","objective","assignment","harness_capabilities","project_extensions","user_updates","plan","decisions"] {
        assert_eq!(view[field], packet[field], "{field}");
    }
    assert_eq!(view["reports"], json!([packet["reports"][79],packet["reports"][80],packet["reports"][81]]));
    assert_eq!(view["notes"], json!([packet["notes"][1],packet["notes"][2],packet["notes"][3],packet["notes"][4]]));
    assert_eq!(view["history_index"]["reports"]["archived_count"], 79);
    assert_eq!(view["history_index"]["reports"]["entries"].as_array().unwrap().len(), 32);
    assert!(view.get("previous_reply").is_none());
    let raw: Value = serde_json::from_slice(&std::fs::read(archive["object_path"].as_str().unwrap()).unwrap()).unwrap();
    for entry in view["history_index"]["reports"]["entries"].as_array().unwrap().iter()
        .chain(view["history_index"]["notes"]["entries"].as_array().unwrap())
        .chain(std::iter::once(&view["history_index"]["previous_reply"]))
    {
        let exact = raw.pointer(entry["pointer"].as_str().unwrap()).unwrap();
        let canonical = stringify(&sa_engine::context::sort_keys(exact));
        assert_eq!(entry["bytes"], canonical.len());
        assert_eq!(entry["sha256"], digest(&canonical));
    }
    // Later archives and reordered arrays cannot change a prior reference.
    packet["reports"].as_array_mut().unwrap().reverse();
    let later = sa_engine::context::archive(&packet, dir.path()).unwrap();
    assert_ne!(archive["sha256"], later["sha256"]);
    assert_eq!(view["history_index"]["source"], archive);
    // The next delta uses the complete receipt as its comparison source.
    // Appends add complete new material after the indexed history, changes
    // replace whole reports, and removals explicitly replace the array.
    let recovered = Baseline { packet:packet.clone(), guide_sha256:digest("guide"), snapshot_sha256:"recovered".into(), reply:"new successful reply".into(), lazy_mcp_schemas:false, needs_recovery:false };
    let mut next = packet.clone();
    next.insert("previous_reply".into(), json!("new successful reply"));
    next["notes"].as_array_mut().unwrap().push(json!({"seq":6,"sender":"user","body":"new unprocessed decision"}));
    next["reports"][0]["result"] = json!("changed indexed report remains full");
    let (delta, _) = delivery::delta(&next, "guide", &recovered);
    assert_eq!(delta["context_delivery"]["array_modes"]["notes"], "append");
    assert_eq!(delta["notes"], json!([next["notes"].as_array().unwrap().last().unwrap()]));
    assert_eq!(restore(&recovered, &delta), next);
    next["reports"].as_array_mut().unwrap().remove(1);
    next["notes"] = json!([{ "seq":6,"sender":"user","body":"rewritten decision" }]);
    let (delta, _) = delivery::delta(&next, "guide", &recovered);
    assert!(delta["context_delivery"]["array_modes"].get("reports").is_none());
    assert!(delta["context_delivery"]["array_modes"].get("notes").is_none());
    assert_eq!(restore(&recovered, &delta), next);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compacting_deduplicates_only_within_admission_and_invalidates_new_recovery_attempts() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = attempts.clone();
    let fx_script = script(move |c| {
        let count = count.clone();
        async move {
            if c.turn() == 1 {
                c.emit("native", json!({"type":"system","subtype":"status","status":"compacting"}));
                c.emit("native", json!({"type":"system","subtype":"status","status":"compacting"}));
            } else if c.turn() == 2 {
                assert_eq!(c.packet["context_delivery"]["mode"], "recovery");
                c.emit("native", json!({"type":"system","subtype":"status","status":"compacting"}));
                c.emit("native", json!({"type":"system","subtype":"status","status":"compacting"}));
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(AdapterError::new(ErrorClass::Protocol, "fixture recovery failure"));
                }
            } else if c.turn() == 3 {
                assert_eq!(c.packet["context_delivery"]["mode"], "recovery");
            } else {
                assert_eq!(c.packet["context_delivery"]["mode"], "delta");
            }
            Ok("success".into())
        }
    });
    let mut fx = Fx::new(fx_script, t1_catalog());
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "done").await;
    fx.engine.follow(ticket, "recovery", false).unwrap();
    fx.finish(ticket, "failed").await;
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='context.delivered'"), 1);
    fx.restart(quiet()).await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    fx.engine.retry(root).unwrap();
    fx.engine.start().unwrap();
    fx.finish(ticket, "done").await;
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    // The original turn and both recovery attempts have independent starts.
    // Missing completion frames cannot make the successful retry delta-safe.
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='context.compacted'"), 3);
    fx.engine.follow(ticket, "recover after retry compacted again", false).unwrap();
    fx.finish(ticket, "done").await;
    fx.engine.follow(ticket, "new epoch continuation", false).unwrap();
    fx.finish(ticket, "done").await;
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovery_is_disabled_by_incremental_setting_and_damaged_archive_replays_full() {
    for mode in ["disabled", "corrupt"] {
        let efficiency = sa_store::efficiency::shared(Default::default());
        let opts = sa_engine::EngineOptions { efficiency:efficiency.clone(), ..quiet() };
        let fx = Fx::with(script(|c| async move {
            if c.turn() == 1 { c.emit("native", json!({"type":"system","subtype":"compact_boundary"})); }
            Ok("success".into())
        }), t1_catalog(), opts);
        let ticket = fx.begin(fx.params().profile("full")).await;
        fx.finish(ticket, "done").await;
        fx.engine.follow(ticket, "recover", false).unwrap();
        fx.finish(ticket, "done").await;
        let packet = fx.shared.calls(|c| Some(c.packet.clone())).pop().unwrap();
        assert_eq!(packet["context_delivery"]["mode"], "recovery");
        if mode == "disabled" {
            efficiency.write().unwrap().incremental_context = false;
        } else {
            std::fs::write(packet["full_context"]["object_path"].as_str().unwrap(), b"damaged").unwrap();
        }
        fx.engine.follow(ticket, "full replay", false).unwrap();
        fx.finish(ticket, "done").await;
        let packet = fx.shared.calls(|c| Some(c.packet.clone())).pop().unwrap();
        assert!(packet.get("context_delivery").is_none(), "{mode}");
        assert_eq!(packet["previous_reply"], "success");
        assert!(packet["harness_capabilities"].is_object());
        fx.engine.close().await;
    }
}

#[test]
#[ignore = "requires explicitly supplied read-only production snapshot replay paths"]
fn replay_compacted_production_turns_without_models_or_database_writes() {
    let database = std::env::var("SA_CONTEXT_REPLAY_DB").expect("read-only database path");
    let output = std::env::var("SA_CONTEXT_REPLAY_OUTPUT").expect("numeric receipt path");
    let conn = rusqlite::Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    conn.execute_batch("PRAGMA query_only=ON; BEGIN").unwrap();
    let mut metrics = Vec::new();
    let dir = tempfile::tempdir().unwrap();
    for turn in [7, 8, 9] {
        let (seq, payload, payload_sha): (i64, String, String) = conn.query_row(
            "SELECT seq,payload,sha256 FROM events WHERE ticket=7 AND job=100 AND turn=? AND kind='context.snapshot' ORDER BY seq LIMIT 1",
            [turn], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).unwrap();
        assert_eq!(digest(&payload), payload_sha);
        let packet = object(serde_json::from_str(&payload).unwrap());
        let receipt: String = conn.query_row(
            "SELECT payload FROM events WHERE job=100 AND seq<? AND kind='context.delivered' ORDER BY seq DESC LIMIT 1",
            [seq], |row| row.get(0),
        ).unwrap();
        let receipt: Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(receipt["complete"], true);
        let before: String = conn.query_row("SELECT payload FROM events WHERE seq=? AND kind='context.snapshot'", [receipt["snapshot_seq"].as_i64().unwrap()], |row| row.get(0)).unwrap();
        let previous = object(serde_json::from_str(&before).unwrap());
        assert_eq!(digest(&stringify(&sa_engine::context::sort_keys(&Value::Object(previous.clone())))), receipt["snapshot_sha256"]);
        let reply: String = conn.query_row("SELECT payload FROM events WHERE seq=? AND kind='reply'", [receipt["reply_seq"].as_i64().unwrap()], |row| row.get(0)).unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["session"], receipt["session"]);
        let old = Baseline {
            packet:previous, guide_sha256:receipt["guide_sha256"].as_str().unwrap().into(),
            snapshot_sha256:receipt["snapshot_sha256"].as_str().unwrap().into(),
            reply:reply["text"].as_str().unwrap().into(), lazy_mcp_schemas:receipt["lazy_mcp_schemas"].as_bool().unwrap_or(false), needs_recovery:true,
        };
        let running: String = conn.query_row("SELECT payload FROM events WHERE job=100 AND seq>? AND kind='running' ORDER BY seq LIMIT 1", [seq], |row| row.get(0)).unwrap();
        let running: Value = serde_json::from_str(&running).unwrap();
        let guide = running["guide"].as_str().unwrap();
        let archive = sa_engine::context::archive(&packet, dir.path()).unwrap();
        let (mut view, delivered_guide) = delivery::recovery(&packet, guide, &old, &archive);
        delivery::defer_mcp_schemas(&mut view, &packet);
        view.insert("full_context".into(), archive);
        let recovery_bytes = delivered_guide.len() + 1 + stringify(&Value::Object(view.clone())).len();
        for key in ["goal","objective","assignment","permission_profile","user_updates","plan","decisions","project_knowledge"] {
            assert_eq!(view.get(key), packet.get(key));
        }
        for report in packet["reports"].as_array().unwrap() {
            if report["state"] != "done" || !old.packet["reports"].as_array().unwrap().contains(report) {
                assert!(view["reports"].as_array().unwrap().contains(report));
            }
        }
        let mut next = packet.clone();
        next.insert("previous_reply".into(), json!("recovery success"));
        next["user_updates"].as_array_mut().unwrap().push(json!("new update"));
        let recovered = Baseline { packet:packet.clone(), guide_sha256:digest(guide), snapshot_sha256:digest(&stringify(&sa_engine::context::sort_keys(&Value::Object(packet.clone())))), reply:"recovery success".into(), lazy_mcp_schemas:true, needs_recovery:false };
        let (delta, delta_guide) = delivery::delta(&next, guide, &recovered);
        assert_eq!(restore(&recovered, &delta), next);
        metrics.push(json!({
            "ticket":7,"job":100,"turn":turn,"snapshot_seq":seq,
            "full_prompt_bytes":running["full_prompt_bytes"],"old_sent_prompt_bytes":running["sent_prompt_bytes"],
            "recovery_prompt_bytes":recovery_bytes,"delta_next_bytes":delta_guide.len()+1+stringify(&Value::Object(delta)).len(),
            "reports_full":packet["reports"].as_array().unwrap().len(),"reports_inline":view["reports"].as_array().unwrap().len(),
            "reports_indexed":view["history_index"]["reports"]["archived_count"],
            "notes_full":packet["notes"].as_array().unwrap().len(),"notes_inline":view["notes"].as_array().unwrap().len(),
        }));
    }
    conn.execute_batch("ROLLBACK").unwrap();
    std::fs::write(output, serde_json::to_vec_pretty(&metrics).unwrap()).unwrap();
}

#[test]
fn lazy_schema_view_keeps_exact_references_and_unicode_safe_previews() {
    let source = object(json!({"project_extensions":schema_fixture()}));
    let mut view = source.clone();
    assert!(delivery::defer_mcp_schemas(&mut view, &source));
    let tool = &view["project_extensions"]["mcp"][0]["tools"][0];
    assert_eq!(tool["description"].as_str().unwrap().chars().count(), 120);
    assert!(tool.get("inputSchema").is_none());
    let raw = Value::Object(source.clone());
    assert_eq!(raw.pointer(tool["schema_ref"].as_str().unwrap()).unwrap(), &source["project_extensions"]["mcp"][0]["tools"][0]);
    assert_eq!(view["project_extensions"]["mcp"][1], source["project_extensions"]["mcp"][1]);
    assert!(view["mcp_schema_lookup"]["instructions"].as_str().unwrap().contains("禁止 cat/type/Get-Content"));
    let small = object(json!({"project_extensions":{"skills":[],"mcp":[source["project_extensions"]["mcp"][1]]}}));
    let mut small_view = small.clone();
    assert!(!delivery::defer_mcp_schemas(&mut small_view, &small));
    assert_eq!(small_view, small);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lazy_schema_initial_and_delta_views_preserve_authorized_calls_and_full_ledger() {
    let context = schema_fixture();
    let extensions = Arc::new(TestExtensions(Mutex::new(context.clone())));
    let opts = sa_engine::EngineOptions { extensions:Some(extensions.clone()), ..quiet() };
    let expected = context.clone();
    let fx = Fx::with(script(move |c| {
        let expected = expected.clone();
        async move {
            let original = read_archive(&c.packet);
            assert_eq!(original["project_extensions"], expected);
            assert!(c.packet["mcp_schema_lookup"].is_object());
            if c.turn() == 1 {
                assert!(c.req.session_id.is_none());
                let tool = &c.packet["project_extensions"]["mcp"][0]["tools"][0];
                assert!(tool.get("inputSchema").is_none());
                let exact = original.pointer(tool["schema_ref"].as_str().unwrap()).unwrap();
                assert!(exact["description"].as_str().unwrap().ends_with("完整操作约束末行"));
                assert_eq!(exact["inputSchema"], expected["mcp"][0]["tools"][0]["inputSchema"]);
                return Ok(wire("SA_TOOL", json!({
                    "id":"authorized","server":"catalog","tool":"tool_0",
                    "arguments":{"choice":exact["inputSchema"]["properties"]["choice"]["enum"][0]},
                })));
            }
            assert_eq!(c.packet["context_delivery"]["mode"], "delta");
            assert!(c.packet.get("project_extensions").is_none());
            // Inherited references resolve against the current full snapshot.
            assert!(original.pointer("/project_extensions/mcp/0/tools/0/inputSchema").is_some());
            Ok("success".into())
        }
    }), t1_catalog(), opts);
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "done").await;
    assert_eq!(extensions.0.lock().unwrap()["invoke_count"], 1);
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    let ledger = fx.engine.db().with(|q| delivery::full_input(q, root, i64::MAX)).unwrap().unwrap();
    let packet: Value = serde_json::from_str(ledger["payload"].as_str().unwrap()).unwrap();
    assert_eq!(packet["project_extensions"], context);
    let metrics = payloads(&fx, "SELECT payload FROM events WHERE kind='running' ORDER BY seq DESC LIMIT 1").pop().unwrap();
    assert_eq!(metrics["lazy_mcp_schemas"], true);
    assert!(metrics["sent_prompt_bytes"].as_u64().unwrap() * 2 < metrics["full_prompt_bytes"].as_u64().unwrap());
    eprintln!("lazy MCP initial bytes full={} sent={} saved={}", metrics["full_prompt_bytes"], metrics["sent_prompt_bytes"], metrics["saved_bytes"]);
    fx.engine.follow(ticket, "continue", false).unwrap();
    fx.finish(ticket, "done").await;
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lazy_schema_toggle_and_new_session_replay_the_correct_directory_view() {
    let efficiency = sa_store::efficiency::shared(Default::default());
    let extensions = Arc::new(TestExtensions(Mutex::new(schema_fixture())));
    let opts = sa_engine::EngineOptions { efficiency:efficiency.clone(), extensions:Some(extensions), ..quiet() };
    let fx = Fx::with(script(|_| async { Ok("success".into()) }), t1_catalog(), opts);
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "done").await;
    for enabled in [false, true] {
        efficiency.write().unwrap().lazy_mcp_schemas = enabled;
        fx.engine.follow(ticket, "toggle", false).unwrap();
        fx.finish(ticket, "done").await;
        let packet = fx.shared.calls(|c| Some(c.packet.clone())).pop().unwrap();
        assert!(packet.get("context_delivery").is_none());
        let tool = &packet["project_extensions"]["mcp"][0]["tools"][0];
        assert_eq!(tool.get("schema_ref").is_some(), enabled);
        assert_eq!(tool.get("inputSchema").is_some(), !enabled);
        assert_eq!(packet.get("mcp_schema_lookup").is_some(), enabled);
    }
    fx.engine.db().tx(|q| { q.run("UPDATE jobs SET session=NULL", &[])?; Ok(()) }).unwrap();
    fx.engine.follow(ticket, "new session", false).unwrap();
    fx.finish(ticket, "done").await;
    let packet = fx.shared.calls(|c| Some(c.packet.clone())).pop().unwrap();
    assert!(packet.get("context_delivery").is_none());
    assert!(packet["project_extensions"]["mcp"][0]["tools"][0]["schema_ref"].is_string());
    read_archive(&packet);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_receipt_with_the_same_guide_only_reuses_the_full_schema_mode() {
    for enabled in [false, true] {
        let settings = sa_store::efficiency::Settings { lazy_mcp_schemas:false, ..Default::default() };
        let efficiency = sa_store::efficiency::shared(settings);
        let extensions = Arc::new(TestExtensions(Mutex::new(schema_fixture())));
        let opts = sa_engine::EngineOptions { efficiency:efficiency.clone(), extensions:Some(extensions), ..quiet() };
        let fx = Fx::with(script(|_| async { Ok("success".into()) }), t1_catalog(), opts);
        let ticket = fx.begin(fx.params().profile("full")).await;
        fx.finish(ticket, "done").await;
        // Represent a real pre-feature receipt. Its successful input was sent
        // in full above, and only the newly introduced mode key is absent.
        fx.engine.db().tx(|q| {
            let receipt = q.one("SELECT seq,payload FROM events WHERE kind='context.delivered' ORDER BY seq DESC LIMIT 1", &[])?.unwrap();
            let mut value: Value = serde_json::from_str(receipt["payload"].as_str().unwrap())?;
            value.as_object_mut().unwrap().remove("lazy_mcp_schemas");
            let payload = stringify(&value);
            q.run("UPDATE events SET payload=?,sha256=? WHERE seq=?", &[&payload, &digest(&payload), &receipt["seq"].as_i64().unwrap()])?;
            Ok(())
        }).unwrap();
        efficiency.write().unwrap().lazy_mcp_schemas = enabled;
        fx.engine.follow(ticket, "continue", false).unwrap();
        fx.finish(ticket, "done").await;
        let packet = fx.shared.calls(|c| Some(c.packet.clone())).pop().unwrap();
        assert_eq!(packet["context_delivery"]["mode"] == "delta", !enabled);
        if enabled {
            assert!(packet["project_extensions"]["mcp"][0]["tools"][0]["schema_ref"].is_string());
            assert!(read_archive(&packet)["project_extensions"]["mcp"][0]["tools"][0]["inputSchema"].is_object());
        }
        fx.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restricted_schema_delivery_stays_complete_and_small_catalogs_stay_inline() {
    for (profile, large) in [("partial", true), ("full", false)] {
        let mut context = schema_fixture();
        if !large { context["mcp"][0]["tools"] = json!([{"name":"inspect","description":"small","inputSchema":{"type":"object"}}]); }
        let extensions = Arc::new(TestExtensions(Mutex::new(context.clone())));
        let opts = sa_engine::EngineOptions { extensions:Some(extensions), ..quiet() };
        let fx = Fx::with(script(|_| async { Ok("success".into()) }), t1_catalog(), opts);
        let ticket = fx.begin(fx.params().profile(profile)).await;
        fx.finish(ticket, "done").await;
        let packet = fx.shared.calls(|c| Some(c.packet.clone())).pop().unwrap();
        assert_eq!(packet["project_extensions"], context);
        assert!(packet.get("mcp_schema_lookup").is_none());
        assert!(packet.get("full_context").is_none());
        fx.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_and_compacted_requests_keep_lazy_schema_references_to_the_raw_snapshot() {
    for oversized in [false, true] {
        let attempts = Arc::new(AtomicUsize::new(0));
        let count = attempts.clone();
        let extensions = Arc::new(TestExtensions(Mutex::new(schema_fixture())));
        let opts = sa_engine::EngineOptions { extensions:Some(extensions), ..quiet() };
        let fx = Fx::with(script(move |c| {
            let count = count.clone();
            async move {
                let original = read_archive(&c.packet);
                assert!(original["project_extensions"]["mcp"][0]["tools"][0]["inputSchema"].is_object());
                assert!(c.packet["mcp_schema_lookup"].is_object());
                assert!(c.req.prompt.len() <= sa_engine::context::MAX_PROMPT_BYTES);
                assert!(c.packet.get("context_delivery").is_none());
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(AdapterError::new(ErrorClass::Protocol, "fixture failure"));
                }
                Ok("success".into())
            }
        }), t1_catalog(), opts);
        let mut params = fx.params().profile("full");
        if oversized { params.goal = "大".repeat(800_000); }
        let ticket = fx.begin(params).await;
        fx.finish(ticket, "failed").await;
        assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='context.delivered'"), 0);
        let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
        fx.engine.retry(root).unwrap();
        fx.finish(ticket, "done").await;
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        if oversized {
            let receipt = payloads(&fx, "SELECT payload FROM events WHERE kind='context.delivered'").pop().unwrap();
            assert_eq!(receipt["complete"], false);
        }
        fx.engine.close().await;
    }
}
