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
        .map(|a| {
            a.iter()
                .map(|e| e["body"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn ticket_cwd(fx: &Fx, ticket: i64) -> String {
    fx.all(&format!("SELECT cwd FROM tickets WHERE id={ticket}"))[0]["cwd"]
        .as_str()
        .unwrap()
        .to_string()
}

fn rejected(fx: &Fx) -> Vec<String> {
    payloads(
        fx,
        "SELECT payload FROM events WHERE kind='knowledge.rejected' ORDER BY seq",
    )
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
                return Ok(format!(
                    "子任务完成\n{}",
                    memo(
                        "lesson",
                        "直接用 cargo 会链接失败，改用 scripts/cargo.cjs。"
                    )
                ));
            }
            if c.turn() == 1 {
                // Streamed previews never record knowledge.
                c.emit(
                    "delta",
                    json!(format!("{}\n", memo("memory", "仅流式出现"))),
                );
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

    let rows =
        fx.all("SELECT kind,body,ticket,job,agent,archived FROM project_knowledge ORDER BY id");
    let summary: Vec<(String, String)> = rows
        .iter()
        .map(|r| {
            (
                r["kind"].as_str().unwrap().into(),
                r["agent"].as_str().unwrap().into(),
            )
        })
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
    assert_eq!(
        fx.int("SELECT count(*) FROM project_knowledge WHERE body='仅流式出现'"),
        0
    );
    let adds = payloads(
        &fx,
        "SELECT payload FROM events WHERE kind='knowledge.add' ORDER BY seq",
    );
    assert_eq!(
        adds.iter().map(|a| a["kind"].clone()).collect::<Vec<_>>(),
        vec![json!("memory"), json!("lesson"), json!("progress")]
    );

    // The child (a different job) saw the lead's memory; the lead's second
    // turn saw the child's lesson. Entries are newest first.
    let child = fx
        .shared
        .calls(|c| (c.packet["depth"] == 1).then(|| c.packet.clone()));
    assert_eq!(bodies(&child[0]), vec!["构建入口是 scripts/cargo.cjs。"]);
    let lead2 = fx
        .shared
        .calls(|c| (c.packet["depth"] == 0 && c.packet["turn"] == 2).then(|| c.packet.clone()));
    assert_eq!(
        bodies(&lead2[0]),
        vec![
            "直接用 cargo 会链接失败，改用 scripts/cargo.cjs。",
            "构建入口是 scripts/cargo.cjs。"
        ]
    );
    let entry = &lead2[0]["project_knowledge"][0];
    assert_eq!(
        entry.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec![
            "id",
            "kind",
            "body",
            "agent",
            "ticket",
            "job",
            "created",
            "relevance"
        ]
    );
    assert_eq!(entry["agent"], json!("deepseek"));
    assert_eq!(entry["ticket"], json!(first));
    // Appended after the Rust-port fields; existing key order is unchanged.
    let keys: Vec<&String> = lead2[0].as_object().unwrap().keys().collect();
    let at = |k: &str| keys.iter().position(|x| *x == k).unwrap();
    assert_eq!(at("project_knowledge"), at("recovery") + 1);
    assert!(at("autonomy") < at("harness_health"));
    // Every turn's guide explains SA_MEMO.
    assert!(
        fx.shared
            .calls(|c| Some(c.req.prompt.contains("SA_MEMO {")))
            .iter()
            .all(|x| *x)
    );

    // A new ticket on the same project sees all three, including progress.
    let second = fx
        .create(CreateParams::new("second", &fx.project).lead("claude"))
        .await;
    fx.finish(second, "done").await;
    let root2 = fx
        .shared
        .calls(|c| (c.packet["ticket"] == second).then(|| c.packet.clone()));
    let seen = bodies(&root2[0]);
    assert_eq!(seen.len(), 3);
    assert!(
        seen[0].starts_with(&format!("工单 #{first}（完成）目标：fixture")),
        "{seen:?}"
    );

    // A different project does not.
    let other = fx.dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let third = fx
        .create(CreateParams::new("other", &other).lead("claude"))
        .await;
    fx.finish(third, "done").await;
    let root3 = fx
        .shared
        .calls(|c| (c.packet["ticket"] == third).then(|| c.packet.clone()));
    assert!(root3[0].get("project_knowledge").is_none());
    // Its own progress is filed under its own project.
    let cwd3 = ticket_cwd(&fx, third);
    assert_eq!(fx.engine.knowledge(&cwd3).unwrap().len(), 1);
    assert_eq!(
        fx.engine.knowledge(&ticket_cwd(&fx, first)).unwrap().len(),
        4
    );
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
    let manual =
        fx.all("SELECT body FROM project_knowledge WHERE kind!='progress' OR body='C' ORDER BY id");
    let manual: Vec<&str> = manual.iter().map(|r| r["body"].as_str().unwrap()).collect();
    assert_eq!(manual, vec!["A", "B", "C"]);
    assert_eq!(
        rejected(&fx),
        vec![
            "invalid_json",
            "invalid_fields",
            "invalid_fields",
            "invalid_fields",
            "too_many",
            "too_many"
        ]
    );
    // Rejections never quote the content.
    assert_eq!(fx.int("SELECT count(*) FROM events WHERE kind='knowledge.rejected' AND payload LIKE '%secret%'"), 0);
    assert_eq!(
        fx.int("SELECT count(*) FROM events WHERE kind='knowledge.add'"),
        4
    );

    // The operator archives A; replaying the same reply (a follow here) does
    // not bring it back from the same job, and B/C stay single.
    let cwd = ticket_cwd(&fx, id);
    let a = fx.int("SELECT id FROM project_knowledge WHERE body='A'");
    fx.engine.archive_knowledge(a, true).unwrap();
    fx.engine.follow(id, "again", false).unwrap();
    fx.until(|| fx.int("SELECT count(*) FROM events WHERE kind='reply'") == 2)
        .await;
    assert_eq!(
        fx.int("SELECT count(*) FROM project_knowledge WHERE body IN ('A','B','C')"),
        3
    );
    assert_eq!(
        fx.int("SELECT count(*) FROM project_knowledge WHERE body='A' AND archived=0"),
        0
    );
    // Limits are per turn: the second turn was capped again.
    assert_eq!(rejected(&fx).iter().filter(|r| *r == "too_many").count(), 4);
    // A follow-up completion files a new progress entry with the follow text.
    let progress: Vec<String> = fx
        .engine
        .knowledge(&cwd)
        .unwrap()
        .into_iter()
        .filter(|e| {
            e.kind == "progress"
                && e.agent.as_deref() == Some("codex")
                && e.body.starts_with("工单")
        })
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
                // A genuine adapter failure makes the root partial.
                return Err(sa_engine::adapter::AdapterError::new(
                    sa_engine::adapter::ErrorClass::Protocol,
                    "fixture adapter failure",
                ));
            }
            if c.turn() == 1 {
                return Ok(format!(
                    "派出检查\n{}",
                    send(json!({"id": "c", "to": "claude", "body": "检查"}))
                ));
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
        json!(format!(
            "工单 #{id}（部分完成）目标：fixture\n结果：核对完毕： 两项通过"
        ))
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
    assert_eq!(
        e(fx.engine.add_knowledge(&project, "secret", "x", None)),
        "知识类型无效"
    );
    assert!(e(fx.engine.add_knowledge(&project, "memory", " ", None)).starts_with("知识内容为空"));
    assert!(
        e(fx.engine
            .add_knowledge(&project, "memory", &"x".repeat(2001), None))
        .starts_with("知识内容为空")
    );
    assert_eq!(
        e(fx.engine.add_knowledge(&project, "memory", "x", Some(99))),
        "工单不存在"
    );
    let other = fx.dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    assert_eq!(
        e(fx.engine
            .add_knowledge(other.to_str().unwrap(), "memory", "x", Some(id))),
        "工单不属于该项目"
    );
    let pinned = fx
        .engine
        .add_knowledge(&project, "memory", " 发布前先跑 clippy ", Some(id))
        .unwrap();
    assert_eq!(
        e(fx.engine
            .add_knowledge(&cwd, "lesson", "发布前先跑 clippy", None)),
        "项目知识库已有相同内容"
    );
    let log = payloads(
        &fx,
        "SELECT payload FROM events WHERE kind='knowledge.add' AND job=1 ORDER BY seq",
    );
    assert_eq!(
        log.last().unwrap(),
        &json!({"id": pinned, "kind": "memory"})
    );
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
    let lesson = fx
        .engine
        .add_knowledge(&cwd, "lesson", "旧经验", None)
        .unwrap();
    fx.engine.archive_knowledge(pinned, true).unwrap();
    fx.engine.archive_knowledge(pinned, true).unwrap();
    assert_eq!(
        payloads(
            &fx,
            "SELECT payload FROM events WHERE kind='knowledge.archived'"
        ),
        vec![json!({"id": pinned, "archived": true})]
    );
    fx.engine.follow(id, "next", false).unwrap();
    fx.until(|| fx.shared.count(|_| true) == 2).await;
    let packet = fx.shared.calls(|c| Some(c.packet.clone())).pop().unwrap();
    let entries = packet["project_knowledge"].as_array().unwrap();
    let ids: Vec<i64> = entries.iter().map(|e| e["id"].as_i64().unwrap()).collect();
    let order: Vec<_> = entries
        .iter()
        .map(|e| (e["relevance"].as_i64().unwrap(), e["id"].as_i64().unwrap()))
        .collect();
    let mut sorted = order.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(order, sorted, "relevant first, then newest");
    let total: usize = entries
        .iter()
        .map(|e| e["body"].as_str().unwrap().encode_utf16().count())
        .sum();
    assert!(total <= 6000, "{total}");
    assert!(
        ids.contains(&lesson) && ids.contains(&1),
        "every kind keeps a share: {ids:?}"
    );
    assert!(
        !ids.contains(&pinned),
        "archived entries leave the envelope"
    );
    assert_eq!(entries.iter().filter(|e| e["kind"] == "memory").count(), 3);
    assert!(
        fx.engine
            .knowledge(&cwd)
            .unwrap()
            .iter()
            .any(|e| e.id == pinned && e.archived)
    );

    // Restore, and refuse a restore that would duplicate an active entry.
    fx.engine.archive_knowledge(pinned, false).unwrap();
    fx.engine.archive_knowledge(pinned, true).unwrap();
    fx.engine
        .add_knowledge(&cwd, "memory", "发布前先跑 clippy", None)
        .unwrap();
    assert_eq!(
        fx.engine
            .archive_knowledge(pinned, false)
            .unwrap_err()
            .to_string(),
        "项目知识库已有相同内容的有效条目"
    );
    assert_eq!(
        fx.engine
            .archive_knowledge(9999, true)
            .unwrap_err()
            .to_string(),
        "知识条目不存在"
    );
}

fn retrieval_db() -> (tempfile::TempDir, rusqlite::Connection) {
    let dir = tempfile::tempdir().unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("knowledge.sqlite3")).unwrap();
    conn.execute_batch(sa_store::knowledge::SCHEMA).unwrap();
    (dir, conn)
}

fn retrieval_add(conn: &rusqlite::Connection, cwd: &str, kind: &str, body: &str) -> i64 {
    sa_engine::knowledge::operator_add(&sa_engine::db::Q(conn), cwd, kind, body, None).unwrap()
}

#[test]
fn retrieval_finds_old_path_and_symbol_beyond_recent_window() {
    let (dir, conn) = retrieval_db();
    let old = retrieval_add(
        &conn,
        "/project",
        "lesson",
        "crates/sa-ext/src/invoke.rs: invoke_call must preserve an uncertain receipt after transport loss.",
    );
    let newer = retrieval_add(
        &conn,
        "/project",
        "memory",
        "MCP supports tools and resources.",
    );
    for n in 0..600 {
        retrieval_add(
            &conn,
            "/project",
            "progress",
            &format!("unrelated deployment progress {n}"),
        );
    }
    let q = sa_engine::db::Q(&conn);
    let selected = sa_engine::knowledge::context_for(
        &q,
        "/project",
        "fix crates/sa-ext/src/invoke.rs invoke_call MCP",
    )
    .unwrap();
    assert_eq!(selected[0]["id"], old);
    assert!(selected[0]["relevance"].as_i64().unwrap() > 16);
    assert!(selected.iter().any(|e| e["id"] == newer));
    assert_eq!(selected[0]["agent"], "operator");
    assert!(selected[0]["created"].as_f64().unwrap() > 0.0);
    assert!(selected[0]["job"].is_null());
    assert!(
        selected[0].get("validated_at").is_none(),
        "created is not validation evidence"
    );
    drop(conn);
    let observer = rusqlite::Connection::open_with_flags(
        dir.path().join("knowledge.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let read = sa_store::knowledge::relevant(&observer, "/project", "invoke_call", 8).unwrap();
    assert_eq!(
        read[0].entry.id, old,
        "retrieval requires no writable FTS migration"
    );
}

#[test]
fn retrieval_chinese_archive_project_isolation_and_literal_query() {
    let (_dir, conn) = retrieval_db();
    let old = retrieval_add(
        &conn,
        "/project",
        "lesson",
        "导入中文文件名时使用 UTF-8；不要按系统代码页解码。",
    );
    for n in 0..300 {
        retrieval_add(&conn, "/project", "progress", &format!("完成无关任务 {n}"));
    }
    let archived = retrieval_add(
        &conn,
        "/project",
        "memory",
        "修复导入器的中文文件名：这条已经归档。",
    );
    conn.execute(
        "UPDATE project_knowledge SET archived=1 WHERE id=?",
        [archived],
    )
    .unwrap();
    let foreign = retrieval_add(&conn, "/other", "memory", "修复导入器的中文文件名");
    let q = sa_engine::db::Q(&conn);
    let selected =
        sa_engine::knowledge::context_for(&q, "/project", "修复导入器的中文文件名").unwrap();
    assert_eq!(selected[0]["id"], old);
    assert!(
        selected
            .iter()
            .all(|e| e["id"] != archived && e["id"] != foreign)
    );
    // Quotes, SQL and FTS operators are data, never executable query syntax.
    sa_engine::knowledge::context_for(&q, "/project", "' OR 1=1 -- \"* : ( ) NEAR").unwrap();
    assert!(sa_store::knowledge::exists(&conn).unwrap());
    assert!(
        sa_engine::knowledge::context_for(&q, "/absent", "中文文件名")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn retrieval_no_match_preserves_kind_balanced_recent_fallback() {
    let (_dir, conn) = retrieval_db();
    retrieval_add(&conn, "/p", "lesson", "retain this lesson");
    retrieval_add(&conn, "/p", "memory", "retain this memory");
    for n in 0..10 {
        retrieval_add(
            &conn,
            "/p",
            "progress",
            &format!("{n} {}", "字".repeat(1400)),
        );
    }
    let q = sa_engine::db::Q(&conn);
    let legacy = sa_engine::knowledge::context(&q, "/p").unwrap();
    for query in ["", "xylophonist_zebra_no_match"] {
        let selected = sa_engine::knowledge::context_for(&q, "/p", query).unwrap();
        assert_eq!(
            selected.iter().map(|e| &e["id"]).collect::<Vec<_>>(),
            legacy.iter().map(|e| &e["id"]).collect::<Vec<_>>()
        );
        assert!(selected.iter().all(|e| e["relevance"] == 0));
    }
}

#[test]
fn retrieval_bounds_utf16_and_entry_count_and_deduplicates_bodies() {
    let (_dir, conn) = retrieval_db();
    for n in 0..80 {
        retrieval_add(
            &conn,
            "/unicode",
            "lesson",
            &format!("needle {n} {}", "😀".repeat(900)),
        );
        retrieval_add(&conn, "/short", "memory", &format!("needle item_{n}"));
    }
    let q = sa_engine::db::Q(&conn);
    let selected = sa_engine::knowledge::context_for(&q, "/unicode", "needle").unwrap();
    assert_eq!(selected.len(), 3);
    assert!(
        selected
            .iter()
            .map(|e| e["body"].as_str().unwrap().encode_utf16().count())
            .sum::<usize>()
            <= 6000
    );
    let selected = sa_engine::knowledge::context_for(&q, "/short", "needle").unwrap();
    assert_eq!(selected.len(), 48);
    // Defensive read path also handles imported legacy duplicates with bad hashes.
    conn.execute("INSERT INTO project_knowledge(cwd,kind,body,created,sha256) VALUES('/short','memory','needle item_79',1,'legacy-duplicate')", []).unwrap();
    let selected = sa_engine::knowledge::context_for(&q, "/short", "needle").unwrap();
    assert_eq!(
        selected
            .iter()
            .filter(|e| e["body"] == "needle item_79")
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn isolated_worker_memo_is_filed_under_the_origin_project() {
    let fx = Fx::new(
        script(|_| async move { Ok("complete".into()) }),
        t1_catalog(),
    );
    let ticket = fx.create(fx.params()).await;
    let cwd = ticket_cwd(&fx, ticket);
    let worktree = fx
        .dir
        .path()
        .join("private-worktree")
        .to_string_lossy()
        .into_owned();
    fx.engine
        .db()
        .with(|q| {
            let entry = sa_engine::knowledge::NewEntry {
                cwd: &worktree,
                kind: "lesson",
                body: "worktree validation must follow merge",
                ticket: Some(ticket),
                job: Some(1),
                agent: "claude",
            };
            assert!(sa_engine::knowledge::insert(q, &entry, None)?.is_some());
            assert!(
                sa_engine::knowledge::insert(q, &entry, None)?.is_none(),
                "origin-project dedup survives worktree paths"
            );
            assert!(sa_engine::knowledge::context_for(q, &worktree, "validation")?.is_empty());
            assert_eq!(
                sa_engine::knowledge::context_for(q, &cwd, "validation")?[0]["body"],
                entry.body
            );
            Ok(())
        })
        .unwrap();
}
