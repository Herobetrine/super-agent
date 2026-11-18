//! Model-free native UI fixture. Run with a fresh absolute directory and a Python 3 executable.
use anyhow::{Context, Result, ensure};
use sa_engine::{Engine, EngineOptions};
use sa_store::delivery;
use serde_json::json;
use std::{fs, path::PathBuf};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let root = PathBuf::from(args.next().context("expected a new absolute directory")?);
    let python = PathBuf::from(
        args.next()
            .context("expected an absolute Python 3 executable")?,
    );
    ensure!(
        root.is_absolute() && python.is_absolute() && python.is_file(),
        "invalid fixture paths"
    );
    delivery::reject_links(root.parent().context("missing parent")?)?;
    fs::create_dir(&root).context("fixture directory must not already exist")?;
    let project = root.join("project");
    let ledger = root.join("ledger");
    fs::create_dir(&project)?;
    fs::create_dir(&ledger)?;
    let project_name = project.to_str().context("project path must be UTF-8")?;
    let engine = Engine::open(&ledger, vec![], EngineOptions::default())?;
    engine.db().tx(|q| {
        q.run("INSERT INTO tickets(id,goal,cwd,writes,lead,created,permission_profile) VALUES(1,'核验商品合计金额',?,0,'codex',?,'readonly')", &[&project_name, &sa_engine::db::now()])?;
        q.run("INSERT INTO jobs(id,ticket,agent,body,writing,depth,state,result) VALUES(1,1,'codex','实现合计函数',0,0,'done','实现已提交检查。')", &[])?;
        Ok(())
    })?;
    let program = project.join("total.py");
    fs::write(
        &program,
        "def total(lines):\n    return sum(price for price, qty in lines)\n",
    )?;
    fs::write(
        project.join("check_total.py"),
        concat!(
            "from runpy import run_path\n",
            "total = run_path('total.py')['total']\n",
            "assert total([]) == 0\n",
            "assert total([(25, 2), (7, 3)]) == 71\n",
            "print('2 price/quantity cases passed')\n"
        ),
    )?;
    let argv = vec![
        python.to_str().context("Python path must be UTF-8")?.into(),
        "-I".into(),
        "-B".into(),
        "check_total.py".into(),
    ];
    let files = vec!["total.py".to_owned(), "check_total.py".to_owned()];
    engine.set_project_checks(project_name, vec![argv.clone()])?;
    let requirement = engine
        .save_requirement(
            1,
            0,
            None,
            "按单价与数量计算各项金额后求和，空清单返回零".into(),
            None,
            None,
            Some(1),
        )?
        .requirements[0]
        .id;
    let revision = engine
        .adopt_delivery_check_with_files(1, 1, requirement, argv, vec!["check_total.py".into()])?
        .contract_revision;
    engine.set_delivery_gate(1, 0, true)?;
    let first = engine.capture_delivery(1, revision, files.clone())?;
    let failed = engine.run_delivery_check(1, revision, first.id, requirement)?;
    let blocked = engine.prepare_delivery(1, revision, first.id, 1)?;
    ensure!(
        failed.status == "fail" && blocked.status == "blocked",
        "incorrect implementation passed"
    );
    fs::write(
        &program,
        "def total(lines):\n    return sum(price * qty for price, qty in lines)\n",
    )?;
    let second = engine.capture_delivery(1, revision, files)?;
    ensure!(
        engine.prepare_delivery(1, revision, second.id, 1)?.status == "blocked",
        "new input inherited an old check"
    );
    let passed = engine.run_delivery_check(1, revision, second.id, requirement)?;
    let ready = engine.prepare_delivery(1, revision, second.id, 1)?;
    ensure!(
        passed.status == "pass" && ready.status == "ready",
        "correct implementation not verified"
    );
    ensure!(
        engine.delivery_summary(1)?.gate.acceptance.is_none(),
        "verification inferred human acceptance"
    );
    // This synthetic operator action exercises the separate acceptance path.
    engine.accept_delivery(1, ready.id, "ready")?;
    fs::write(program, "def total(lines):\n    return 0\n")?;
    engine.close().await;
    let reopened = delivery::read_summary(&ledger, 1)?;
    ensure!(
        reopened.gate.status == "ready" && reopened.requirements[0].status == "stale",
        "reopened projection mixed up inputs"
    );
    let acceptance = reopened
        .gate
        .acceptance
        .context("lost the synthetic acceptance")?;
    ensure!(
        acceptance.assessment_id == ready.id && acceptance.snapshot_id == second.id,
        "accepted a different version"
    );
    let receipt = json!({"first_run":failed, "first_gate":blocked, "second_run":passed, "second_gate":ready,
        "current_project_status": reopened.requirements[0].status, "synthetic_operator_acceptance":acceptance});
    fs::write(
        root.join("receipt.json"),
        serde_json::to_vec_pretty(&receipt)?,
    )?;
    println!("{}", root.join("receipt.json").display());
    Ok(())
}
