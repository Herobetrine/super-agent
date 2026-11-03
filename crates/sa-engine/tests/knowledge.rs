//! Shared per-project knowledge base: `SA_MEMO`, automatic root progress,
//! the `project_knowledge` envelope field and the operator API (spec §4.4).

mod common;

use common::*;
use sa_engine::CreateParams;
use serde_json::{Value, json};

fn memo(kind: &str, text: &str) -> String {
    wire("SA_MEMO", json!({"kind": kind, "text": text}))
}

fn bodies(packet: &Value) -> Vec<String> {
    packet["project_knowledge"]
        .as_array()
        .map(|a| a.iter().map(|e| e["body"].as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

fn ticket_cwd(fx: &Fx, ticket: i64) -> String {
    fx.all(&format!("SELECT cwd FROM tickets WHERE id={ticket}"))[0]["cwd"]
        .as_str()
        .unwrap()
        .to_string()
}

fn rejected(fx: &Fx) -> Vec<String> {
    payloads(fx, "SELECT payload FROM events WHERE kind='knowledge.rejected' ORDER BY seq")
        .iter()
        .map(|p| p["reason"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memos_reach_other_jobs_and_tickets_of_the_same_project_only() {
    let fx = Fx::new(
        script(|c| async move {
            let goal = c.packet["goal"].as_str().unwrap_or_default().to_string();
            if goal != "fixture" {
                return Ok("已查看".into());
            }
            if c.depth() > 0 {
                return Ok(format!("子任务完成\n{}", memo("lesson", "直接用 cargo 会链接失败，改用 scripts/cargo.cjs。")));
            }
            if c.turn() == 1 {
                // Streamed previews never record knowledge.
                c.emit("delta", json!(format!("{}\n", memo("memory", "仅流式出现"))));
                c.emit("message.boundary", Value::Null);
                return Ok(format!(
                    "{}\n{}",
                    memo("memory", "构建入口是 scripts/cargo.cjs。"),
                    send(json!({"id": "check", "to": "deepseek", "body": "核对构建"}))
                ));
            }
            Ok("主控完成".into())
        }),
        t1_catalog(),
    );
    let first = fx.begin(fx.params()).await;
    fx.finish(first, "done").await;

    let rows = fx.all("SELECT kind,body,ticket,job,agent,archived FROM project_knowledge ORDER BY id");
    let summary: Vec<(String, String)> = rows
        .iter()
        .map(|r| (r["kind"].as_str().unwrap().into(), r["agent"].as_str().unwrap().into()))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("memory".into(), "codex".into()),
            ("lesson".into(), "deepseek".into()),
            ("progress".into(), "codex".into())
        ]
    );
    assert_eq!(rows[1]["job"], json!(2));
    assert_eq!(fx.int("SELECT count(*) FROM project_knowledge WHERE body='仅流式出现'"), 0);
    let adds = payloads(&fx, "SELECT payload FROM events WHERE kind='knowledge.add' ORDER BY seq");
    assert_eq!(adds.iter().map(|a| a["kind"].clone()).collect::<Vec<_>>(), vec![json!("memory"), json!("lesson"), json!("progress")]);

    // The child (a different job) saw the lead's memory; the lead's second
    // turn saw the child's lesson. Entries are newest first.
    let child = fx.shared.calls(|c| (c.packet["depth"] == 1).then(|| c.packet.clone()));
    assert_eq!(bodies(&child[0]), vec!["构建入口是 scripts/cargo.cjs。"]);
    let lead2 = fx.shared.calls(|c| (c.packet["depth"] == 0 && c.packet["turn"] == 2).then(|| c.packet.clone()));
    assert_eq!(
        bodies(&lead2[0]),
        vec!["直接用 cargo 会链接失败，改用 scripts/cargo.cjs。", "构建入口是 scripts/cargo.cjs。"]
    );
    let entry = &lead2[0]["project_knowledge"][0];
    assert_eq!(
        entry.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["id", "kind", "body", "agent", "ticket"]
    );
    assert_eq!(entry["agent"], json!("deepseek"));
    assert_eq!(entry["ticket"], json!(first));
    // Appended after the Rust-port fields; existing key order is unchanged.
    let keys: Vec<&String> = lead2[0].as_object().unwrap().keys().collect();
    let at = |k: &str| keys.iter().position(|x| *x == k).unwrap();
    assert_eq!(at("project_knowledge"), at("recovery") + 1);
    assert!(at("autonomy") < at("harness_health"));
    // Every turn's guide explains SA_MEMO.
    assert!(fx.shared.calls(|c| Some(c.req.prompt.contains("SA_MEMO {"))).iter().all(|x| *x));

    // A new ticket on the same project sees all three, including progress.
    let second = fx.create(CreateParams::new("second", &fx.project).lead("claude")).await;
    fx.finish(second, "done").await;
    let root2 = fx.shared.calls(|c| (c.packet["ticket"] == second).then(|| c.packet.clone()));
    let seen = bodies(&root2[0]);
    assert_eq!(seen.len(), 3);
    assert!(seen[0].starts_with(&format!("工单 #{first}（完成）目标：fixture")), "{seen:?}");

    // A different project does not.
    let other = fx.dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let third = fx.create(CreateParams::new("other", &other).lead("claude")).await;
    fx.finish(third, "done").await;
    let root3 = fx.shared.calls(|c| (c.packet["ticket"] == third).then(|| c.packet.clone()));
    assert!(root3[0].get("project_knowledge").is_none());
    // Its own progress is filed under its own project.
    let cwd3 = ticket_cwd(&fx, third);
    assert_eq!(fx.engine.knowledge(&cwd3).unwrap().len(), 1);
    assert_eq!(fx.engine.knowledge(&ticket_cwd(&fx, first)).unwrap().len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicates_limits_and_malformed_memos_never_fail_the_job() {
    let text = [
        memo("memory", "A"),
        memo("memory", "  A \t"),
        "SA_MEMO {broken".into(),
        memo("secret", "x"),
        wire("SA_MEMO", json!({"kind": "memory", "text": "x", "id": "k"})),
        memo("memory", &"字".repeat(2001)),
        memo("lesson", "B"),
        memo("progress", "C"),
        memo("memory", "D"),
        memo("memory", "E"),
        "SA_MEMOX ignored".into(),
        "已记录".into(),
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
    let manual = fx.all("SELECT body FROM project_knowledge WHERE kind!='progress' OR body='C' ORDER BY id");
    let manual: Vec<&str> = manual.iter().map(|r| r["body"].as_str().unwrap()).collect();
    assert_eq!(manual, vec!["A", "B", "C"]);
    assert_eq!(
        rejected(&fx),
        vec!["invalid_json", "invalid_fields", "invalid_fields", "invalid_fields", "too_many", "too_many"]
    );
    // Rejections never quote the content.
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='knowledge.rejected' AND payload LIKE '%secret%'"), 0);
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='knowledge.add'"), 4);

    // The operator archives A; replaying the same reply (a follow here) does
    // not bring it back from the same job, and B/C stay single.
    let cwd = ticket_cwd(&fx, id);
    let a = fx.int("SELECT id FROM project_knowledge WHERE body='A'");
    fx.engine.archive_knowledge(a, true).unwrap();
    fx.engine.follow(id, "again", false).unwrap();
    fx.until(|| fx.int("SELECT count(*) FROM events WHERE kind='reply'") == 2).await;
    assert_eq!(fx.int("SELECT count(*) FROM project_knowledge WHERE body IN ('A','B','C')"), 3);
    assert_eq!(fx.int("SELECT count(*) FROM project_knowledge WHERE body='A' AND archived=0"), 0);
    // Limits are per turn: the second turn was capped again.
    assert_eq!(rejected(&fx).iter().filter(|r| *r == "too_many").count(), 4);
    // A follow-up completion files a new progress entry with the follow text.
    let progress: Vec<String> = fx
        .engine
        .knowledge(&cwd)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "progress" && e.agent.as_deref() == Some("codex") && e.body.starts_with("工单"))
        .map(|e| e.body)
        .collect();
    assert_eq!(progress.len(), 2);
    assert!(progress[0].contains("\n最新追加：again\n"), "{progress:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn root_completion_files_progress_but_children_do_not() {
    let fx = Fx::new(
        script(|c| async move {
            if c.depth() > 0 {
                // A malformed final control fails the child.
                return Ok("SA_SEND {broken".into());
            }
            if c.turn() == 1 {
                return Ok(format!("派出检查\n{}", send(json!({"id": "c", "to": "claude", "body": "检查"}))));
            }
            Ok(format!(
                "  核对完毕：\n\n两项通过\n{}",
                wire("SA_PROGRESS", json!({"id": "p", "summary": "收尾"}))
            ))
        }),
        t1_catalog(),
    );
    let id = fx.begin(fx.params()).await;
    fx.finish(id, "partial").await;
    let rows = fx.all("SELECT kind,body,ticket,job,agent FROM project_knowledge");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["kind"], json!("progress"));
    assert_eq!(rows[0]["agent"], json!("codex"));
    assert_eq!(rows[0]["ticket"], json!(id));
    assert_eq!(rows[0]["job"], json!(1));
    assert_eq!(
        rows[0]["body"],
        json!(format!("工单 #{id}（部分完成）目标：fixture\n结果：核对完毕： 两项通过"))
    );
    let add = payloads(&fx, "SELECT payload FROM events WHERE kind='knowledge.add'");
    assert_eq!(add, vec![json!({"id": 1, "kind": "progress"})]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn operator_entries_archive_and_budgeted_envelope() {
    let fx = Fx::new(script(|_| async move { Ok("看过了".into()) }), t1_catalog());
    let id = fx.begin(fx.params()).await;
    fx.finish(id, "done").await;
    let cwd = ticket_cwd(&fx, id);
    let project = fx.project.to_string_lossy().into_owned();

    // Operator API: validation, project match, dedupe.
    let e = |r: anyhow::Result<i64>| r.unwrap_err().to_string();
    assert_eq!(e(fx.engine.add_knowledge(&project, "secret", "x", None)), "知识类型无效");
    assert!(e(fx.engine.add_knowledge(&project, "memory", " ", None)).starts_with("知识内容为空"));
    assert!(e(fx.engine.add_knowledge(&project, "memory", &"x".repeat(2001), None)).starts_with("知识内容为空"));
    assert_eq!(e(fx.engine.add_knowledge(&project, "memory", "x", Some(99))), "工单不存在");
    let other = fx.dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    assert_eq!(
        e(fx.engine.add_knowledge(other.to_str().unwrap(), "memory", "x", Some(id))),
        "工单不属于该项目"
    );
    let pinned = fx.engine.add_knowledge(&project, "memory", " 发布前先跑 clippy ", Some(id)).unwrap();
    assert_eq!(e(fx.engine.add_knowledge(&cwd, "lesson", "发布前先跑 clippy", None)), "项目知识库已有相同内容");
    let log = payloads(&fx, "SELECT payload FROM events WHERE kind='knowledge.add' AND job=1 ORDER BY seq");
    assert_eq!(log.last().unwrap(), &json!({"id": pinned, "kind": "memory"}));
    let listed = fx.engine.knowledge(&project).unwrap();
    assert_eq!(listed[0].id, pinned);
    assert_eq!(listed[0].body, "发布前先跑 clippy");
    assert_eq!(listed[0].agent.as_deref(), Some("operator"));
    assert_eq!(listed[0].job, None);
    assert_eq!(listed[0].cwd, cwd);
    // The observer read path sees the same rows.
    assert_eq!(sa_store::knowledge::read(&fx.data, &cwd).unwrap(), listed);

    // Budget: ten 1500-character memories, one lesson, then one progress.
    for i in 0..10 {
        fx.engine
            .add_knowledge(&cwd, "memory", &format!("{i}{}", "记".repeat(1499)), None)
            .unwrap();
    }
    let lesson = fx.engine.add_knowledge(&cwd, "lesson", "旧经验", None).unwrap();
    fx.engine.archive_knowledge(pinned, true).unwrap();
    fx.engine.archive_knowledge(pinned, true).unwrap();
    assert_eq!(
        payloads(&fx, "SELECT payload FROM events WHERE kind='knowledge.archived'"),
        vec![json!({"id": pinned, "archived": true})]
    );
    fx.engine.follow(id, "next", false).unwrap();
    fx.until(|| fx.shared.count(|_| true) == 2).await;
    let packet = fx.shared.calls(|c| Some(c.packet.clone())).pop().unwrap();
    let entries = packet["project_knowledge"].as_array().unwrap();
    let ids: Vec<i64> = entries.iter().map(|e| e["id"].as_i64().unwrap()).collect();
    let mut sorted = ids.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(ids, sorted, "newest first");
    let total: usize = entries
        .iter()
        .map(|e| e["body"].as_str().unwrap().encode_utf16().count())
        .sum();
    assert!(total <= 6000, "{total}");
    assert!(ids.contains(&lesson) && ids.contains(&1), "every kind keeps a share: {ids:?}");
    assert!(!ids.contains(&pinned), "archived entries leave the envelope");
    assert_eq!(entries.iter().filter(|e| e["kind"] == "memory").count(), 3);
    assert!(fx.engine.knowledge(&cwd).unwrap().iter().any(|e| e.id == pinned && e.archived));

    // Restore, and refuse a restore that would duplicate an active entry.
    fx.engine.archive_knowledge(pinned, false).unwrap();
    fx.engine.archive_knowledge(pinned, true).unwrap();
    fx.engine.add_knowledge(&cwd, "memory", "发布前先跑 clippy", None).unwrap();
    assert_eq!(
        fx.engine.archive_knowledge(pinned, false).unwrap_err().to_string(),
        "项目知识库已有相同内容的有效条目"
    );
    assert_eq!(fx.engine.archive_knowledge(9999, true).unwrap_err().to_string(), "知识条目不存在");
}
