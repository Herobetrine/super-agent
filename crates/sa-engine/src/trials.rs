//! Controlled trials over a fixed task suite (architecture §13, P5).
//!
//! A suite is a versioned file of small representative tasks. Each task has a
//! fixture project, operator requirements (shown to agents), hidden
//! acceptance checks (never copied into the project) and a reference
//! solution used only to validate the suite itself. A trial runs one task
//! under one variant in a fresh temporary project and journal, drives the
//! scripted operator events, answers questions with their declared default,
//! confirms integration candidates, and records a run with acceptance,
//! delivery, spending, time, interactions and rework. Reports aggregate runs
//! per task and variant; nothing here calls a model unless the caller passes
//! real adapters (the CLI requires an explicit billable flag for that).

use crate::{CreateParams, Engine, EngineOptions, adapter::Adapter, workspaces};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

pub const RECORD_VERSION: u32 = 1;
const CHECK_TIMEOUT: Duration = Duration::from_secs(600);
const TERMINAL: &[&str] = &["done", "partial", "failed", "cancelled", "interrupted"];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suite {
    pub schema_version: u32,
    pub id: String,
    pub version: String,
    pub tasks: Vec<Task>,
}

fn full() -> String {
    "full".into()
}
fn thirty() -> u64 {
    30
}
fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    /// One of the §13.2 categories, e.g. `small_change`, `cross_module`.
    pub category: String,
    /// Directory relative to the suite file holding `project/`,
    /// `acceptance/` and `solution/`.
    pub fixture: String,
    pub goal: String,
    #[serde(default)]
    pub lead: Option<String>,
    #[serde(default = "full")]
    pub profile: String,
    #[serde(default)]
    pub project_checks: Vec<Vec<String>>,
    #[serde(default)]
    pub requirements: Vec<Requirement>,
    #[serde(default)]
    pub gate: bool,
    #[serde(default)]
    pub isolated_writers: bool,
    #[serde(default)]
    pub integration_queue: bool,
    /// Hidden independent checks; files are relative to `acceptance/`.
    pub acceptance: Vec<Check>,
    #[serde(default)]
    pub script: Vec<Step>,
    #[serde(default = "thirty")]
    pub minutes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requirement {
    pub text: String,
    #[serde(default = "yes")]
    pub required: bool,
    /// Verifier files are project-relative and visible to agents.
    #[serde(default)]
    pub check: Option<Check>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub argv: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub when: When,
    pub action: Action,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum When {
    AfterSeconds(u64),
    /// After the lead's reply to this turn number.
    AfterRootTurn(i64),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Follow { body: String },
    /// Revise requirement `requirement` (0-based index in the task).
    Revise { requirement: usize, text: String, #[serde(default)] check: Option<Check> },
    /// Close the engine and reopen the same journal (process restart).
    Restart,
}

/// One controlled variable set. Everything not listed stays at the default.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variant {
    pub id: String,
    #[serde(default)]
    pub lead: Option<String>,
    #[serde(default)]
    pub selections: Option<Value>,
    /// Overrides merged into the default efficiency settings.
    #[serde(default)]
    pub efficiency: Option<Value>,
    #[serde(default)]
    pub isolated_writers: Option<bool>,
    /// Execution policy mode of the trial project: `record` or `apply` (P2).
    #[serde(default)]
    pub policy: Option<String>,
}

pub struct LoadedSuite {
    pub suite: Suite,
    pub base: PathBuf,
    pub sha256: String,
}

fn safe_relative(path: &str) -> Result<()> {
    let p = Path::new(path);
    ensure!(
        !path.is_empty() && p.is_relative() && p.components().all(|c| matches!(c, std::path::Component::Normal(_))),
        "路径必须是相对路径且不含 ..：{path}"
    );
    Ok(())
}

pub fn load(path: &Path) -> Result<LoadedSuite> {
    let bytes = std::fs::read(path).with_context(|| format!("无法读取任务集 {}", path.display()))?;
    ensure!(bytes.len() <= 4 * 1024 * 1024, "任务集文件超过 4 MiB");
    let suite: Suite = serde_json::from_slice(&bytes).context("任务集 JSON 无效")?;
    ensure!(suite.schema_version == 1, "仅支持 schema_version 1");
    ensure!(!suite.tasks.is_empty() && suite.tasks.len() <= 64, "任务数必须在 1 到 64 之间");
    let mut ids = std::collections::HashSet::new();
    for task in &suite.tasks {
        ensure!(ids.insert(task.id.clone()), "任务编号重复：{}", task.id);
        safe_relative(&task.fixture)?;
        ensure!(!task.acceptance.is_empty(), "任务 {} 缺少验收检查", task.id);
        for check in task.acceptance.iter().chain(task.requirements.iter().filter_map(|r| r.check.as_ref())) {
            workspaces::validate_command(&check.argv)?;
            for file in &check.files {
                safe_relative(file)?;
            }
        }
        let commands = commands_for(task);
        ensure!(commands.len() <= 4, "任务 {} 的项目检查与需求检查合计超过 4 条", task.id);
    }
    Ok(LoadedSuite {
        sha256: crate::protocol::digest(&String::from_utf8_lossy(&bytes)),
        base: path.parent().map(Path::to_path_buf).unwrap_or_default(),
        suite,
    })
}

/// Project checks plus every requirement check (adoption requires them).
fn commands_for(task: &Task) -> Vec<Vec<String>> {
    let mut commands = task.project_checks.clone();
    for check in task.requirements.iter().filter_map(|r| r.check.as_ref()) {
        if !commands.contains(&check.argv) {
            commands.push(check.argv.clone());
        }
    }
    commands
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    if !from.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let target = to.join(&name);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn git(path: &Path, args: &[&str]) -> Result<()> {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=Super Agent Trial", "-c", "user.email=trial@localhost", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(path)
        .output()?;
    ensure!(out.status.success(), "git {} 失败：{}", args.join(" "), String::from_utf8_lossy(&out.stderr));
    Ok(())
}

/// A fresh committed project from the task fixture (plus an optional overlay).
pub fn materialize(suite: &LoadedSuite, task: &Task, into: &Path, overlay: Option<&str>) -> Result<()> {
    std::fs::create_dir_all(into)?;
    let fixture = suite.base.join(&task.fixture);
    ensure!(fixture.join("project").is_dir(), "任务 {} 缺少 project 目录", task.id);
    copy_tree(&fixture.join("project"), into)?;
    git(into, &["init", "-q"])?;
    git(into, &["config", "core.autocrlf", "false"])?;
    git(into, &["add", "--all"])?;
    git(into, &["commit", "-q", "--allow-empty", "-m", "Trial fixture"])?;
    if let Some(overlay) = overlay {
        copy_tree(&fixture.join(overlay), into)?;
    }
    Ok(())
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckResult {
    pub argv: Vec<String>,
    /// `pass`, `fail` or `unknown` (could not run).
    pub status: String,
    pub exit: Option<i32>,
    pub tail: String,
}

fn run_check(cwd: &Path, check: &Check) -> CheckResult {
    let outcome = workspaces::command_with_cancel(cwd, &check.argv, CHECK_TIMEOUT, None);
    match outcome {
        Ok(out) => {
            let tail = String::from_utf8_lossy(&out.output[out.output.len().saturating_sub(2000)..]).into_owned();
            CheckResult {
                argv: check.argv.clone(),
                status: if out.timed_out { "unknown" } else if out.ok { "pass" } else { "fail" }.into(),
                exit: out.exit,
                tail,
            }
        }
        Err(error) => CheckResult { argv: check.argv.clone(), status: "unknown".into(), exit: None, tail: format!("{error:#}") },
    }
}

/// Copy `project` (without `.git`) and the hidden acceptance files into a
/// scratch directory, then run every acceptance check there.
pub fn accept(suite: &LoadedSuite, task: &Task, project: &Path) -> Result<Vec<CheckResult>> {
    let scratch = tempfile::tempdir()?;
    copy_tree(project, scratch.path())?;
    let hidden = suite.base.join(&task.fixture).join("acceptance");
    let mut results = vec![];
    for check in &task.acceptance {
        for file in &check.files {
            let target = scratch.path().join(file);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(hidden.join(file), &target).with_context(|| format!("缺少验收文件 {file}"))?;
        }
        results.push(run_check(scratch.path(), check));
    }
    Ok(results)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TaskValidation {
    pub task: String,
    pub initial: Vec<CheckResult>,
    pub solution: Vec<CheckResult>,
    pub requirement_checks_on_solution: Vec<CheckResult>,
    pub valid: bool,
    pub reason: String,
}

/// Prove each task is meaningful before any paid run: the hidden acceptance
/// fails on the initial fixture and passes on the reference solution, and the
/// operator requirement checks pass on the solution.
pub fn validate(suite: &LoadedSuite) -> Result<Vec<TaskValidation>> {
    let mut out = vec![];
    for task in &suite.suite.tasks {
        let initial_dir = tempfile::tempdir()?;
        materialize(suite, task, initial_dir.path(), None)?;
        let initial = accept(suite, task, initial_dir.path())?;
        let solved_dir = tempfile::tempdir()?;
        materialize(suite, task, solved_dir.path(), Some("solution"))?;
        let solution = accept(suite, task, solved_dir.path())?;
        let revised = task.script.iter().filter_map(|step| match &step.action {
            Action::Revise { check: Some(check), .. } => Some(check),
            _ => None,
        });
        let requirement_checks_on_solution: Vec<_> = task
            .requirements
            .iter()
            .filter_map(|r| r.check.as_ref())
            .chain(revised)
            .map(|check| run_check(solved_dir.path(), check))
            .collect();
        let starts_failing = initial.iter().any(|r| r.status == "fail");
        let solvable = solution.iter().all(|r| r.status == "pass")
            && requirement_checks_on_solution.iter().all(|r| r.status == "pass");
        let reason = match (starts_failing, solvable) {
            (true, true) => "初始项目未通过验收，参考解全部通过".to_string(),
            (false, _) => "初始项目已经通过全部验收，任务没有区分度".to_string(),
            (_, false) => "参考解未通过验收或需求检查".to_string(),
        };
        out.push(TaskValidation {
            task: task.id.clone(),
            initial,
            solution,
            requirement_checks_on_solution,
            valid: starts_failing && solvable,
            reason,
        });
    }
    Ok(out)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Interactions {
    pub questions_answered: u64,
    pub approvals: u64,
    pub integration_confirmations: u64,
    pub scripted_follows: u64,
    pub scripted_revisions: u64,
    pub restarts: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Rework {
    pub check_bounces: u64,
    pub integration_bounces: u64,
    pub platform_retries: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RunRecord {
    pub schema_version: u32,
    pub suite: Value,
    pub task: String,
    pub category: String,
    pub variant: Variant,
    pub repetition: u32,
    pub root_state: String,
    pub timed_out: bool,
    pub elapsed_ms: u64,
    pub acceptance: Vec<CheckResult>,
    /// `success`, `failure` or `unknown` (§13.4: unknown is not success).
    pub verdict: String,
    pub delivery_status: Option<String>,
    pub ledger: Value,
    pub interactions: Interactions,
    pub rework: Rework,
    pub notes: Vec<String>,
}

fn efficiency(variant: &Variant) -> Result<sa_store::efficiency::Settings> {
    let mut value = serde_json::to_value(sa_store::efficiency::Settings::default())?;
    if let Some(Value::Object(overrides)) = &variant.efficiency {
        for (key, v) in overrides {
            ensure!(value.get(key).is_some(), "未知的效率设置：{key}");
            value[key] = v.clone();
        }
    }
    Ok(serde_json::from_value(value)?)
}

fn count(engine: &Engine, ticket: i64, kinds: &[&str]) -> Result<u64> {
    let list = kinds.iter().map(|k| format!("'{k}'")).collect::<Vec<_>>().join(",");
    engine.db().with(|q| {
        Ok(q.conn().query_row(
            &format!("SELECT count(*) FROM events WHERE ticket=? AND kind IN ({list})"),
            [ticket],
            |r| r.get::<_, i64>(0),
        )? as u64)
    })
}

fn root_state(engine: &Engine, ticket: i64) -> Result<(String, i64)> {
    let root = engine.root(ticket)?;
    Ok((
        root.get("state").and_then(Value::as_str).unwrap_or_default().to_string(),
        root.get("turn").and_then(Value::as_i64).unwrap_or(0),
    ))
}

fn busy(engine: &Engine, ticket: i64) -> Result<bool> {
    engine.db().with(|q| {
        Ok(q.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM jobs WHERE ticket=? AND state IN ('queued','running','waiting'))",
            [ticket],
            |r| r.get::<_, bool>(0),
        )?)
    })
}

/// Everything a trial needs to (re)open its engine.
pub struct Harness<'a> {
    pub adapters: &'a dyn Fn() -> Vec<Arc<dyn Adapter>>,
    pub options: &'a dyn Fn(sa_store::efficiency::Settings) -> EngineOptions,
    pub poll: Duration,
}

/// Run one task under one variant in `dir` (must be empty or absent).
pub async fn run(
    harness: &Harness<'_>,
    suite: &LoadedSuite,
    task: &Task,
    variant: &Variant,
    repetition: u32,
    dir: &Path,
) -> Result<RunRecord> {
    ensure!(!dir.exists() || std::fs::read_dir(dir)?.next().is_none(), "试验目录必须为空");
    let project = dir.join("project");
    let journal = dir.join("journal");
    materialize(suite, task, &project, None)?;
    let settings = efficiency(variant)?;
    let open = |settings: sa_store::efficiency::Settings| {
        Engine::open(&journal, (harness.adapters)(), (harness.options)(settings))
    };
    let mut engine = open(settings.clone())?;
    let commands = commands_for(task);
    if !commands.is_empty() {
        engine.set_project_checks(&project.to_string_lossy(), commands)?;
    }
    let ticket = engine
        .create(CreateParams {
            goal: task.goal.clone(),
            cwd: project.clone(),
            lead: variant.lead.clone().or_else(|| task.lead.clone()),
            permission_profile: Some(task.profile.clone()),
            selections: variant.selections.clone(),
            ..Default::default()
        })
        .await?;
    let mut requirement_ids = vec![];
    let mut contract = 0;
    for requirement in &task.requirements {
        let summary = engine.save_requirement(ticket, contract, None, requirement.text.clone(), None, None, None)?;
        contract = summary.contract_revision;
        let id = summary.requirements.last().context("需求未保存")?.id;
        requirement_ids.push(id);
        if !requirement.required {
            let summary = engine.set_requirement_disposition(ticket, contract, id, false, false, "试验任务定义为可选".into())?;
            contract = summary.contract_revision;
        }
        if let Some(check) = &requirement.check {
            let summary = engine.adopt_delivery_check_with_files(ticket, contract, id, check.argv.clone(), check.files.clone())?;
            contract = summary.contract_revision;
        }
    }
    if task.gate {
        engine.set_delivery_gate(ticket, 0, true)?;
    }
    if variant.isolated_writers.unwrap_or(task.isolated_writers) {
        engine.set_isolated_writers(ticket, true)?;
    }
    if task.integration_queue {
        engine.set_integration_queue(ticket, true)?;
    }
    if let Some(mode) = &variant.policy {
        engine.set_execution_policy(&project.to_string_lossy(), mode)?;
    }
    engine.start()?;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(task.minutes * 60);
    let mut interactions = Interactions::default();
    let mut notes = vec![];
    let mut pending: Vec<Step> = task.script.clone();
    let mut timed_out = false;
    loop {
        tokio::time::sleep(harness.poll).await;
        if let Some(fault) = engine.fault() {
            notes.push(format!("调度器故障：{fault}"));
            break;
        }
        let (state, turn) = root_state(&engine, ticket)?;
        let mut due = vec![];
        pending.retain(|step| {
            let ready = match step.when {
                When::AfterSeconds(seconds) => started.elapsed() >= Duration::from_secs(seconds),
                When::AfterRootTurn(n) => turn > n || (turn == n && !matches!(state.as_str(), "queued" | "running")),
            };
            if ready {
                due.push(step.clone());
            }
            !ready
        });
        for step in due {
            match step.action {
                Action::Follow { body } => {
                    engine.follow(ticket, &body, true)?;
                    interactions.scripted_follows += 1;
                }
                Action::Revise { requirement, text, check } => {
                    let id = *requirement_ids.get(requirement).context("脚本引用的需求不存在")?;
                    let current = engine.delivery_summary(ticket)?.contract_revision;
                    let mut summary = engine.save_requirement(ticket, current, Some(id), text, None, None, None)?;
                    if let Some(check) = check {
                        let mut commands = engine.project_checks(&project.to_string_lossy())?;
                        if !commands.contains(&check.argv) {
                            commands.push(check.argv.clone());
                            engine.set_project_checks(&project.to_string_lossy(), commands)?;
                        }
                        summary = engine.adopt_delivery_check_with_files(
                            ticket, summary.contract_revision, id, check.argv, check.files)?;
                    }
                    let _ = summary;
                    interactions.scripted_revisions += 1;
                }
                Action::Restart => {
                    engine.close().await;
                    engine = open(settings.clone())?;
                    engine.start()?;
                    interactions.restarts += 1;
                }
            }
        }
        for question in engine.questions(ticket)? {
            let id = question["id"].as_i64().unwrap_or_default();
            let answer = question["fallback"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| question["options"][0].as_str().map(str::to_string))
                .unwrap_or_else(|| "按你的判断继续".into());
            engine.answer(id, &answer)?;
            interactions.questions_answered += 1;
        }
        let approvals: Vec<i64> = engine.db().with(|q| {
            let mut stmt = q.conn().prepare("SELECT id FROM approvals WHERE ticket=? AND decision='pending'")?;
            let ids = stmt.query_map([ticket], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            Ok(ids)
        })?;
        for id in approvals {
            engine.decide(id, true)?;
            interactions.approvals += 1;
        }
        if task.integration_queue {
            for item in engine.integration_queue(ticket)? {
                if item.state == "awaiting_confirmation"
                    && let Some(token) = item.validation["confirmation_token"].as_str()
                {
                    if engine.confirm_integration(item.job, token).await.is_ok() {
                        interactions.integration_confirmations += 1;
                    } else {
                        notes.push(format!("候选 #{} 确认失败", item.job));
                    }
                }
            }
        }
        let awaiting = task.integration_queue
            && engine
                .integration_queue(ticket)?
                .iter()
                .any(|i| matches!(i.state.as_str(), "queued" | "validating" | "awaiting_confirmation" | "publishing"));
        if TERMINAL.contains(&state.as_str()) && pending.is_empty() && !busy(&engine, ticket)? && !awaiting {
            break;
        }
        if Instant::now() > deadline {
            timed_out = true;
            engine.cancel(ticket)?;
            notes.push(format!("超过 {} 分钟上限，已停止工单", task.minutes));
            break;
        }
    }
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let (root_state, _) = root_state(&engine, ticket)?;
    let delivery = engine.delivery_summary(ticket).ok();
    let delivery_status = delivery.as_ref().map(|d| d.gate.status.clone());
    let ledger = engine.db().with(|q| sa_store::ledger::ticket_ledger(q.conn(), ticket))?;
    let rework = Rework {
        check_bounces: count(&engine, ticket, &["check.bounced"])?,
        integration_bounces: count(&engine, ticket, &["integration.bounced"])?,
        platform_retries: count(&engine, ticket, &["turn.retry_scheduled", "route.rotated", "auto.retry"])?,
    };
    engine.close().await;
    let acceptance = accept(suite, task, &project)?;
    let verdict = if timed_out || acceptance.iter().any(|r| r.status == "unknown") {
        "unknown"
    } else if acceptance.iter().all(|r| r.status == "pass")
        && (!task.gate || delivery_status.as_deref() == Some("ready"))
    {
        "success"
    } else {
        "failure"
    };
    Ok(RunRecord {
        schema_version: RECORD_VERSION,
        suite: json!({"id": suite.suite.id, "version": suite.suite.version, "sha256": suite.sha256}),
        task: task.id.clone(),
        category: task.category.clone(),
        variant: variant.clone(),
        repetition,
        root_state,
        timed_out,
        elapsed_ms,
        acceptance,
        verdict: verdict.into(),
        delivery_status,
        ledger: json!({
            "totals": ledger.totals, "partial": ledger.partial, "coverage": ledger.coverage,
            "buckets": ledger.buckets, "gaps": ledger.gaps.len(),
        }),
        interactions,
        rework,
        notes,
    })
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Group {
    pub task: String,
    pub category: String,
    pub variant: String,
    pub runs: u64,
    pub successes: u64,
    pub failures: u64,
    pub unknown: u64,
    /// Successes over all runs; unknown runs count in the denominator.
    pub success_rate: Option<f64>,
    pub elapsed_p50_ms: Option<u64>,
    pub elapsed_p90_ms: Option<u64>,
    /// Reported total tokens over every run, including failures.
    pub reported_tokens: Option<u64>,
    /// True when any run's usage coverage was incomplete.
    pub tokens_partial: bool,
    /// Total spend per success; undefined with zero successes or partial usage.
    pub tokens_per_success: Option<f64>,
    pub human_interactions: u64,
    pub rework: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Comparison {
    pub task: String,
    pub baseline: String,
    pub variant: String,
    pub success_rate_delta: Option<f64>,
    pub elapsed_p50_delta_ms: Option<i64>,
    /// Only when neither side has partial usage.
    pub tokens_per_success_delta: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TrialReport {
    pub schema_version: u32,
    pub suite: Value,
    pub groups: Vec<Group>,
    pub comparisons: Vec<Comparison>,
    pub notes: Vec<String>,
}

fn percentile(sorted: &[u64], p: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    Some(sorted[rank - 1])
}

/// Aggregate run records; the first variant seen per task is the baseline.
pub fn aggregate(records: &[RunRecord]) -> Result<TrialReport> {
    let Some(first) = records.first() else { bail!("没有试验记录") };
    ensure!(records.iter().all(|r| r.suite == first.suite), "记录来自不同版本的任务集，不能合并");
    let mut groups: BTreeMap<(String, String), (Group, Vec<u64>)> = BTreeMap::new();
    let mut baselines: BTreeMap<String, String> = BTreeMap::new();
    for record in records {
        baselines.entry(record.task.clone()).or_insert_with(|| record.variant.id.clone());
        let (group, times) = groups
            .entry((record.task.clone(), record.variant.id.clone()))
            .or_insert_with(|| {
                (
                    Group {
                        task: record.task.clone(),
                        category: record.category.clone(),
                        variant: record.variant.id.clone(),
                        reported_tokens: Some(0),
                        ..Default::default()
                    },
                    vec![],
                )
            });
        group.runs += 1;
        match record.verdict.as_str() {
            "success" => group.successes += 1,
            "failure" => group.failures += 1,
            _ => group.unknown += 1,
        }
        times.push(record.elapsed_ms);
        let tokens = record.ledger["totals"]["total_tokens"].as_u64();
        group.reported_tokens = group.reported_tokens.zip(tokens).and_then(|(a, b)| a.checked_add(b));
        group.tokens_partial |= record.ledger["partial"] != false || tokens.is_none();
        let i = &record.interactions;
        group.human_interactions += i.questions_answered + i.approvals + i.integration_confirmations;
        group.rework += record.rework.check_bounces + record.rework.integration_bounces;
    }
    let mut out = vec![];
    for (_, (mut group, mut times)) in groups {
        times.sort_unstable();
        group.success_rate = (group.runs > 0).then(|| group.successes as f64 / group.runs as f64);
        group.elapsed_p50_ms = percentile(&times, 0.5);
        group.elapsed_p90_ms = percentile(&times, 0.9);
        group.tokens_per_success = (!group.tokens_partial && group.successes > 0)
            .then(|| group.reported_tokens.map(|t| t as f64 / group.successes as f64))
            .flatten();
        out.push(group);
    }
    let mut comparisons = vec![];
    for group in &out {
        let Some(baseline) = baselines.get(&group.task) else { continue };
        if &group.variant == baseline {
            continue;
        }
        let Some(base) = out.iter().find(|g| g.task == group.task && &g.variant == baseline) else { continue };
        comparisons.push(Comparison {
            task: group.task.clone(),
            baseline: baseline.clone(),
            variant: group.variant.clone(),
            success_rate_delta: group.success_rate.zip(base.success_rate).map(|(a, b)| a - b),
            elapsed_p50_delta_ms: group.elapsed_p50_ms.zip(base.elapsed_p50_ms).map(|(a, b)| a as i64 - b as i64),
            tokens_per_success_delta: group.tokens_per_success.zip(base.tokens_per_success).map(|(a, b)| a - b),
        });
    }
    Ok(TrialReport {
        schema_version: 1,
        suite: first.suite.clone(),
        groups: out,
        comparisons,
        notes: vec![
            "样本少时只用于探索，不能据此宣称统计显著或覆盖所有真实项目".into(),
            "未知结果不计为成功；用量覆盖不完整时不计算单位成功支出".into(),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(task: &str, variant: &str, verdict: &str, ms: u64, tokens: Option<u64>, partial: bool) -> RunRecord {
        RunRecord {
            suite: json!({"id": "s", "version": "1", "sha256": "x"}),
            task: task.into(),
            category: "small_change".into(),
            variant: Variant { id: variant.into(), ..Default::default() },
            verdict: verdict.into(),
            elapsed_ms: ms,
            ledger: json!({"totals": {"total_tokens": tokens}, "partial": partial}),
            ..Default::default()
        }
    }

    #[test]
    fn aggregate_keeps_unknown_out_of_success_and_spend_needs_complete_usage() {
        let records = vec![
            record("t", "base", "success", 1000, Some(100), false),
            record("t", "base", "unknown", 3000, Some(50), false),
            record("t", "base", "failure", 2000, Some(30), false),
            record("t", "new", "success", 500, Some(60), false),
            record("t", "new", "success", 700, Some(40), true),
        ];
        let report = aggregate(&records).unwrap();
        let base = report.groups.iter().find(|g| g.variant == "base").unwrap();
        assert_eq!((base.runs, base.successes, base.unknown, base.failures), (3, 1, 1, 1));
        assert_eq!(base.success_rate, Some(1.0 / 3.0));
        assert_eq!(base.elapsed_p50_ms, Some(2000));
        // All attempts count, including failed and unknown runs.
        assert_eq!(base.tokens_per_success, Some(180.0));
        let new = report.groups.iter().find(|g| g.variant == "new").unwrap();
        assert!(new.tokens_partial);
        assert_eq!(new.tokens_per_success, None);
        let comparison = &report.comparisons[0];
        assert_eq!((comparison.baseline.as_str(), comparison.variant.as_str()), ("base", "new"));
        assert_eq!(comparison.tokens_per_success_delta, None);
        assert!(comparison.success_rate_delta.unwrap() > 0.6);
        let mixed = vec![record("t", "a", "success", 1, None, false), {
            let mut other = record("t", "a", "success", 1, None, false);
            other.suite = json!({"id": "s", "version": "2", "sha256": "y"});
            other
        }];
        assert!(aggregate(&mixed).is_err());
    }

    #[test]
    fn suite_paths_must_stay_inside_the_suite() {
        assert!(safe_relative("fixtures/a").is_ok());
        assert!(safe_relative("../a").is_err());
        assert!(safe_relative("C:/a").is_err() || cfg!(not(windows)));
        assert!(safe_relative("").is_err());
    }
}
