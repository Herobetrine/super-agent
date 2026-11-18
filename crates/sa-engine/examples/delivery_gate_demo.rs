//! Build a disposable, model-free delivery gate fixture for native UI review.
//! The destination must be new; no scheduler or external command is started.
use anyhow::{Context, Result, ensure};
use sa_engine::{Engine, EngineOptions, db::Who};
use sa_store::delivery::{self, sha256};
use serde_json::json;
use std::{fs, path::PathBuf};

#[tokio::main]
async fn main() -> Result<()> {
    let root = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .context("provide a new absolute fixture directory")?,
    );
    ensure!(root.is_absolute(), "fixture directory must be absolute");
    let parent = root.parent().context("fixture directory needs a parent")?;
    delivery::reject_links(parent)?;
    fs::create_dir(&root).context("fixture directory must not already exist")?;
    let project = root.join("project");
    let ledger = root.join("ledger");
    fs::create_dir(&project)?;
    fs::create_dir(&ledger)?;
    let original = "预先约定的交付内容\n".as_bytes();
    fs::write(project.join("result.txt"), original)?;
    let engine = Engine::open(&ledger, vec![], EngineOptions::default())?;
    engine.db().tx(|q| {
        q.run("INSERT INTO tickets(id,goal,cwd,writes,lead,created,permission_profile) VALUES(1,'快照交付门演示',?,0,'codex',1,'readonly')", &[&project.to_string_lossy().as_ref()])?;
        q.run("INSERT INTO jobs(id,ticket,agent,body,writing,depth,state,result) VALUES(1,1,'codex','合成演示任务',0,0,'done','执行结束。交付标准和用户验收另行记录。')", &[])?;
        q.log(Who { ticket: 1, id: 1, turn: 1 }, "reply", &json!({"text":"执行结束。交付标准和用户验收另行记录。","state":"done"}))?;
        Ok(())
    })?;
    engine.save_requirement(
        1,
        0,
        None,
        "交付文件符合预先约定的内容".into(),
        Some("result.txt".into()),
        Some(sha256(original)),
        None,
    )?;
    let requirements =
        engine.save_requirement(1, 1, None, "另行评估使用体验".into(), None, None, None)?;
    let optional_id = requirements.requirements[1].id;
    let first = engine.capture_delivery(1, 2, vec!["result.txt".into()])?;
    ensure!(
        engine.prepare_delivery(1, 2, first.id, 0).is_err(),
        "disabled gate must reject preparation"
    );
    let enabled = engine.set_delivery_gate(1, 0, true)?;
    let policy = enabled.gate.policy_revision;
    let partial = engine.prepare_delivery(1, 2, first.id, policy)?;
    ensure!(
        partial.status == "partial",
        "unknown required item must prevent ready"
    );
    let revised = engine.set_requirement_disposition(
        1,
        2,
        optional_id,
        false,
        false,
        "操作员明确将体验评估作为后续可选项，本次只交付约定文件".into(),
    )?;
    ensure!(
        revised.contract_revision == 3,
        "scope edit must advance contract"
    );
    ensure!(
        engine.delivery_summary(1)?.gate.status == "stale",
        "scope revision must invalidate prior assessment"
    );
    ensure!(
        engine.prepare_delivery(1, 2, first.id, policy).is_err(),
        "old displayed revision must be rejected"
    );
    let second = engine.capture_delivery(1, 3, vec!["result.txt".into()])?;
    let ready = engine.prepare_delivery(1, 3, second.id, policy)?;
    ensure!(
        ready.status == "ready",
        "optional unknown item must not block required file standard"
    );
    fs::write(project.join("result.txt"), "后续尚未交付的改动\n")?;
    let changed = engine.delivery_summary(1)?;
    ensure!(
        changed.requirements[0].status == "stale",
        "current-file mismatch must remain visible"
    );
    ensure!(
        changed.gate.status == "ready",
        "frozen snapshot readiness must remain independent of later workspace changes"
    );
    let historical = delivery::read_snapshot(&ledger, 1, first.id)?;
    ensure!(
        historical.requirements[1].required,
        "old snapshot must preserve original required scope"
    );
    ensure!(
        historical.requirements[1].status == "unknown",
        "historical unknown cannot become pass"
    );
    let state = engine
        .db()
        .with(|q| q.one("SELECT state FROM jobs WHERE id=1", &[]))?
        .context("missing fixture job")?;
    ensure!(
        state["state"] == "done",
        "delivery gate must not alter execution state"
    );
    ensure!(
        engine
            .db()
            .with(|q| q.int("SELECT count(*) FROM events WHERE kind='turn.started'", &[]))?
            == 0,
        "fixture must not execute a model"
    );
    let receipt = json!({
        "fixture": "synthetic_no_models_no_scheduler", "ticket": 1,
        "ledger": ledger, "project": project,
        "initial": partial, "current": ready,
        "current_file_status": changed.requirements[0].status,
        "snapshot_gate_status": changed.gate.status,
        "execution_state": state["state"], "human_acceptance": "not_recorded"
    });
    fs::write(
        root.join("receipt.json"),
        serde_json::to_vec_pretty(&receipt)?,
    )?;
    engine.close().await;
    // A reopened read-only projection must retain the same independent states.
    let reopened = delivery::read_summary(&ledger, 1)?;
    ensure!(
        reopened.gate.status == "ready" && reopened.requirements[0].status == "stale",
        "read-only reopen changed delivery facts"
    );
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    Ok(())
}
