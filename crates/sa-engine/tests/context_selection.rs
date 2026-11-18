//! P1B context selection: requirements in the envelope, role packs, the
//! per-turn `context.selection` manifest and typed, invalidated knowledge.

mod common;

use common::*;
use serde_json::{Value, json};
use std::path::Path;
use tokio_util::sync::CancellationToken;

fn selections(fx: &Fx, job: i64) -> Vec<Value> {
    payloads(
        fx,
        &format!("SELECT payload FROM events WHERE job={job} AND kind='context.selection' ORDER BY seq"),
    )
}

fn ids(items: &Value) -> Vec<i64> {
    items.as_array().unwrap().iter().map(|i| i["id"].as_i64().unwrap()).collect()
}

/// The lossless packet of a call: the wire view, or the archive behind a delta.
fn full(c: &Call) -> Value {
    if c.packet["context_delivery"]["mode"] != "delta" {
        return c.packet.clone();
    }
    let bytes = std::fs::read(c.packet["full_context"]["object_path"].as_str().unwrap()).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn packet_of(fx: &Fx, job: i64, turn: i64) -> Value {
    fx.shared
        .calls(|c| (c.packet["job"] == job && c.packet["turn"] == turn).then(|| full(c)))
        .pop()
        .unwrap_or_else(|| panic!("no call for job {job} turn {turn}"))
}

/// Child jobs inserted before the scheduler starts, so requirements can name them.
/// 2 writer, 3 reviewer, 4 and 5 unrelated siblings; 2 needs 4.
fn children(fx: &Fx, ticket: i64) -> i64 {
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    fx.engine
        .db()
        .with(|q| {
            for (agent, writing, body) in [
                ("deepseek", 1, "implement"),
                ("deepseek", 0, "review"),
                ("deepseek", 0, "sibling a"),
                ("deepseek", 0, "sibling b"),
            ] {
                q.run(
                    "INSERT INTO jobs(ticket,parent,sender,agent,body,writing,depth,state) VALUES(?,?,'codex',?,?,?,1,'queued')",
                    &[&ticket, &root, &agent, &body, &writing],
                )?;
            }
            q.run("INSERT INTO job_deps(job,needs) VALUES(2,4)", &[])?;
            Ok(())
        })
        .unwrap();
    root
}

fn quiet_script() -> Script {
    script(|c| async move {
        if c.depth() == 0 {
            // Lead waits for the children to be done before finishing.
            return Ok("完成".into());
        }
        Ok("子任务完成".into())
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requirements_roles_roster_and_manifests() {
    let fx = Fx::new(quiet_script(), t1_catalog());
    let ticket = fx.create(fx.params().profile("full")).await;
    children(&fx, ticket);
    let e = &fx.engine;
    let mut revision = 0;
    let mut add = |text: &str, job: Option<i64>| {
        let summary = e.save_requirement(ticket, revision, None, text.into(), None, None, job).unwrap();
        revision += 1;
        summary.requirements.last().unwrap().id
    };
    let r1 = add("必须一：结果等于 42", None);
    let r2 = add("必须二：不得改动公共接口", None);
    let r3 = add("可选三：补充文档", None);
    let r4 = add("可选四：由实现者负责的清理", Some(2));
    let r5 = add("已撤回的需求", None);
    for (id, required, withdrawn) in [(r3, false, false), (r4, false, false), (r5, true, true)] {
        e.set_requirement_disposition(ticket, revision, id, required, withdrawn, "整理".into()).unwrap();
        revision += 1;
    }
    e.start().unwrap();
    fx.until(|| fx.int("SELECT count(*) FROM jobs WHERE state='done'") == 5).await;

    // Lead: all active items (required first), no assignment marks.
    let lead = packet_of(&fx, 1, 1);
    let req = &lead["requirements"];
    assert_eq!(req["contract_revision"], revision);
    assert_eq!(req["gate_enabled"], false);
    assert_eq!(ids(&req["items"]), vec![r1, r2, r3, r4]);
    assert!(req["items"][0].get("assigned").is_none());
    assert_eq!(req["items"][0]["revision"], 1);
    assert_eq!(req["items"][0]["required"], true);
    assert_eq!(req["items"][2]["required"], false);
    assert!(lead.get("requirements_omitted").is_none());
    assert_eq!(lead["roster"].as_array().unwrap().len(), 5);
    assert!(lead.get("roster_omitted").is_none());

    // Implementer: every required item plus its own; the others are counted.
    let writer = packet_of(&fx, 2, 1);
    assert_eq!(ids(&writer["requirements"]["items"]), vec![r1, r2, r4]);
    let marks: Vec<bool> = writer["requirements"]["items"].as_array().unwrap().iter().map(|i| i["assigned"].as_bool().unwrap()).collect();
    assert_eq!(marks, vec![false, false, true]);
    assert_eq!(writer["requirements_omitted"]["count"], 1);
    assert!(writer["requirements_omitted"]["fetch"].as_str().unwrap().contains("主控"));
    // Roster: parent, itself and its prerequisite; siblings 3 and 5 are omitted.
    let roster: Vec<i64> = writer["roster"].as_array().unwrap().iter().map(|r| r["job"].as_i64().unwrap()).collect();
    assert_eq!(roster, vec![1, 2, 4]);
    assert_eq!(writer["roster_omitted"]["count"], 2);
    assert_eq!(writer["prerequisites"][0]["job"], 4);

    // Reviewer: required items only, none assigned.
    let reviewer = packet_of(&fx, 3, 1);
    assert_eq!(ids(&reviewer["requirements"]["items"]), vec![r1, r2]);
    assert!(reviewer["requirements"]["items"].as_array().unwrap().iter().all(|i| i["assigned"] == false));
    assert_eq!(reviewer["requirements_omitted"]["count"], 2);
    assert_eq!(reviewer["roster"].as_array().unwrap().len(), 2);

    // One manifest per turn, with role, sources and omitted counts.
    let m = &selections(&fx, 1)[0];
    assert_eq!((m["schema_version"].as_i64(), m["policy"].as_str(), m["role"].as_str()), (Some(1), Some("roles-1"), Some("lead")));
    assert_eq!(m["mode"], "full");
    assert!(m["omitted"].as_array().unwrap().is_empty());
    let sources: Vec<(i64, i64)> = m["sources"].as_array().unwrap().iter()
        .filter(|s| s["key"] == "requirements").map(|s| (s["id"].as_i64().unwrap(), s["revision"].as_i64().unwrap())).collect();
    assert_eq!(sources, vec![(r1, 1), (r2, 1), (r3, 1), (r4, 1)]);
    let m = &selections(&fx, 2)[0];
    assert_eq!(m["role"], "implementer");
    let omitted: Vec<(String, i64)> = m["omitted"].as_array().unwrap().iter().map(|o| (o["key"].as_str().unwrap().into(), o["count"].as_i64().unwrap())).collect();
    assert!(omitted.contains(&("roster".into(), 2)) && omitted.contains(&("requirements".into(), 1)), "{omitted:?}");
    assert!(m["omitted"][0]["fetch"].as_str().is_some());
    assert!(m["sources"].as_array().unwrap().iter().any(|s| s["key"] == "requirements" && s["id"] == r4 && s["reason"] == "assigned"));
    assert_eq!(selections(&fx, 3)[0]["role"], "reviewer");
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tickets_without_a_contract_get_no_requirements_key() {
    let fx = Fx::new(quiet_script(), t1_catalog());
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "done").await;
    let packet = packet_of(&fx, 1, 1);
    assert!(packet.get("requirements").is_none() && packet.get("requirements_omitted").is_none());
    // The manifest is still recorded and lists no requirements.
    let m = &selections(&fx, 1)[0];
    assert_eq!(m["role"], "lead");
    assert!(m["sources"].as_array().unwrap().iter().all(|s| s["key"] != "requirements"));
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requirements_are_bounded_and_truncation_is_visible_and_char_safe() {
    let fx = Fx::new(quiet_script(), t1_catalog());
    let ticket = fx.create(fx.params()).await;
    for i in 0..40 {
        fx.engine
            .save_requirement(ticket, i, None, format!("需求{i}：{}", "字".repeat(1200)), None, None, None)
            .unwrap();
    }
    fx.engine.start().unwrap();
    fx.finish(ticket, "done").await;
    let packet = packet_of(&fx, 1, 1);
    let items = packet["requirements"]["items"].as_array().unwrap();
    assert!(items.len() <= 32 && !items.is_empty());
    assert!(serde_json::to_vec(&packet["requirements"]).unwrap().len() <= 16 * 1024);
    assert!(items.iter().all(|i| i["text"].as_str().unwrap().chars().count() <= 600));
    assert_eq!(packet["requirements_omitted"]["count"].as_u64().unwrap() as usize, 40 - items.len());
    assert_eq!(packet["requirements_omitted"]["truncated"], packet["requirements_omitted"]["count"]);
    let m = &selections(&fx, 1)[0];
    assert!(m["omitted"].as_array().unwrap().iter().any(|o| o["key"] == "requirements" && o["count"] == packet["requirements_omitted"]["count"]));
    assert!(serde_json::to_vec(m).unwrap().len() <= 16 * 1024);
    fx.engine.close().await;
}

#[test]
fn required_items_survive_compaction() {
    let items: Vec<Value> = (1..=3)
        .map(|id| json!({"id": id, "revision": 1, "text": format!("必需 {id}"), "required": true}))
        .collect();
    let mut packet = serde_json::Map::new();
    packet.insert("job".into(), json!(1));
    packet.insert("requirements".into(), json!({"contract_revision": 3, "gate_enabled": true, "items": items}));
    packet.insert("requirements_omitted".into(), json!({"count": 2, "fetch": "问主控"}));
    packet.insert("notes".into(), json!(["x".repeat(400_000)]));
    packet.insert("user_updates".into(), json!(["y".repeat(400_000)]));
    let dir = tempfile::tempdir().unwrap();
    let out = sa_engine::context::compact(&packet, "guide", dir.path(), 64 * 1024).unwrap();
    assert_eq!(out["requirements"], packet["requirements"]);
    assert_eq!(out["requirements_omitted"], packet["requirements_omitted"]);
    assert!(out.contains_key("full_context"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incremental_delivery_still_saves_bytes_and_resends_changed_requirements() {
    let fx = Fx::new(quiet_script(), t1_catalog());
    for id in 1..=3 {
        fx.engine
            .add_knowledge(fx.project.to_str().unwrap(), "memory", &format!("stable {id} {}", "x".repeat(700)), None)
            .unwrap();
    }
    let ticket = fx.create(fx.params().profile("full")).await;
    let summary = fx.engine.save_requirement(ticket, 0, None, "结果等于 42".into(), None, None, None).unwrap();
    let requirement = summary.requirements[0].id;
    fx.engine.start().unwrap();
    fx.finish(ticket, "done").await;

    fx.engine.follow(ticket, "继续", false).unwrap();
    fx.finish(ticket, "done").await;
    let second = fx.shared.calls(|c| (c.packet["turn"] == 2).then(|| c.packet.clone())).pop().unwrap();
    assert_eq!(second["context_delivery"]["mode"], "delta");
    // CURRENT key: resent although unchanged.
    assert_eq!(second["requirements"]["items"][0]["text"], "结果等于 42");
    let metrics = payloads(&fx, "SELECT payload FROM events WHERE kind='running' ORDER BY seq DESC LIMIT 1").pop().unwrap();
    assert!(metrics["sent_prompt_bytes"].as_u64().unwrap() < metrics["full_prompt_bytes"].as_u64().unwrap());

    fx.engine.save_requirement(ticket, 1, Some(requirement), "结果等于 43".into(), None, None, None).unwrap();
    fx.engine.follow(ticket, "再继续", false).unwrap();
    fx.finish(ticket, "done").await;
    let third = fx.shared.calls(|c| (c.packet["turn"] == 3).then(|| c.packet.clone())).pop().unwrap();
    assert_eq!(third["context_delivery"]["mode"], "delta");
    assert_eq!(third["requirements"]["items"][0]["text"], "结果等于 43");
    assert_eq!(third["requirements"]["items"][0]["revision"], 2);
    assert_eq!(third["requirements"]["contract_revision"], 2);
    // The full snapshot behind the delta holds the same view.
    assert_eq!(third["requirements"], fx.shared.calls(|c| (c.packet["turn"] == 3).then(|| full(c))).pop().unwrap()["requirements"]);
    let modes: Vec<Value> = selections(&fx, 1).iter().map(|m| m["mode"].clone()).collect();
    assert_eq!(modes, vec![json!("full"), json!("delta"), json!("delta")]);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn advisor_sees_required_requirements_and_the_selection_is_journaled() {
    let fx = Fx::new(
        script(|c| async move {
            if c.req.prompt.starts_with("You are the advisor") {
                return Ok("建议".into());
            }
            Ok("done".into())
        }),
        t1_catalog(),
    );
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "done").await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    let worker = fx
        .engine
        .db()
        .with(|q| {
            q.run(
                "INSERT INTO jobs(ticket,parent,agent,body,writing,depth,turn,state) VALUES(?,?,'deepseek','w',0,1,1,'running')",
                &[&ticket, &root],
            )?;
            Ok(q.conn().last_insert_rowid())
        })
        .unwrap();
    let brief = "Which approach?";
    let advice_prompt = || fx.shared.calls(|c| c.req.prompt.starts_with("You are the advisor").then(|| c.req.prompt.clone())).pop().unwrap();
    // No contract: the prompt is exactly the historical one.
    fx.engine.consult(ticket, worker, 1, brief, CancellationToken::new()).await.unwrap();
    assert_eq!(advice_prompt(), format!("{}\n\n{brief}", sa_engine::advice::GUIDE));
    let first = &selections(&fx, worker)[0];
    assert_eq!(first["role"], "advisor");
    assert!(first["sources"].as_array().unwrap().iter().all(|s| s["key"] != "requirements"));

    let summary = fx.engine.save_requirement(ticket, 0, None, "必须：结果等于 42".into(), None, None, None).unwrap();
    let optional = fx.engine.save_requirement(ticket, 1, None, "可选：写文档".into(), None, None, None).unwrap().requirements[1].id;
    fx.engine.set_requirement_disposition(ticket, 2, optional, false, false, "非必需".into()).unwrap();
    fx.engine.consult(ticket, worker, 1, brief, CancellationToken::new()).await.unwrap();
    let prompt = advice_prompt();
    assert!(prompt.starts_with(sa_engine::advice::GUIDE) && prompt.ends_with(brief));
    assert!(prompt.contains("当前需求") && prompt.contains("必须：结果等于 42"));
    assert!(!prompt.contains("写文档"), "optional items stay out of the advisor section");
    assert!(prompt.len() <= sa_engine::advice::GUIDE.len() + 4096 + brief.len() + 8);
    let m = selections(&fx, worker).pop().unwrap();
    assert_eq!(m["role"], "advisor");
    assert_eq!(m["sources"][0], json!({"key": "requirements", "id": summary.requirements[0].id, "revision": 1, "reason": "required"}));
    assert_eq!(m["omitted"][0]["count"], 1);
    // Journaled on the caller's job, next to its advice.started event.
    assert_eq!(fx.int(&format!("SELECT count(*) FROM events WHERE job={worker} AND kind='advice.started'")), 2);
    fx.engine.close().await;
}

fn memo_line(v: Value) -> String {
    wire("SA_MEMO", v)
}

fn rejected(fx: &Fx) -> Vec<String> {
    payloads(fx, "SELECT payload FROM events WHERE kind='knowledge.rejected' ORDER BY seq")
        .iter()
        .map(|p| p["reason"].as_str().unwrap().to_string())
        .collect()
}

fn write(project: &Path, name: &str, text: &str) {
    std::fs::write(project.join(name), text).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memo_with_paths_goes_stale_when_the_file_changes_and_recovers_when_restored() {
    let fx = Fx::new(
        script(|c| async move {
            if c.turn() == 1 {
                return Ok(memo_line(json!({"kind": "memory", "text": "服务端口配置在 conf.toml", "paths": ["conf.toml", "absent.toml"]})));
            }
            Ok("好".into())
        }),
        t1_catalog(),
    );
    write(&fx.project, "conf.toml", "port = 1\n");
    let project = fx.project.to_str().unwrap().to_string();
    fx.engine.add_knowledge(&project, "lesson", "无依赖文件的经验", None).unwrap();
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "done").await;

    // Recorded basis: digests, files basis, verified now, type fact.
    let rows = fx.all("SELECT * FROM knowledge_basis");
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0]["type"].as_str(), rows[0]["basis"].as_str(), rows[0]["state"].as_str()), (Some("fact"), Some("files"), Some("current")));
    assert!(rows[0]["verified"].as_f64().unwrap() > 0.0);
    let paths: Value = serde_json::from_str(rows[0]["paths"].as_str().unwrap()).unwrap();
    assert_eq!(paths[0]["path"], "conf.toml");
    assert_eq!(paths[0]["sha256"], sha("port = 1\n"));
    assert_eq!(paths[1]["sha256"], "missing");

    // Unchanged files: no status; the entry without paths never has one.
    fx.engine.follow(ticket, "继续", false).unwrap();
    fx.finish(ticket, "done").await;
    let turn2 = packet_of(&fx, 1, 2);
    assert!(turn2["project_knowledge"].as_array().unwrap().iter().all(|e| e.get("status").is_none()));

    write(&fx.project, "conf.toml", "port = 2\n");
    fx.engine.follow(ticket, "文件改了", false).unwrap();
    fx.finish(ticket, "done").await;
    let turn3 = packet_of(&fx, 1, 3);
    let entries = turn3["project_knowledge"].as_array().unwrap();
    let last = entries.last().unwrap();
    assert_eq!((last["status"].as_str(), last["body"].as_str()), (Some("stale"), Some("服务端口配置在 conf.toml")));
    assert_eq!(last["stale_paths"], json!(["conf.toml"]));
    assert!(entries[..entries.len() - 1].iter().all(|e| e.get("status").is_none()), "current entries come first: {entries:?}");
    assert_eq!(fx.all("SELECT state FROM knowledge_basis")[0]["state"], "stale");
    let manifest = selections(&fx, 1).pop().unwrap();
    assert!(manifest["sources"].as_array().unwrap().iter().any(|s| s["key"] == "project_knowledge" && s["state"] == "stale"));
    let operator = fx.engine.knowledge(&project).unwrap();
    let stale = operator.iter().find(|e| e.body.starts_with("服务端口")).unwrap();
    assert_eq!((stale.state.as_deref(), stale.paths.clone()), (Some("stale"), vec!["conf.toml".to_string(), "absent.toml".to_string()]));

    // Restoring the recorded bytes makes the entry current again.
    write(&fx.project, "conf.toml", "port = 1\n");
    fx.engine.follow(ticket, "文件还原", false).unwrap();
    fx.finish(ticket, "done").await;
    let turn4 = packet_of(&fx, 1, 4);
    assert!(turn4["project_knowledge"].as_array().unwrap().iter().all(|e| e.get("status").is_none()));
    assert_eq!(fx.all("SELECT state FROM knowledge_basis")[0]["state"], "current");
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn supersedes_applies_only_within_the_same_project_and_two_key_memos_are_unchanged() {
    let other = tempfile::tempdir().unwrap();
    let other_project = other.path().join("other");
    std::fs::create_dir_all(&other_project).unwrap();
    let fx = Fx::new(
        script(|c| async move {
            if c.turn() != 1 {
                return Ok("好".into());
            }
            Ok([
                memo_line(json!({"kind": "lesson", "text": "新结论", "supersedes": 1})),
                memo_line(json!({"kind": "lesson", "text": "跨项目尝试", "supersedes": 2})),
                memo_line(json!({"kind": "memory", "text": "只有两个键"})),
                memo_line(json!({"kind": "memory", "text": "路径越界", "paths": ["../secret"]})),
                memo_line(json!({"kind": "memory", "text": "未知键", "extra": 1})),
                memo_line(json!({"kind": "memory", "text": "九个路径", "paths": (0..9).map(|i| format!("f{i}")).collect::<Vec<_>>()})),
            ].join("\n"))
        }),
        t1_catalog(),
    );
    let project = fx.project.to_str().unwrap().to_string();
    let mine = fx.engine.add_knowledge(&project, "lesson", "旧结论", None).unwrap();
    let foreign = fx.engine.add_knowledge(other_project.to_str().unwrap(), "lesson", "别的项目的结论", None).unwrap();
    assert_eq!((mine, foreign), (1, 2));
    let ticket = fx.begin(fx.params().profile("full")).await;
    fx.finish(ticket, "done").await;

    let old = fx.all("SELECT archived FROM project_knowledge WHERE id=1");
    assert_eq!(old[0]["archived"], 1);
    let basis = fx.all("SELECT state,superseded_by FROM knowledge_basis WHERE entry=1");
    assert_eq!((basis[0]["state"].as_str(), basis[0]["superseded_by"].as_i64()), (Some("superseded"), Some(3)));
    // The other project's entry is untouched; the memo itself still lands.
    assert_eq!(fx.all("SELECT archived FROM project_knowledge WHERE id=2")[0]["archived"], 0);
    assert_eq!(fx.int("SELECT count(*) FROM knowledge_basis WHERE entry=2"), 0);
    assert_eq!(fx.int("SELECT count(*) FROM project_knowledge WHERE body='跨项目尝试'"), 1);
    // Two-key memos keep working without any basis row; invalid forms are rejected whole.
    assert_eq!(fx.int("SELECT count(*) FROM project_knowledge WHERE body='只有两个键'"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM knowledge_basis"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM project_knowledge WHERE body IN ('路径越界','未知键','九个路径')"), 0);
    assert_eq!(rejected(&fx), vec!["supersedes_unavailable", "invalid_fields", "invalid_fields", "invalid_fields"]);
    let listed = fx.engine.knowledge(&project).unwrap();
    let old = listed.iter().find(|e| e.id == 1).unwrap();
    assert_eq!((old.archived, old.state.as_deref(), old.superseded_by), (true, Some("superseded"), Some(3)));
    // Restoring a superseded entry makes it current again.
    fx.engine.archive_knowledge(1, false).unwrap();
    assert_eq!(fx.all("SELECT state FROM knowledge_basis WHERE entry=1")[0]["state"], "current");
    fx.engine.close().await;
    drop(other);
}
