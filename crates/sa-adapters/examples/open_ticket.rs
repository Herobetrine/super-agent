//! Create one ticket in a data directory without scheduling it. The desktop
//! started with `--data <dir>` then runs it under the operator's eyes.
//!
//! `open_ticket --data DIR --spec spec.json`
//!
//! spec.json: {"cwd", "goal", "lead", "profile", "selections", "requirements": [..],
//! "efficiency": {..}, "policy": "record"|"apply", "project_checks": [[argv]..],
//! "hydration": ["path"..], "isolated_writers": bool, "integration_queue": bool}
use anyhow::{Context, Result, ensure};
use sa_adapters::sys::Sys;
use sa_engine::{CreateParams, Engine, EngineOptions};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

#[derive(Deserialize)]
struct Spec {
    cwd: PathBuf,
    goal: String,
    lead: String,
    profile: String,
    selections: Value,
    #[serde(default)]
    requirements: Vec<String>,
    #[serde(default)]
    efficiency: Option<Value>,
    #[serde(default)]
    policy: Option<String>,
    #[serde(default)]
    project_checks: Vec<Vec<String>>,
    #[serde(default)]
    hydration: Vec<String>,
    #[serde(default)]
    isolated_writers: bool,
    #[serde(default)]
    integration_queue: bool,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |name: &str| {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().with_context(|| format!("缺少 {name}"))
    };
    let data = PathBuf::from(value("--data")?);
    let spec: Spec = serde_json::from_slice(&std::fs::read(value("--spec")?)?)?;
    ensure!(spec.cwd.is_dir(), "项目目录不存在");
    let mut settings = serde_json::to_value(sa_store::efficiency::Settings::default())?;
    if let Some(Value::Object(overrides)) = &spec.efficiency {
        for (key, v) in overrides {
            ensure!(settings.get(key).is_some(), "未知的效率设置：{key}");
            settings[key] = v.clone();
        }
    }
    let settings: sa_store::efficiency::Settings = serde_json::from_value(settings)?;
    std::fs::create_dir_all(&data)?;
    let engine = Engine::open(
        &data,
        sa_adapters::all(Sys::from_process()),
        EngineOptions { efficiency: sa_store::efficiency::shared(settings.clone()), ..Default::default() },
    )?;
    engine.db().with(|q| sa_store::efficiency::save(q.conn(), &settings))?;
    let project = spec.cwd.to_string_lossy().to_string();
    if !spec.project_checks.is_empty() {
        engine.set_project_checks(&project, spec.project_checks.clone())?;
    }
    if !spec.hydration.is_empty() {
        engine.set_workspace_hydration(&project, spec.hydration.clone())?;
    }
    let ticket = engine
        .create(CreateParams {
            goal: spec.goal.clone(),
            cwd: spec.cwd.clone(),
            lead: Some(spec.lead.clone()),
            permission_profile: Some(spec.profile.clone()),
            allow_lead_routing: Some(true),
            selections: Some(spec.selections.clone()),
            ..Default::default()
        })
        .await?;
    let mut contract = 0;
    for text in &spec.requirements {
        contract = engine.save_requirement(ticket, contract, None, text.clone(), None, None, None)?.contract_revision;
    }
    if spec.isolated_writers {
        engine.set_isolated_writers(ticket, true)?;
    }
    if spec.integration_queue {
        engine.set_integration_queue(ticket, true)?;
    }
    if let Some(mode) = &spec.policy {
        engine.set_execution_policy(&spec.cwd.to_string_lossy(), mode)?;
    }
    engine.close().await;
    println!("已创建工单 {ticket}（需求 {} 条，合同版本 {contract}）", spec.requirements.len());
    Ok(())
}
