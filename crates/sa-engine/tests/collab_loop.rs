//! Collaboration controls through scheduler admission and durable replies.

mod common;

use common::*;
use sa_engine::EngineOptions;
use sa_store::efficiency::{Settings, shared};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn report(status: &str, summary: &str) -> String {
    wire("SA_REPORT", json!({"status": status, "summary": summary}))
}

fn ask(key: &str) -> String {
    wire("SA_ASK", json!({
        "id": key, "question": "采用哪个范围？", "options": ["现有范围", "扩展范围"],
        "default": "等待操作者选择",
    }))
}

fn child(fx: &Fx, key: &str) -> i64 {
    fx.all(&format!("SELECT id FROM jobs WHERE parent IS NOT NULL AND msgkey='{key}' ORDER BY id DESC"))[0]["id"]
        .as_i64().unwrap()
}

fn state(fx: &Fx, job: i64) -> String {
    fx.all(&format!("SELECT state FROM jobs WHERE id={job}"))[0]["state"].as_str().unwrap().into()
}

fn event_count(fx: &Fx, kind: &str) -> i64 {
    fx.int(&format!("SELECT count(*) FROM events WHERE kind='{kind}'"))
}

fn has_note(packet: &Value, body: &str) -> bool {
    packet["notes"].as_array().unwrap().iter().any(|note| note["body"] == body)
}

async fn assert_parked(fx: &Fx, ticket: i64, calls: usize, turn: i64) {
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    // Waiting for children can precede the question turn. Start the no-wake
    // assertions only after the root has committed its parked question.
    fx.until(|| root_state(&fx.engine, ticket) == "waiting"
        && fx.engine.active_count() == 0
        && fx.int(&format!("SELECT count(*) FROM questions WHERE job={root} AND state='pending' AND parked=1")) == 1).await;
    for _ in 0..32 {
        assert_eq!(fx.engine.step(), 0);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(fx.shared.count(|_| true), calls);
    assert_eq!(fx.engine.root(ticket).unwrap()["turn"], turn);
    assert_eq!(fx.int("SELECT count(*) FROM questions WHERE state='pending' AND parked=1"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_report_reaches_parent_and_resolution_controls_parent_state() {
    for resolved in [false, true] {
        let fx = Fx::new(script(move |c| async move {
            if c.depth() > 0 {
                return Ok(format!("已完成调查，等待凭据。\n{}", report("blocked", "缺少目标系统凭据")));
            }
            if c.turn() == 1 { return Ok(send(json!({"id": "worker"}))); }
            let packet = c.full_packet();
            let row = &packet["reports"][0];
            assert_eq!(row["state"], "partial");
            assert_eq!(row["outcome"]["status"], "blocked");
            assert_eq!(row["outcome"]["summary"], "缺少目标系统凭据");
            assert!(row["result"].as_str().unwrap().starts_with("已完成调查，等待凭据。\n"));
            Ok(wire("SA_REPORT", if resolved {
                json!({"status": "done", "summary": "已通过本地替代数据完成核对", "resolved": [
                    {"job": row["id"], "evidence": "独立核对了本地替代数据及输出"}
                ]})
            } else {
                json!({"status": "done", "summary": "已汇总当前调查"})
            }))
        }), t1_catalog());
        let ticket = fx.begin(fx.params()).await;
        fx.finish(ticket, if resolved { "done" } else { "partial" }).await;
        assert_eq!(state(&fx, child(&fx, "worker")), "partial");
        assert_eq!(event_count(&fx, "report"), 2);
        assert_eq!(fx.int("SELECT count(*) FROM job_outcomes"), 2);
        fx.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unresolved_partial_child_does_not_withdraw_its_parents_question() {
    for self_report in [false, true] {
        let fx = Fx::new(script(move |c| async move {
            if c.depth() > 0 {
                return Ok(report("partial", "实现还有一项未完成"));
            }
            if c.turn() == 1 {
                return Ok(send(json!({"id": "worker"})));
            }
            let packet = c.full_packet();
            assert_eq!(packet["reports"].as_array().unwrap().len(), 1);
            assert_eq!(packet["reports"][0]["state"], "partial");
            if c.turn() == 2 {
                let question = ask("unfinished-scope");
                // A self-reported partial outcome must also preserve the
                // question while the unresolved child prevents completion.
                return Ok(if self_report {
                    format!("{}\n{question}", report("partial", "等待决定剩余工作的范围"))
                } else {
                    question
                });
            }
            assert_eq!(c.turn(), 3);
            assert!(has_note(&packet, "Answer to your question \"unfinished-scope\": 保留未完成项"));
            assert!(packet.get("open_question").is_none());
            Ok("已按回答保留未完成项，子任务仍需后续处理".into())
        }), t1_catalog());
        let ticket = fx.begin(fx.params().profile("readonly")).await;
        assert_parked(&fx, ticket, 3, 2).await;
        assert_eq!(state(&fx, child(&fx, "worker")), "partial");
        assert_eq!(fx.int("SELECT count(*) FROM job_failovers"), 0);
        let questions = fx.engine.questions(ticket).unwrap();
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0]["state"], "pending");
        assert_eq!(questions[0]["parked"], 1);
        assert_eq!(event_count(&fx, "ask"), 1);
        assert_eq!(event_count(&fx, "ask.withdrawn"), 0);
        let question = questions[0]["id"].as_i64().unwrap();
        fx.engine.answer(question, "保留未完成项").unwrap();
        fx.finish(ticket, "partial").await;
        assert!(fx.engine.questions(ticket).unwrap().is_empty());
        let rows = fx.all("SELECT id,state,parked,answer FROM questions");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], question);
        assert_eq!(rows[0]["state"], "answered");
        assert_eq!(rows[0]["parked"], 0);
        assert_eq!(rows[0]["answer"], "保留未完成项");
        assert_eq!(event_count(&fx, "answer"), 1);
        assert_eq!(event_count(&fx, "ask.withdrawn"), 0);
        assert_eq!(fx.shared.count(|call| call.packet["depth"] == 0), 3);
        assert_eq!(state(&fx, child(&fx, "worker")), "partial");
        fx.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reports_are_final_only_last_valid_wins_and_missing_line_clears_outcome() {
    let fx = Fx::new(script(|c| async move {
        match c.turn() {
            1 => {
                c.emit("delta", json!(format!("{}\n", report("blocked", "预览不可提交"))));
                Ok("最终完成，没有控制行".into())
            }
            2 => Ok([
                report("partial", "第一条有效结论"),
                report("done", "最终有效结论"),
                wire("SA_REPORT", json!({"status": "unknown", "summary": "无效状态"})),
            ].join("\n")),
            _ => Ok("新的回复没有结论行".into()),
        }
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    fx.finish(ticket, "done").await;
    assert_eq!(fx.int("SELECT count(*) FROM job_outcomes"), 0);
    assert_eq!(event_count(&fx, "report"), 0);
    fx.engine.follow(ticket, "提交结构化结论", false).unwrap();
    fx.finish(ticket, "done").await;
    assert_eq!(event_count(&fx, "protocol.rejected"), 1);
    let outcomes = payloads(&fx, "SELECT payload FROM job_outcomes");
    assert_eq!(outcomes[0]["status"], "done");
    assert_eq!(outcomes[0]["summary"], "最终有效结论");
    fx.engine.follow(ticket, "继续检查", false).unwrap();
    fx.finish(ticket, "done").await;
    assert_eq!(fx.int("SELECT count(*) FROM job_outcomes"), 0);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_report_has_a_durable_rejection_and_preserves_default_completion() {
    let invalid = [
        json!({"status": "blocked", "summary": " "}),
        json!({"status": "done", "summary": "🧭".repeat(301)}),
        json!({"status": "done", "summary": "ok", "verified": [""]}),
        json!({"status": "done", "summary": "ok", "artifacts": ["x".repeat(261)]}),
        json!({"status": "done", "summary": "ok", "resolved": [{"job": 2, "evidence": " "}]}),
    ];
    let text = invalid.into_iter().map(|payload| wire("SA_REPORT", payload)).collect::<Vec<_>>().join("\n");
    let fx = Fx::new(script(move |_| {
        let text = text.clone();
        async move { Ok(text) }
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    fx.finish(ticket, "done").await;
    assert_eq!(event_count(&fx, "protocol.rejected"), 5);
    assert_eq!(fx.int("SELECT count(*) FROM job_outcomes"), 0);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn root_question_parks_until_answer_and_answer_is_delivered_once() {
    let fx = Fx::new(script(|c| async move {
        if c.turn() == 1 { return Ok(ask("scope")); }
        let packet = c.full_packet();
        assert!(has_note(&packet, "Answer to your question \"scope\": 保持现有范围"));
        assert!(packet.get("open_question").is_none());
        assert_eq!(c.req.session_id.as_deref(), Some(format!("session-{}", c.job()).as_str()));
        Ok("已按回答完成".into())
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    assert_parked(&fx, ticket, 1, 1).await;
    let questions = fx.engine.questions(ticket).unwrap();
    assert_eq!(questions.len(), 1);
    let question = questions[0]["id"].as_i64().unwrap();
    for invalid in [" ".to_string(), "🧭".repeat(2001)] {
        assert!(fx.engine.answer(question, &invalid).is_err());
    }
    fx.engine.answer(question, "保持现有范围").unwrap();
    assert!(fx.engine.answer(question, "再次回答").is_err());
    fx.finish(ticket, "done").await;
    assert!(fx.engine.questions(ticket).unwrap().is_empty());
    let rows = fx.all("SELECT state,answer,closed FROM questions");
    assert_eq!(rows[0]["state"], "answered");
    assert_eq!(rows[0]["answer"], "保持现有范围");
    assert!(rows[0]["closed"].is_number());
    assert_eq!(event_count(&fx, "answer"), 1);
    assert_eq!(event_count(&fx, "ask.withdrawn"), 0);
    assert_eq!(fx.shared.count(|_| true), 2);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn asking_child_keeps_parent_asleep_when_its_sibling_finishes() {
    let sibling = Gate::new();
    let fx = Fx::new(script({
        let sibling = sibling.clone();
        move |c| {
            let sibling = sibling.clone();
            async move {
                let packet = c.full_packet();
                if c.depth() == 0 {
                    if c.turn() == 1 {
                        return Ok([
                            send(json!({"id": "asker", "body": "asker"})),
                            send(json!({"id": "sibling", "body": "sibling"})),
                        ].join("\n"));
                    }
                    assert_eq!(packet["reports"].as_array().unwrap().len(), 2);
                    assert!(packet["reports"].as_array().unwrap().iter().all(|row| row["state"] == "done"));
                    return Ok("已核对两份结论".into());
                }
                if packet["assignment"] == "sibling" {
                    sibling.wait().await;
                    return Ok("兄弟任务完成".into());
                }
                if c.turn() == 1 { return Ok(ask("child-scope")); }
                assert!(has_note(&packet, "Answer to your question \"child-scope\": 采用现有范围"));
                Ok("提问子任务完成".into())
            }
        }
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    fx.until(|| fx.engine.questions(ticket).unwrap().len() == 1
        && fx.shared.count(|call| call.packet["depth"] == 1) == 2).await;
    sibling.open();
    fx.until(|| state(&fx, child(&fx, "sibling")) == "done" && fx.engine.active_count() == 0).await;
    for _ in 0..24 { fx.engine.step(); tokio::task::yield_now().await; }
    assert_eq!(fx.shared.count(|call| call.packet["depth"] == 0), 1);
    assert_eq!(state(&fx, child(&fx, "asker")), "waiting");
    let question = fx.engine.questions(ticket).unwrap()[0]["id"].as_i64().unwrap();
    fx.engine.answer(question, "采用现有范围").unwrap();
    fx.finish(ticket, "done").await;
    assert_eq!(fx.shared.count(|call| call.packet["depth"] == 0), 2);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn question_lifetime_repeats_replaces_and_withdraws() {
    let fx = Fx::new(script(|c| async move {
        let packet = c.full_packet();
        match c.turn() {
            1 => Ok(ask("first")),
            2 => {
                assert_eq!(packet["open_question"]["key"], "first");
                assert!(packet["open_question"]["asked"].is_number());
                Ok(ask("first"))
            }
            3 => {
                assert_eq!(packet["open_question"]["key"], "first");
                Ok(ask("second"))
            }
            _ => {
                assert_eq!(packet["open_question"]["key"], "second");
                Ok("问题已由调查解决".into())
            }
        }
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    assert_parked(&fx, ticket, 1, 1).await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    let original = fx.engine.questions(ticket).unwrap()[0]["id"].clone();
    fx.engine.note(ticket, root, "请补充检查").unwrap();
    fx.until(|| fx.shared.count(|_| true) == 2 && fx.engine.active_count() == 0).await;
    assert_parked(&fx, ticket, 2, 2).await;
    assert_eq!(fx.engine.questions(ticket).unwrap()[0]["id"], original);
    assert_eq!(event_count(&fx, "ask"), 1);
    fx.engine.note(ticket, root, "请更新需要的决定").unwrap();
    fx.until(|| fx.shared.count(|_| true) == 3 && fx.engine.active_count() == 0).await;
    assert_parked(&fx, ticket, 3, 3).await;
    assert_ne!(fx.engine.questions(ticket).unwrap()[0]["id"], original);
    assert_eq!(event_count(&fx, "ask"), 2);
    assert_eq!(event_count(&fx, "ask.withdrawn"), 1);
    fx.engine.note(ticket, root, "已找到调查依据").unwrap();
    fx.finish(ticket, "done").await;
    assert!(fx.engine.questions(ticket).unwrap().is_empty());
    assert_eq!(event_count(&fx, "ask.withdrawn"), 2);
    assert_eq!(fx.int("SELECT count(*) FROM questions WHERE state='withdrawn'"), 2);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn operator_followup_withdraws_and_wakes_a_parked_readonly_root() {
    let fx = Fx::new(script(|c| async move {
        if c.turn() == 1 { return Ok(ask("follow")); }
        let packet = c.full_packet();
        assert!(packet.get("open_question").is_none());
        assert!(packet["user_updates"].as_array().unwrap().iter().any(|row| row == "直接采用现有范围"));
        Ok("已完成追加指令".into())
    }), t1_catalog());
    let ticket = fx.begin(fx.params().profile("readonly")).await;
    assert_parked(&fx, ticket, 1, 1).await;
    fx.engine.follow(ticket, "直接采用现有范围", false).unwrap();
    fx.finish(ticket, "done").await;
    assert_eq!(fx.shared.count(|_| true), 2);
    assert_eq!(event_count(&fx, "ask.withdrawn"), 1);
    assert!(fx.engine.questions(ticket).unwrap().is_empty());
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_roots_park_before_advancing_and_answers_resume_the_same_phase() {
    for continuous in [true, false] {
        let resumed = Gate::new();
        let fx = Fx::new(script({
            let resumed = resumed.clone();
            move |c| {
                let resumed = resumed.clone();
                async move {
                    if c.turn() == 1 {
                        return Ok(format!("{}\n{}\n{}", ask("auto-scope"),
                            wire("SA_NEXT", json!({"body": "这条自动推进必须被提问阻止"})),
                            wire("SA_FINISH", json!({"status": "done", "summary": "这条完成也必须被提问阻止"}))));
                    }
                    assert_eq!(c.phase(), if continuous { Some("work") } else { None });
                    assert_eq!(c.req.session_id.as_deref(), Some(format!("session-{}", c.job()).as_str()));
                    assert!(has_note(&c.full_packet(), "Answer to your question \"auto-scope\": 继续当前阶段"));
                    resumed.open();
                    c.cancel.cancelled().await;
                    Ok("测试结束".into())
                }
            }
        }), t1_catalog());
        let ticket = fx.create(fx.params().profile("auto")).await;
        if !continuous {
            fx.engine.db().with(|q| q.run("DELETE FROM continuous_runs WHERE ticket=?", &[&ticket])).unwrap();
        }
        fx.engine.start().unwrap();
        assert_parked(&fx, ticket, 1, 1).await;
        assert_eq!(event_count(&fx, "auto.next"), 0);
        assert_eq!(event_count(&fx, "auto.backoff"), 0);
        assert_eq!(event_count(&fx, "auto.milestone"), 0);
        if continuous {
            assert_eq!(fx.all("SELECT phase FROM continuous_runs")[0]["phase"], "work");
        }
        let question = fx.engine.questions(ticket).unwrap()[0]["id"].as_i64().unwrap();
        fx.engine.answer(question, "继续当前阶段").unwrap();
        fx.until(|| resumed.is_open()).await;
        fx.engine.cancel(ticket).unwrap();
        fx.until(|| fx.engine.active_count() == 0).await;
        fx.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continue_resumes_native_session_with_one_note_and_one_new_result() {
    let fx = Fx::new(script(|c| async move {
        let packet = c.full_packet();
        if c.depth() > 0 {
            if c.turn() == 1 { return Ok("第一版结果".into()); }
            assert_eq!(c.turn(), 2);
            assert_eq!(c.req.session_id.as_deref(), Some(format!("session-{}", c.job()).as_str()));
            assert_eq!(packet["assignment"], "原始任务");
            assert!(has_note(&packet, "请修复审查发现的边界条件"));
            return Ok("修复后的完整结果\n最后一行".into());
        }
        match c.turn() {
            1 => Ok(send(json!({"id": "writer", "body": "原始任务"}))),
            2 => {
                let command = wire("SA_SEND", json!({"id": "fix", "continue": packet["reports"][0]["id"],
                    "body": "请修复审查发现的边界条件"}));
                c.emit("delta", json!(format!("{command}\n{command}\n")));
                Ok(format!("{command}\n{command}"))
            }
            _ => {
                assert_eq!(packet["reports"][0]["result"], "修复后的完整结果\n最后一行");
                assert_eq!(packet["reports"][0]["state"], "done");
                Ok("已复核修复".into())
            }
        }
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    fx.finish(ticket, "done").await;
    let worker = child(&fx, "writer");
    assert_eq!(fx.shared.count(|call| call.packet["depth"] == 1), 2);
    assert_eq!(event_count(&fx, "continued"), 1);
    assert_eq!(fx.int(&format!("SELECT count(*) FROM mailbox WHERE target={worker} AND source IS NOT NULL")), 1);
    assert_eq!(fx.int("SELECT count(*) FROM jobs"), 2);
    assert_eq!(fx.all(&format!("SELECT body,turn FROM jobs WHERE id={worker}"))[0]["body"], "原始任务");
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continue_rejects_forbidden_targets_fields_and_conflicting_replays() {
    for case in ["running", "cancelled", "failed", "foreign", "nonchild", "to", "after", "conflict"] {
        let target = Arc::new(Mutex::new(None::<i64>));
        let fx = Fx::new(script({
            let target = target.clone();
            move |c| {
                let target = target.clone();
                async move {
                    if c.depth() > 0 { return Ok("子任务完成".into()); }
                    if c.turn() == 1 { return Ok(send(json!({"id": "worker"}))); }
                    if c.turn() == 2 { return Ok("初次汇总".into()); }
                    let job = target.lock().unwrap().unwrap();
                    let mut command = json!({"id": "resume", "continue": job, "body": "继续核对"});
                    if case == "to" { command["to"] = json!("claude"); }
                    if case == "after" { command["after"] = json!([job]); }
                    if case == "conflict" {
                        let first = wire("SA_SEND", command.clone());
                        command["body"] = json!("同键不同正文");
                        return Ok(format!("{first}\n{}", wire("SA_SEND", command)));
                    }
                    Ok(wire("SA_SEND", command))
                }
            }
        }), t1_catalog());
        let ticket = fx.begin(fx.params()).await;
        fx.finish(ticket, "done").await;
        let worker = child(&fx, "worker");
        let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
        let selected = match case {
            "foreign" => {
                let other = fx.create(fx.params()).await;
                fx.engine.root(other).unwrap()["id"].as_i64().unwrap()
            }
            "nonchild" => root,
            _ => worker,
        };
        // Establish each forbidden state on an existing, normally dispatched child.
        if matches!(case, "running" | "cancelled" | "failed") {
            fx.engine.db().with(|q| q.run("UPDATE jobs SET state=? WHERE id=?", &[&case, &worker])).unwrap();
        }
        *target.lock().unwrap() = Some(selected);
        fx.engine.follow(ticket, "检查续派授权", false).unwrap();
        fx.until(|| event_count(&fx, "protocol.rejected") == 1).await;
        let rejection = &payloads(&fx, "SELECT payload FROM events WHERE kind='protocol.rejected'")[0];
        assert_eq!(rejection["executed"], false, "{case}");
        assert!(rejection["reason"].as_str().is_some_and(|reason| !reason.is_empty()), "{case}");
        assert_eq!(event_count(&fx, "continued"), i64::from(case == "conflict"), "{case}");
        fx.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dependent_dispatch_waits_and_delivers_full_prerequisite_with_one_parent_wake() {
    let implementer = Gate::new();
    let result = format!("实现结果\n{}\n最后一行", "完整内容".repeat(12_000));
    let fx = Fx::with(script({
        let (implementer, result) = (implementer.clone(), result.clone());
        move |c| {
            let (implementer, result) = (implementer.clone(), result.clone());
            async move {
                let packet = c.full_packet();
                if c.depth() == 0 {
                    if c.turn() == 1 {
                        return Ok([
                            send(json!({"id": "impl", "body": "implement"})),
                            send(json!({"id": "review", "body": "review", "after": ["impl"]})),
                        ].join("\n"));
                    }
                    assert_eq!(packet["reports"].as_array().unwrap().len(), 2);
                    assert!(packet["reports"].as_array().unwrap().iter().all(|row| row["state"] == "done"));
                    return Ok("已核对实现和审查".into());
                }
                if packet["assignment"] == "implement" {
                    implementer.wait().await;
                    return Ok(format!("{result}\n{}", report("done", "实现完成")));
                }
                let prerequisites = packet["prerequisites"].as_array().unwrap();
                assert_eq!(prerequisites.len(), 1);
                assert_eq!(prerequisites[0]["agent"], "deepseek");
                assert_eq!(prerequisites[0]["state"], "done");
                assert_eq!(prerequisites[0]["outcome"]["summary"], "实现完成");
                assert_eq!(prerequisites[0]["result"], format!("{result}\n{}", report("done", "实现完成")));
                Ok("审查通过".into())
            }
        }
    }), t1_catalog(), EngineOptions {
        efficiency: shared(Settings { max_collaborators: 1, ..Default::default() }),
        ..quiet()
    });
    let ticket = fx.begin(fx.params()).await;
    fx.until(|| fx.int("SELECT count(*) FROM job_deps") == 1
        && fx.shared.count(|call| call.packet["assignment"] == "implement") == 1).await;
    for _ in 0..24 { fx.engine.step(); tokio::task::yield_now().await; }
    assert_eq!(fx.shared.count(|call| call.packet["assignment"] == "review"), 0);
    assert_eq!(state(&fx, child(&fx, "review")), "queued");
    assert_eq!(fx.shared.count(|call| call.packet["depth"] == 0), 1);
    implementer.open();
    fx.finish(ticket, "done").await;
    assert_eq!(fx.shared.count(|call| call.packet["depth"] == 0), 2);
    assert_eq!(fx.shared.count(|call| call.packet["assignment"] == "review"), 1);
    assert_eq!(event_count(&fx, "skipped"), 0);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_prerequisite_skips_dependents_and_unknown_reference_is_rejected() {
    let fx = Fx::new(script(|c| async move {
        if c.depth() > 0 {
            assert_eq!(c.full_packet()["assignment"], "implement");
            return Ok(report("partial", "实现剩余一项未完成"));
        }
        if c.turn() == 1 {
            return Ok([
                send(json!({"id": "impl", "body": "implement"})),
                send(json!({"id": "review", "body": "review", "after": ["impl"]})),
                send(json!({"id": "publish", "body": "publish", "after": ["review"]})),
                send(json!({"id": "unknown", "after": ["missing"]})),
            ].join("\n"));
        }
        Ok("已汇总未完成项和跳过原因".into())
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    fx.finish(ticket, "partial").await;
    let implementer = child(&fx, "impl");
    let review = child(&fx, "review");
    assert_eq!(state(&fx, review), "cancelled");
    assert_eq!(state(&fx, child(&fx, "publish")), "cancelled");
    assert_eq!(fx.all(&format!("SELECT result FROM jobs WHERE id={review}"))[0]["result"],
        format!("前置任务 #{implementer} 以「partial」结束，本任务未执行。"));
    assert_eq!(event_count(&fx, "skipped"), 2);
    assert_eq!(event_count(&fx, "protocol.rejected"), 1);
    let rejection = &payloads(&fx, "SELECT payload FROM events WHERE kind='protocol.rejected'")[0];
    assert!(rejection["reason"].as_str().unwrap().contains("after 引用了不存在的任务"));
    assert_eq!(fx.shared.count(|call| call.packet["depth"] == 1), 1);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parked_question_outcome_and_unmet_dependency_survive_restart() {
    let mut fx = Fx::new(script(|c| async move {
        let packet = c.full_packet();
        if c.depth() == 0 {
            if c.turn() == 1 {
                return Ok([
                    send(json!({"id": "impl", "body": "implement"})),
                    send(json!({"id": "review", "body": "review", "after": ["impl"]})),
                ].join("\n"));
            }
            assert!(packet["reports"].as_array().unwrap().iter().all(|row| row["state"] == "done"));
            return Ok("重启后完成完整闭环".into());
        }
        if packet["assignment"] == "review" {
            assert_eq!(packet["prerequisites"][0]["result"], "回答后完成实现");
            return Ok("重启后审查通过".into());
        }
        if c.turn() == 1 {
            return Ok(format!("{}\n{}", report("done", "已准备，等待范围确认"), ask("restart-scope")));
        }
        assert!(has_note(&packet, "Answer to your question \"restart-scope\": 继续现有范围"));
        assert_eq!(c.req.session_id.as_deref(), Some(format!("session-{}", c.job()).as_str()));
        Ok("回答后完成实现".into())
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    fx.until(|| fx.engine.questions(ticket).unwrap().len() == 1 && fx.engine.active_count() == 0).await;
    let implementer = child(&fx, "impl");
    let reviewer = child(&fx, "review");
    let pending = fx.engine.questions(ticket).unwrap();
    let outcome = fx.all("SELECT * FROM job_outcomes");
    let deps = fx.all("SELECT * FROM job_deps");
    assert_eq!(state(&fx, implementer), "waiting");
    assert_eq!(state(&fx, reviewer), "queued");
    assert_eq!(outcome.len(), 1);
    fx.restart(quiet()).await;
    assert_eq!(fx.engine.questions(ticket).unwrap(), pending);
    assert_eq!(fx.all("SELECT * FROM job_outcomes"), outcome);
    assert_eq!(fx.all("SELECT * FROM job_deps"), deps);
    fx.engine.start().unwrap();
    for _ in 0..32 { fx.engine.step(); tokio::task::yield_now().await; }
    assert_eq!(fx.shared.count(|_| true), 2);
    assert_eq!(fx.int("SELECT parked FROM questions WHERE state='pending'"), 1);
    fx.engine.answer(pending[0]["id"].as_i64().unwrap(), "继续现有范围").unwrap();
    fx.finish(ticket, "done").await;
    assert_eq!(fx.shared.count(|call| call.packet["depth"] == 0), 2);
    assert_eq!(state(&fx, reviewer), "done");
    assert_eq!(fx.int(&format!("SELECT count(*) FROM job_outcomes WHERE job={implementer}")), 0);
    assert_eq!(fx.int("SELECT count(*) FROM questions WHERE state='answered'"), 1);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waiting_dependencies_use_neither_the_collaborator_slot_nor_reader_slice() {
    let independent = Gate::new();
    let fx = Fx::with(script({
        let independent = independent.clone();
        move |c| {
            let independent = independent.clone();
            async move {
                let packet = c.full_packet();
                if c.depth() == 0 {
                    if c.turn() > 1 { return Ok("全部工作已核对".into()); }
                    let mut commands = vec![send(json!({"id": "impl", "body": "implement"}))];
                    for index in 0..12 {
                        commands.push(send(json!({"id": format!("review-{index}"),
                            "body": "review", "after": ["impl"]})));
                    }
                    commands.push(send(json!({"id": "independent", "body": "independent"})));
                    return Ok(commands.join("\n"));
                }
                if packet["assignment"] == "implement" {
                    return Ok(if c.turn() == 1 { ask("slot") } else { "实现完成".into() });
                }
                if packet["assignment"] == "independent" {
                    independent.wait().await;
                    return Ok("独立工作完成".into());
                }
                assert_eq!(packet["prerequisites"][0]["state"], "done");
                Ok("审查完成".into())
            }
        }
    }), t1_catalog(), EngineOptions {
        efficiency: shared(Settings { max_collaborators: 1, ..Default::default() }),
        ..quiet()
    });
    let ticket = fx.begin(fx.params()).await;
    fx.until(|| fx.shared.count(|call| call.packet["assignment"] == "independent") == 1).await;
    assert_eq!(fx.int("SELECT count(*) FROM job_deps"), 12);
    assert_eq!(fx.int("SELECT count(*) FROM jobs WHERE parent IS NOT NULL AND state='queued'"), 12);
    assert_eq!(fx.shared.count(|call| call.packet["assignment"] == "review"), 0);
    assert_eq!(fx.int("SELECT count(*) FROM jobs WHERE parent IS NOT NULL AND state='running'"), 1);
    let question = fx.engine.questions(ticket).unwrap()[0]["id"].as_i64().unwrap();
    fx.engine.answer(question, "继续实现").unwrap();
    independent.open();
    fx.finish(ticket, "done").await;
    assert_eq!(fx.shared.count(|call| call.packet["assignment"] == "review"), 12);
    assert_eq!(fx.shared.count(|call| call.packet["depth"] == 0), 2);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dependency_replays_compare_resolved_sets_and_numeric_child_references_work() {
    let release = Gate::new();
    let fx = Fx::new(script({
        let release = release.clone();
        move |c| {
            let release = release.clone();
            async move {
                let packet = c.full_packet();
                if c.depth() > 0 {
                    if packet["assignment"] == "implement" { release.wait().await; }
                    return Ok("依赖已验证".into());
                }
                match c.turn() {
                    1 => Ok([
                        send(json!({"id": "impl", "body": "implement"})),
                        send(json!({"id": "other", "body": "implement"})),
                        send(json!({"id": "review", "body": "review", "after": ["impl", "other"]})),
                        send(json!({"id": "review", "body": "review", "after": ["other", "impl", "impl"]})),
                        send(json!({"id": "review", "body": "review", "after": ["impl"]})),
                    ].join("\n")),
                    2 => {
                        let original = packet["reports"].as_array().unwrap().iter()
                            .find(|row| row["result"] == "依赖已验证").unwrap();
                        Ok(send(json!({"id": "numeric-review", "body": "review", "after": [original["id"]]})))
                    }
                    _ => Ok("依赖引用与幂等已核对".into()),
                }
            }
        }
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    fx.until(|| event_count(&fx, "protocol.rejected") == 1).await;
    assert_eq!(fx.int("SELECT count(*) FROM job_deps"), 2);
    assert_eq!(fx.int("SELECT count(*) FROM jobs WHERE msgkey='review'"), 1);
    release.open();
    fx.finish(ticket, "done").await;
    assert_eq!(event_count(&fx, "protocol.rejected"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM jobs WHERE msgkey='numeric-review'"), 1);
    assert_eq!(fx.int("SELECT count(*) FROM job_deps"), 3);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ticket_cancellation_withdraws_pending_questions_and_blocks_answers() {
    let fx = Fx::new(script(|_| async { Ok(ask("cancelled")) }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    assert_parked(&fx, ticket, 1, 1).await;
    let question = fx.engine.questions(ticket).unwrap()[0]["id"].as_i64().unwrap();
    fx.engine.cancel(ticket).unwrap();
    assert_eq!(root_state(&fx.engine, ticket), "cancelled");
    assert!(fx.engine.questions(ticket).unwrap().is_empty());
    assert!(fx.engine.answer(question, "过期回答").is_err());
    assert_eq!(fx.int("SELECT count(*) FROM questions WHERE state='withdrawn'"), 1);
    assert_eq!(event_count(&fx, "ask.withdrawn"), 1);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ask_uses_only_the_last_valid_final_line() {
    let fx = Fx::new(script(|c| async move {
        if c.turn() == 1 {
            c.emit("delta", json!(format!("{}\n", ask("preview"))));
            return Ok(wire("SA_ASK", json!({"id": "blank", "question": " "})));
        }
        Ok([
            ask("earlier"), ask("last-valid"),
            wire("SA_ASK", json!({"id": "too-many-options", "question": "采用哪个范围？",
                "options": ["a", "b", "c", "d", "e"]})),
            wire("SA_ASK", json!({"id": "long", "question": "🧭".repeat(251)})),
        ].join("\n"))
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    fx.finish(ticket, "done").await;
    assert!(fx.engine.questions(ticket).unwrap().is_empty());
    assert_eq!(event_count(&fx, "ask"), 0);
    assert_eq!(event_count(&fx, "protocol.rejected"), 1);
    fx.engine.follow(ticket, "再说明所需决定", false).unwrap();
    assert_parked(&fx, ticket, 2, 2).await;
    assert_eq!(fx.all("SELECT msgkey FROM questions WHERE state='pending'")[0]["msgkey"], "last-valid");
    assert_eq!(event_count(&fx, "ask"), 1);
    assert_eq!(event_count(&fx, "protocol.rejected"), 3);
    fx.engine.cancel(ticket).unwrap();
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retrying_a_failed_child_unparks_its_questioning_ancestor() {
    let fx = Fx::new(script(|c| async move {
        if c.depth() > 0 { return Ok("子任务已重新核对".into()); }
        if c.turn() == 1 { return Ok(send(json!({"id": "worker"}))); }
        Ok("父任务已核对最新结果".into())
    }), t1_catalog());
    let ticket = fx.begin(fx.params()).await;
    fx.finish(ticket, "done").await;
    fx.until(|| fx.engine.active_count() == 0).await;
    let worker = child(&fx, "worker");
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    // This persisted combination can occur when an operator retries a child
    // after its parent has already asked about the failed result.
    fx.engine.db().tx(|q| {
        q.run("UPDATE jobs SET state='failed' WHERE id=?", &[&worker])?;
        q.run("UPDATE jobs SET state='waiting' WHERE id=?", &[&root])?;
        q.run("INSERT INTO questions(ticket,job,turn,msgkey,question,parked,asked) VALUES(?,?,2,'retry-parent','如何处理失败？',1,?)",
            &[&ticket, &root, &sa_engine::db::now()])?;
        Ok(())
    }).unwrap();
    fx.engine.retry(worker).unwrap();
    assert_eq!(fx.int("SELECT parked FROM questions WHERE state='pending'"), 0);
    assert_eq!(state(&fx, worker), "queued");
    fx.finish(ticket, "done").await;
    assert_eq!(fx.shared.count(|call| call.packet["depth"] == 1), 2);
    assert!(fx.engine.questions(ticket).unwrap().is_empty());
    assert_eq!(event_count(&fx, "ask.withdrawn"), 1);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dependency_follows_the_entire_failover_chain() {
    for complete in [true, false] {
        let release = Gate::new();
        let effective = Arc::new(Mutex::new(0i64));
        let fx = Fx::new(script({
            let (release, effective) = (release.clone(), effective.clone());
            move |c| {
                let (release, effective) = (release.clone(), effective.clone());
                async move {
                    let packet = c.full_packet();
                    if c.depth() == 0 {
                        if c.turn() == 1 {
                            return Ok(send(json!({"id": "review", "body": "review", "after": ["original"]})));
                        }
                        return Ok("已核对自动改派链及审查结果".into());
                    }
                    if packet["assignment"] == "replacement" {
                        release.wait().await;
                        return Ok(report(if complete { "done" } else { "partial" }, "最终替代执行者的结果"));
                    }
                    assert!(complete, "a partial effective dependency must skip review");
                    assert_eq!(packet["prerequisites"].as_array().unwrap().len(), 1);
                    assert_eq!(packet["prerequisites"][0]["job"], *effective.lock().unwrap());
                    assert_eq!(packet["prerequisites"][0]["outcome"]["summary"], "最终替代执行者的结果");
                    Ok("审查替代结果完成".into())
                }
            }
        }), t1_catalog());
        let ticket = fx.create(fx.params()).await;
        let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
        let (original, replacement) = fx.engine.db().tx(|q| {
            let mut jobs = Vec::new();
            for (key, state) in [("original", "cancelled"), ("intermediate", "cancelled"), ("replacement", "queued")] {
                jobs.push(q.insert("INSERT INTO jobs(ticket,parent,sender,agent,body,writing,depth,state,source_turn,msgkey)
                    VALUES(?,?,'codex','deepseek','replacement',0,1,?,1,?)", &[&ticket, &root, &state, &key])?);
            }
            q.run("INSERT INTO job_failovers(original,replacement) VALUES(?,?),(?,?)", &[&jobs[0], &jobs[1], &jobs[1], &jobs[2]])?;
            Ok((jobs[0], jobs[2]))
        }).unwrap();
        *effective.lock().unwrap() = replacement;
        fx.engine.start().unwrap();
        fx.until(|| fx.int("SELECT count(*) FROM job_deps") == 1 && state(&fx, replacement) == "running").await;
        for _ in 0..20 { fx.engine.step(); tokio::task::yield_now().await; }
        assert_eq!(state(&fx, child(&fx, "review")), "queued");
        assert_eq!(event_count(&fx, "skipped"), 0);
        assert_eq!(fx.int("SELECT needs FROM job_deps"), original);
        release.open();
        fx.finish(ticket, if complete { "done" } else { "partial" }).await;
        assert_eq!(state(&fx, child(&fx, "review")), if complete { "done" } else { "cancelled" });
        assert_eq!(event_count(&fx, "skipped"), i64::from(!complete));
        if !complete {
            let skipped = &payloads(&fx, "SELECT payload FROM events WHERE kind='skipped'")[0];
            assert_eq!(skipped["needs"], replacement);
            assert_eq!(skipped["state"], "partial");
        }
        fx.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continued_prerequisite_blocks_a_dependent_that_has_not_started() {
    let independent = Gate::new();
    let implementation = Gate::new();
    let fx = Fx::with(script({
        let (independent, implementation) = (independent.clone(), implementation.clone());
        move |c| {
            let (independent, implementation) = (independent.clone(), implementation.clone());
            async move {
                let packet = c.full_packet();
                if c.depth() == 0 {
                    if c.turn() == 1 {
                        return Ok([
                            send(json!({"id": "impl", "body": "implement"})),
                            send(json!({"id": "independent", "body": "independent"})),
                            send(json!({"id": "review", "body": "review", "after": ["impl"]})),
                        ].join("\n"));
                    }
                    if c.turn() == 2 {
                        let worker = packet["reports"].as_array().unwrap().iter()
                            .find(|row| row["result"] == "第一版实现").unwrap();
                        return Ok(wire("SA_SEND", json!({"id": "fix", "continue": worker["id"], "body": "修复后再交给审查"})));
                    }
                    return Ok("已核对修复和审查".into());
                }
                if packet["assignment"] == "independent" {
                    independent.wait().await;
                    return Ok("独立工作完成".into());
                }
                if packet["assignment"] == "implement" {
                    if c.turn() == 1 { return Ok("第一版实现".into()); }
                    implementation.wait().await;
                    return Ok("第二版实现".into());
                }
                assert_eq!(packet["prerequisites"][0]["result"], "第二版实现");
                Ok("第二版审查完成".into())
            }
        }
    }), t1_catalog(), EngineOptions {
        efficiency: shared(Settings { max_collaborators: 1, ..Default::default() }),
        ..quiet()
    });
    let ticket = fx.begin(fx.params()).await;
    fx.until(|| fx.shared.count(|call| call.packet["assignment"] == "independent") == 1
        && root_state(&fx.engine, ticket) == "waiting").await;
    let root = fx.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
    fx.engine.note(ticket, root, "请让原实现者修复后再审查").unwrap();
    fx.until(|| event_count(&fx, "continued") == 1 && root_state(&fx.engine, ticket) == "waiting").await;
    assert_eq!(state(&fx, child(&fx, "impl")), "queued");
    independent.open();
    fx.until(|| fx.shared.count(|call| call.packet["assignment"] == "implement" && call.packet["turn"] == 2) == 1).await;
    for _ in 0..20 { fx.engine.step(); tokio::task::yield_now().await; }
    assert_eq!(fx.shared.count(|call| call.packet["assignment"] == "review"), 0);
    assert_eq!(state(&fx, child(&fx, "review")), "queued");
    implementation.open();
    fx.finish(ticket, "done").await;
    assert_eq!(fx.shared.count(|call| call.packet["depth"] == 0), 3);
    fx.engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_child_can_continue_but_merged_replaced_and_old_phase_children_cannot() {
    for case in ["partial", "merged", "replaced", "floor"] {
        let target = Arc::new(Mutex::new(None::<i64>));
        let fx = Fx::new(script({
            let target = target.clone();
            move |c| {
                let target = target.clone();
                async move {
                    let packet = c.full_packet();
                    if c.depth() > 0 {
                        return Ok(if c.turn() == 1 { report("partial", "需要后续补充") } else { "补充完成".into() });
                    }
                    if c.turn() == 1 { return Ok(send(json!({"id": "worker"}))); }
                    if c.turn() == 2 { return Ok("已收到未完成结论".into()); }
                    if c.turn() == 3 {
                        let target = target.lock().unwrap().unwrap();
                        return Ok(wire("SA_SEND", json!({"id": "resume-partial", "continue": target, "body": "补齐剩余工作"})));
                    }
                    assert_eq!(packet["reports"][0]["result"], "补充完成");
                    Ok("已核对补充结果".into())
                }
            }
        }), t1_catalog());
        let ticket = fx.begin(fx.params()).await;
        fx.finish(ticket, "partial").await;
        fx.until(|| fx.engine.active_count() == 0).await;
        let worker = child(&fx, "worker");
        *target.lock().unwrap() = Some(worker);
        fx.engine.db().tx(|q| {
            if case == "merged" {
                q.run("INSERT INTO job_workspaces(job,ticket,source,path,branch,state) VALUES(?,?,'fixture','fixture','fixture','merged')",
                    &[&worker, &ticket])?;
            }
            if case == "replaced" {
                let replacement = q.insert("INSERT INTO jobs(ticket,parent,sender,agent,body,writing,depth,state,result)
                    SELECT ticket,parent,sender,agent,body,writing,depth,'done','替代任务完成' FROM jobs WHERE id=?", &[&worker])?;
                q.run("INSERT INTO job_failovers(original,replacement) VALUES(?,?)", &[&worker, &replacement])?;
            }
            if case == "floor" {
                q.run("INSERT INTO continuous_runs(ticket,boundary_job) VALUES(?,?)", &[&ticket, &worker])?;
            }
            Ok(())
        }).unwrap();
        fx.engine.follow(ticket, "检查部分结果的续派", false).unwrap();
        if case == "partial" {
            fx.finish(ticket, "done").await;
            assert_eq!(event_count(&fx, "continued"), 1);
            assert_eq!(event_count(&fx, "protocol.rejected"), 0);
            assert_eq!(fx.shared.count(|call| call.packet["depth"] == 1), 2);
        } else {
            fx.until(|| event_count(&fx, "protocol.rejected") == 1).await;
            assert_eq!(event_count(&fx, "continued"), 0, "{case}");
            assert_eq!(fx.shared.count(|call| call.packet["depth"] == 1), 1, "{case}");
        }
        fx.engine.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_outcome_and_question_do_not_suppress_valid_auto_finish() {
    for continuous in [true, false] {
        let fx = Fx::new(script(move |c| async move {
            if c.turn() == 1 {
                return Ok([
                    wire("SA_REPORT", json!({"status": "invalid", "summary": "无效结论应被忽略"})),
                    wire("SA_ASK", json!({"id": "invalid-question", "question": " "})),
                    wire("SA_FINISH", json!({"status": "done", "summary": "已独立核对当前阶段的结果"})),
                ].join("\n"));
            }
            assert!(continuous, "a finished legacy root must not be readmitted");
            assert_eq!(c.phase(), Some("discover"));
            c.cancel.cancelled().await;
            Ok("发现轮测试结束".into())
        }), t1_catalog());
        let ticket = fx.create(fx.params().profile("auto")).await;
        if !continuous {
            fx.engine.db().with(|q| q.run("DELETE FROM continuous_runs WHERE ticket=?", &[&ticket])).unwrap();
        }
        fx.engine.start().unwrap();
        if continuous {
            fx.until(|| event_count(&fx, "auto.milestone") == 1).await;
            let runs = fx.all("SELECT phase,milestone FROM continuous_runs");
            assert_eq!(runs[0]["phase"], "discover");
            assert_eq!(runs[0]["milestone"], 1);
        } else {
            fx.finish(ticket, "done").await;
            assert_eq!(event_count(&fx, "auto.finish"), 1);
            assert_eq!(fx.shared.count(|_| true), 1);
        }
        assert_eq!(event_count(&fx, "protocol.rejected"), 2);
        let receipts = payloads(&fx, "SELECT payload FROM events WHERE kind='protocol.rejected' ORDER BY seq");
        assert_eq!(receipts[0]["tag"], "SA_REPORT");
        assert_eq!(receipts[1]["tag"], "SA_ASK");
        assert_eq!(event_count(&fx, "auto.next"), 0);
        assert_eq!(event_count(&fx, "auto.backoff"), 0);
        assert_eq!(fx.int("SELECT count(*) FROM job_outcomes"), 0);
        assert!(fx.engine.questions(ticket).unwrap().is_empty());
        if continuous {
            fx.engine.cancel(ticket).unwrap();
            fx.until(|| fx.engine.active_count() == 0).await;
        }
        fx.engine.close().await;
    }
}
