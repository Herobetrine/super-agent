//! Operator-enabled candidate preparation. Publishing always needs a separate
//! operator confirmation against the exact validated trees and check commands.
use crate::db::{Q, int_of, str_of};
use crate::operations::{Scope, kind};
use crate::protocol::digest;
use crate::workspaces::{self, Workspace};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

// Bump whenever candidate validation or publication semantics change.
pub(crate) const VERSION: &str = "integration-queue/1";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntegrationItem {
    pub job: i64,
    pub ticket: i64,
    pub turn: i64,
    pub state: String,
    /// Number of automatic repair requests already sent, capped at two.
    pub attempts: i64,
    pub detail: String,
    pub validation: Value,
    pub version: String,
    pub commands: Vec<Vec<String>>,
}

/// Safe for an observer Q on a journal predating the queue tables.
pub fn enabled(q: &Q, ticket: i64) -> Result<bool> {
    if !q.exists(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='integration_policy'",
        &[],
    )? {
        return Ok(false);
    }
    q.exists(
        "SELECT 1 FROM integration_policy WHERE ticket=? AND enabled=1",
        &[&ticket],
    )
}

pub fn list(q: &Q, ticket: i64) -> Result<Vec<IntegrationItem>> {
    if !q.exists(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='integration_queue'",
        &[],
    )? {
        return Ok(vec![]);
    }
    q.all(
        "SELECT * FROM integration_queue WHERE ticket=? ORDER BY job",
        &[&ticket],
    )?
    .iter()
    .map(|r| {
        Ok(IntegrationItem {
            job: int_of(r, "job"),
            ticket: int_of(r, "ticket"),
            turn: int_of(r, "turn"),
            state: str_of(r, "state").into(),
            attempts: int_of(r, "attempts"),
            detail: str_of(r, "detail").into(),
            version: str_of(r, "version").into(),
            validation: serde_json::from_str(str_of(r, "validation"))?,
            commands: serde_json::from_str(str_of(r, "commands"))?,
        })
    })
    .collect()
}

pub(crate) fn get(q: &Q, job: i64) -> Result<IntegrationItem> {
    let row = q
        .one("SELECT ticket FROM integration_queue WHERE job=?", &[&job])?
        .ok_or_else(|| anyhow::anyhow!("合入队列项不存在"))?;
    list(q, int_of(&row, "ticket"))?
        .into_iter()
        .find(|item| item.job == job)
        .ok_or_else(|| anyhow::anyhow!("合入队列项不存在"))
}

/// Reconcile journal state before admission. Never silently reuse an obsolete
/// candidate, including a continued writer or changed operator checks.
pub(crate) fn reconcile(q: &Q) -> Result<()> {
    q.run("UPDATE integration_queue SET state='stale',detail='任务版本已改变，需重新验证'
      WHERE state='awaiting_confirmation' AND EXISTS(SELECT 1 FROM jobs j WHERE j.id=integration_queue.job
        AND (j.turn!=integration_queue.turn OR j.state!='done'))", &[])?;
    for row in q.all(
        "SELECT DISTINCT ticket FROM integration_queue WHERE state='awaiting_confirmation'",
        &[],
    )? {
        let ticket = int_of(&row, "ticket");
        let commands = commands(q, ticket)?;
        q.run("UPDATE integration_queue SET state='stale',detail='项目检查已改变，需重新验证' WHERE ticket=? AND state='awaiting_confirmation' AND commands!=?",&[&ticket,&json!(commands).to_string()])?;
    }
    for row in q.all(
        "SELECT j.id,j.ticket,j.turn,t.cwd FROM jobs j JOIN job_workspaces w ON w.job=j.id
      JOIN tickets t ON t.id=j.ticket JOIN integration_policy p ON p.ticket=j.ticket AND p.enabled=1
      LEFT JOIN integration_queue i ON i.job=j.id
      WHERE j.state='done' AND j.writing=1 AND w.state!='merged'
        AND (i.job IS NULL OR (i.turn!=j.turn AND i.state!='merged'))",
        &[],
    )? {
        let commands = crate::checks::configured(q, str_of(&row, "cwd"))?;
        q.run("INSERT INTO integration_queue(job,ticket,turn,state,version,commands)
          VALUES(?,?,?,'queued',?,?) ON CONFLICT(job) DO UPDATE SET turn=excluded.turn,state='queued',
          version=excluded.version,commands=excluded.commands,validation='{}',detail='等待验证新任务版本'",
          &[&int_of(&row,"id"),&int_of(&row,"ticket"),&int_of(&row,"turn"),&VERSION,&json!(commands).to_string()])?;
    }
    Ok(())
}

/// Only process-lifetime operations can hold a validating/publishing lease.
/// A restart revalidates candidates and reconciles a publication that committed
/// its Git fast-forward immediately before the journal receipt was persisted.
pub(crate) fn recover(q: &Q) -> Result<()> {
    q.run(
        "UPDATE integration_queue SET state='queued',detail='上次验证中断，将重新验证'
      WHERE state='validating'",
        &[],
    )?;
    q.run(
        "UPDATE integration_queue SET state='stale',detail='验证程序版本已变化，需重新验证'
      WHERE state='awaiting_confirmation' AND version!=?",
        &[&VERSION],
    )?;
    // Publishing is deliberately left for explicit confirmation to reconcile.
    Ok(())
}

pub(crate) fn commands(q: &Q, ticket: i64) -> Result<Vec<Vec<String>>> {
    let row = q
        .one("SELECT cwd FROM tickets WHERE id=?", &[&ticket])?
        .ok_or_else(|| anyhow::anyhow!("工单不存在"))?;
    let project = crate::services::realpath(Path::new(str_of(&row, "cwd")))?;
    crate::checks::configured(q, &project.to_string_lossy())
}

pub(crate) fn current(q: &Q, item: &IntegrationItem) -> Result<()> {
    ensure!(enabled(q, item.ticket)?, "该工单未启用自动合入队列");
    ensure!(item.version == VERSION, "验证程序版本已变化，请重新验证");
    let row = q
        .one(
            "SELECT state,turn FROM jobs WHERE id=? AND ticket=?",
            &[&item.job, &item.ticket],
        )?
        .ok_or_else(|| anyhow::anyhow!("任务不存在"))?;
    ensure!(
        str_of(&row, "state") == "done" && int_of(&row, "turn") == item.turn,
        "任务已续派或改变，请重新验证"
    );
    ensure!(
        commands(q, item.ticket)? == item.commands && !item.commands.is_empty(),
        "项目检查已改变，请重新验证"
    );
    Ok(())
}

pub(crate) fn checkpoint(w: &Workspace, cancel: &dyn Fn() -> bool) -> Result<Workspace> {
    checkpoint_with(w, cancel, &Scope::detached())
}

/// The checkpoint commit runs under a `git.checkpoint` receipt.
fn checkpoint_with(w: &Workspace, cancel: &dyn Fn() -> bool, ops: &Scope) -> Result<Workspace> {
    ensure!(!cancel(), "合入验证已取消");
    workspaces::verify(w)?;
    let path = Path::new(&w.path);
    workspaces::git(path, &["add", "--all", "--", "."])?;
    if !workspaces::git(path, &["diff", "--cached", "--name-only"])?.is_empty() {
        ops.begin(kind::CHECKPOINT, json!({"job":w.job,"path":w.path,"branch":w.branch,
            "pre_head":workspaces::head(path)?}))?;
        workspaces::git(
            path,
            &["commit", "-m", "Checkpoint isolated Super Agent task"],
        )?;
        ops.effect(kind::CHECKPOINT)?;
    }
    workspaces::clean(path)?;
    let mut ready = w.clone();
    ready.head = workspaces::head(path)?;
    ready.state = "ready".into();
    Ok(ready)
}

fn run_checks(
    path: &Path,
    commands: &[Vec<String>],
    cancel: &dyn Fn() -> bool,
) -> Result<(bool, Value)> {
    let baseline = workspaces::head(path)?;
    let mut results = vec![];
    for argv in commands {
        workspaces::validate_command(argv)?;
        let output =
            workspaces::command_with_cancel(path, argv, Duration::from_secs(600), Some(cancel))?;
        ensure!(!output.cancelled, "合入验证已取消");
        let unchanged = workspaces::head(path)? == baseline && workspaces::clean(path).is_ok();
        let ok = output.ok && !output.timed_out && unchanged;
        results.push(
            json!({"argv":argv,"ok":ok,"exit":output.exit,"timed_out":output.timed_out,
            "unchanged":unchanged,"output_tail":String::from_utf8_lossy(&output.output)}),
        );
        if !ok {
            return Ok((false, json!(results)));
        }
    }
    Ok((true, json!(results)))
}

/// No command in preparation updates the source branch or source working tree.
pub(crate) fn prepare(
    w: &Workspace,
    data: &Path,
    commands: &[Vec<String>],
    cancel: &dyn Fn() -> bool,
) -> Result<Workspace> {
    ensure!(!commands.is_empty(), "必须先配置项目完成检查");
    let mut ready = checkpoint(w, cancel)?;
    let source = Path::new(&w.source);
    workspaces::clean(source)?;
    let baseline = workspaces::head(source)?;
    let (passed, output) = run_checks(Path::new(&w.path), commands, cancel)?;
    ready.validation = json!({"identity":w.validation["identity"],"version":VERSION,"head":ready.head,
        "commands":commands,"baseline":baseline,"workspace_checks":output,"passed":false});
    if let Some(heads) = w.validation.get("prerequisite_heads") {
        ready.validation["prerequisite_heads"] = heads.clone();
    }
    if !passed {
        ready.state = "failed".into();
        ready.detail = "隔离任务验证未通过".into();
        return Ok(ready);
    }
    let home = data.join("integrations");
    std::fs::create_dir_all(&home)?;
    let candidate = tempfile::Builder::new()
        .prefix(&format!("queue-{}-", w.job))
        .tempdir_in(home)?
        .keep();
    let candidate_text = candidate.to_string_lossy().to_string();
    workspaces::git(
        source,
        &["worktree", "add", "--detach", &candidate_text, &baseline],
    )?;
    ready.validation["candidate"] = json!(candidate_text);
    let merge = workspaces::command_with_cancel(
        &candidate,
        &workspaces::git_argv(&["merge", "--no-ff", "--no-edit", &ready.head]),
        Duration::from_secs(60),
        Some(cancel),
    )?;
    ensure!(!merge.cancelled, "合入验证已取消");
    ready.validation["merge_output_tail"] = json!(String::from_utf8_lossy(&merge.output));
    if !merge.ok {
        ready.validation["conflicts"] = json!(workspaces::git(
            &candidate,
            &["diff", "--name-only", "--diff-filter=U"]
        )?);
        ready.state = "conflict".into();
        ready.detail = "合并候选存在冲突，原项目未改变".into();
        return Ok(ready);
    }
    let candidate_head = workspaces::head(&candidate)?;
    let (passed, output) = run_checks(&candidate, commands, cancel)?;
    ready.validation["candidate_head"] = json!(candidate_head);
    ready.validation["candidate_checks"] = output;
    ready.validation["passed"] = json!(passed);
    // The unique retained candidate directory supplies a per-validation nonce,
    // including when Git returns the same commit on a repeated merge.
    ready.validation["confirmation_token"] = json!(digest(
        &json!({
            "nonce":candidate_text,"validation":ready.validation
        })
        .to_string()
    ));
    ready.state = if passed {
        "awaiting_confirmation"
    } else {
        "failed"
    }
    .into();
    ready.detail = if passed {
        "候选验证通过，等待操作者确认合入"
    } else {
        "合并候选未通过验证"
    }
    .into();
    Ok(ready)
}

/// Confirmation checks every input again. Replaying an interrupted publication
/// only recognizes the exact previously validated commit, never merges twice.
pub(crate) fn publish(
    w: &Workspace,
    validation: &Value,
    cancel: &dyn Fn() -> bool,
    ops: &Scope,
) -> Result<Workspace> {
    ensure!(!cancel(), "合入操作已取消");
    ensure!(
        validation["version"] == VERSION && validation["passed"] == true,
        "缺少当前版本的通过回执"
    );
    workspaces::verify(w)?;
    workspaces::clean(Path::new(&w.path))?;
    ensure!(
        workspaces::head(Path::new(&w.path))? == validation["head"],
        "隔离任务版本已改变，请重新验证"
    );
    let candidate = validation["candidate"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("候选路径缺失"))?;
    let candidate_head = validation["candidate_head"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("候选提交缺失"))?;
    workspaces::clean(Path::new(candidate))?;
    ensure!(
        workspaces::head(Path::new(candidate))? == candidate_head,
        "候选已变化，请重新验证"
    );
    let source = Path::new(&w.source);
    workspaces::clean(source)?;
    let source_head = workspaces::head(source)?;
    if source_head != candidate_head {
        ensure!(
            source_head == validation["baseline"],
            "原项目有新提交，请重新验证"
        );
        ensure!(!cancel(), "合入操作已取消");
        ops.begin(kind::PUBLISH, json!({"job":w.job,"source":w.source,
            "source_ref":w.validation["identity"]["source_ref"],"baseline":source_head,"target":candidate_head}))?;
        workspaces::git(
            source,
            &[
                "merge",
                "--no-overwrite-ignore",
                "--ff-only",
                candidate_head,
            ],
        )?;
        ops.effect(kind::PUBLISH)?;
    }
    let mut merged = w.clone();
    merged.state = "merged".into();
    merged.validation = validation.clone();
    merged.validation["integrated_head"] = json!(candidate_head);
    merged.detail = "操作者已确认，候选已本地合入".into();
    Ok(merged)
}

/// Construct a dependent writer from all completed prerequisite branches.
/// The dependency vector stores both task turn and exact head, so a continued
/// prerequisite can never reuse the previous version unnoticed.
pub(crate) fn provision_dependencies(
    w: &Workspace,
    data: &Path,
    prerequisites: &[(Workspace, i64)],
    ops: &Scope,
) -> Result<Workspace> {
    if prerequisites.is_empty() {
        return workspaces::provision_with(w, data, ops);
    }
    let prerequisites = prerequisites
        .iter()
        .map(|(p, turn)| Ok((checkpoint_with(p, &|| false, ops)?, *turn)))
        .collect::<Result<Vec<_>>>()?;
    let heads: Vec<Value> = prerequisites
        .iter()
        .map(|(p, turn)| json!({"job":p.job,"turn":turn,"head":p.head}))
        .collect();
    if w.validation.get("prerequisite_heads").is_some() {
        ensure!(
            w.validation["prerequisite_heads"] == json!(heads),
            "前置分支已续派或变化，请人工核对当前写任务"
        );
        return workspaces::provision_with(w, data, ops);
    }
    ensure!(
        w.state == "planned",
        "已有工作区缺少前置分支快照，不能自动改变基线"
    );
    let mut ready = workspaces::provision_with(w, data, ops)?;
    for (prerequisite, _) in &prerequisites {
        workspaces::verify(prerequisite)?;
        workspaces::clean(Path::new(&prerequisite.path))?;
        ensure!(
            workspaces::head(Path::new(&prerequisite.path))? == prerequisite.head,
            "前置工作区发生变化"
        );
        ops.begin(kind::DEPENDENCY_MERGE, json!({"job":w.job,"path":ready.path,"branch":ready.branch,
            "pre_head":workspaces::head(Path::new(&ready.path))?,"merge_head":prerequisite.head,"prerequisite":prerequisite.job}))?;
        workspaces::git(
            Path::new(&ready.path),
            &["merge", "--no-ff", "--no-edit", &prerequisite.head],
        )?;
        ops.effect(kind::DEPENDENCY_MERGE)?;
    }
    ready.base = workspaces::head(Path::new(&ready.path))?;
    ready.head = ready.base.clone();
    ready.validation["prerequisite_heads"] = json!(heads);
    ready.validation["prerequisite_sha"] = json!(digest(&json!(heads).to_string()));
    Ok(ready)
}
