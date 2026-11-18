//! Platform delivery evidence after a writing job's accepted completion
//! claim. It reuses operator-adopted criteria only: no requirement, command
//! or verifier is created here. With the gate off the result is recorded and
//! reported; with the gate on a failed required criterion goes back to the job
//! through the completion-check bounce budget.
use super::{contract_revision, ticket_project};
use crate::{Engine, db::Q, workspaces};
use anyhow::{Result, bail, ensure};
use sa_store::delivery::{self, DeliveryFile, RequirementView, SnapshotOrigin};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

/// Bounded reason text sent back to the lead.
const REASON_CHARS: usize = 240;

pub(crate) struct AutoRequest {
    pub ticket: i64,
    pub job: i64,
    pub turn: i64,
    /// The job's effective work directory (its isolated worktree if any).
    pub cwd: String,
}

pub(crate) struct AutoOutcome {
    pub payload: Value,
    /// Same shape as a failed completion check: argv, exit code, output tail.
    pub failure: Option<(Vec<String>, Option<i32>, String)>,
}

struct Plan {
    contract: i64,
    root: Result<(PathBuf, bool, String), String>,
    latest: Option<i64>,
    /// Filled from `latest` outside the journal lock.
    previous: Option<(i64, i64, Vec<DeliveryFile>)>,
    pinned: Vec<String>,
    behavior: Vec<i64>,
    gate: (bool, i64),
}

fn plan(q: &Q, request: &AutoRequest) -> Result<Option<Plan>> {
    let project = ticket_project(q, request.ticket)?;
    let requirements = delivery::requirements(q.conn(), request.ticket)?;
    let active: Vec<&RequirementView> = requirements
        .iter()
        .filter(|item| !item.withdrawn && (item.behavior.is_some() || item.path.is_some()))
        .collect();
    if active.is_empty() {
        return Ok(None);
    }
    let gate = delivery::gate_metadata(q.conn(), request.ticket)?;
    let project_scope = workspaces::scope(&project);
    let cwd_scope = workspaces::scope(&request.cwd);
    let root = match workspaces::get(q, request.job)? {
        Some(w) if !w.path.is_empty() && workspaces::scope(&w.path) == cwd_scope => {
            if workspaces::scope(&w.source) == project_scope {
                Ok((PathBuf::from(&w.path), true, w.base))
            } else {
                Err("隔离工作区的来源不是工单项目目录，无法按项目相对路径捕获".to_string())
            }
        }
        _ if workspaces::overlaps(&cwd_scope, &project_scope) => {
            Ok((PathBuf::from(&project), false, "HEAD".to_string()))
        }
        _ => Err("任务目录不在工单项目内，无法捕获交付输入".to_string()),
    };
    let latest = q.conn().query_row(
        "SELECT MAX(id) FROM delivery_snapshots WHERE ticket=?",
        [request.ticket],
        |row| row.get::<_, Option<i64>>(0),
    )?;
    let mut pinned = vec![];
    for item in &active {
        pinned.extend(item.path.iter().cloned());
        if let Some(check) = &item.behavior {
            pinned.extend(check.verifier_files.iter().map(|file| file.path.clone()));
        }
    }
    let behavior = active
        .iter()
        .filter(|item| {
            item.behavior
                .as_ref()
                .is_some_and(|check| check.id > 0 && check.requirement_revision == item.revision)
        })
        .map(|item| item.id)
        .collect();
    Ok(Some(Plan {
        contract: contract_revision(q, request.ticket)?,
        root,
        latest,
        previous: None,
        pinned,
        behavior,
        gate: (gate.enabled && gate.policy_revision > 0, gate.policy_revision),
    }))
}

/// Existing regular files only, in listing order, without Git metadata and
/// without case-insensitive duplicates. Returns paths and their total size.
fn existing(root: &Path, paths: impl IntoIterator<Item = String>) -> (Vec<String>, u64, bool) {
    let mut seen = HashSet::new();
    let mut kept = vec![];
    let mut total = 0;
    let mut oversized = false;
    for path in paths {
        let Ok(path) = delivery::normalize_path(&path) else { continue };
        if path.split('/').any(|part| part.eq_ignore_ascii_case(".git"))
            || !seen.insert(path.to_ascii_lowercase())
        {
            continue;
        }
        let Ok(source) = delivery::project_file(root, &path) else { continue };
        let Ok(meta) = std::fs::symlink_metadata(&source) else { continue };
        if !meta.is_file() {
            continue;
        }
        oversized |= meta.len() > delivery::MAX_FILE_BYTES;
        total += meta.len();
        kept.push(path);
    }
    (kept, total, oversized)
}

fn fits(paths: &[String], total: u64, oversized: bool) -> bool {
    !paths.is_empty()
        && paths.len() <= delivery::MAX_FILES
        && total <= delivery::MAX_TOTAL_BYTES
        && !oversized
}

/// All project files when they fit the snapshot bounds, otherwise the
/// previous snapshot's inputs plus this job's changes and pinned verifiers.
fn select(
    root: &Path,
    base: &str,
    plan: &Plan,
    cancel: &dyn Fn() -> bool,
) -> Result<(Vec<String>, &'static str)> {
    let Some(listed) = workspaces::git_paths(
        root,
        &["ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        cancel,
    )?
    else {
        bail!("无法列出任务目录的 Git 文件，未自动捕获");
    };
    let (all, total, oversized) = existing(root, listed);
    if fits(&all, total, oversized) {
        return Ok((all, "project_files"));
    }
    let mut candidates: Vec<String> = plan.pinned.clone();
    if let Some((_, _, files)) = &plan.previous {
        candidates.extend(files.iter().map(|file| file.path.clone()));
    }
    for args in [
        &["diff", "--name-only", "--relative", "-z", base][..],
        &["ls-files", "-z", "--others", "--exclude-standard"][..],
    ] {
        let Some(changed) = workspaces::git_paths(root, args, cancel)? else {
            bail!("无法列出任务改动的文件，未自动捕获");
        };
        candidates.extend(changed);
    }
    let (some, total, oversized) = existing(root, candidates);
    ensure!(
        fits(&some, total, oversized),
        "项目文件和本次改动均超出快照上限（{} 个文件、单文件 4 MiB、合计 64 MiB），请操作员显式捕获",
        delivery::MAX_FILES
    );
    Ok((some, "previous_and_changed"))
}

fn digests(root: &Path, paths: &[String]) -> Result<BTreeMap<String, String>> {
    paths
        .iter()
        .map(|path| {
            let bytes = delivery::read_stable_file(&delivery::project_file(root, path)?)?;
            Ok((path.clone(), delivery::sha256(&bytes)))
        })
        .collect()
}

fn short(text: &str) -> String {
    let mut chars = text.chars();
    let mut kept: String = chars.by_ref().take(REASON_CHARS).collect();
    if chars.next().is_some() {
        kept.push('…');
    }
    kept
}

fn unknown(mut payload: Value, reason: impl std::fmt::Display) -> AutoOutcome {
    payload["status"] = json!("unknown");
    payload["reason"] = json!(short(&reason.to_string()));
    AutoOutcome { payload, failure: None }
}

fn last_terminal(q: &Q, ticket: i64, snapshot: i64, requirement: i64) -> Result<bool> {
    q.exists(
        "SELECT 1 FROM delivery_behavior_attempts a JOIN delivery_behavior_results r ON r.attempt_id=a.id
          WHERE a.id=(SELECT MAX(id) FROM delivery_behavior_attempts WHERE ticket=? AND snapshot_id=? AND requirement=?)
            AND r.status IN ('pass','fail')",
        &[&ticket, &snapshot, &requirement],
    )
}

impl Engine {
    /// `Ok(None)` when the ticket has no active operator criterion. Input and
    /// capture problems become an `unknown` record instead of a turn failure.
    pub(crate) fn auto_delivery(
        &self,
        request: &AutoRequest,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Option<AutoOutcome>> {
        let Some(mut plan) = self.db().with(|q| plan(q, request))? else {
            return Ok(None);
        };
        plan.previous = plan.latest.and_then(|id| {
            delivery::read_snapshot(self.data_dir(), request.ticket, id)
                .ok()
                .map(|frozen| (frozen.id, frozen.contract_revision, frozen.files))
        });
        let mut payload = json!({
            "contract_revision": plan.contract,
            "gate": plan.gate.0,
        });
        let (root, isolated, base) = match &plan.root {
            Ok(root) => root.clone(),
            Err(reason) => return Ok(Some(unknown(payload, reason))),
        };
        payload["isolated"] = json!(isolated);
        let (paths, selection) = match select(&root, &base, &plan, cancel) {
            Ok(selected) => selected,
            Err(error) => {
                ensure!(!cancel(), "任务已中止");
                return Ok(Some(unknown(payload, format!("{error:#}"))));
            }
        };
        payload["selection"] = json!(selection);
        payload["files"] = json!(paths.len());
        let current = match digests(&root, &paths) {
            Ok(current) => current,
            Err(error) => return Ok(Some(unknown(payload, format!("{error:#}")))),
        };
        let reuse = plan.previous.as_ref().and_then(|(id, contract, files)| {
            (*contract == plan.contract
                && files.len() == current.len()
                && files.iter().all(|file| current.get(&file.path) == Some(&file.sha256)))
            .then_some(*id)
        });
        let snapshot = match reuse {
            Some(id) => id,
            None => {
                let origin = SnapshotOrigin {
                    kind: SnapshotOrigin::AUTO.into(),
                    job: request.job,
                    turn: request.turn,
                    isolated,
                    selection: selection.into(),
                };
                match self.capture_inner(request.ticket, plan.contract, paths, Some(&root), Some(origin)) {
                    Ok(frozen) => frozen.id,
                    Err(error) => return Ok(Some(unknown(payload, format!("自动捕获失败：{error:#}")))),
                }
            }
        };
        payload["snapshot_id"] = json!(snapshot);
        payload["reused"] = json!(reuse.is_some());
        let mut errors = vec![];
        for requirement in &plan.behavior {
            ensure!(!cancel(), "任务已中止");
            if reuse.is_some()
                && self.db().with(|q| last_terminal(q, request.ticket, snapshot, *requirement))?
            {
                continue;
            }
            if let Err(error) = self.run_delivery_check_with_cancel(
                request.ticket,
                plan.contract,
                snapshot,
                *requirement,
                cancel,
            ) {
                ensure!(!cancel(), "任务已中止");
                errors.push(json!({"requirement": requirement, "error": short(&format!("{error:#}"))}));
            }
        }
        if !errors.is_empty() {
            payload["errors"] = json!(errors);
        }
        let (frozen, requirements) = match delivery::read_snapshot(self.data_dir(), request.ticket, snapshot)
            .and_then(|frozen| Ok((frozen, self.db().with(|q| delivery::requirements(q.conn(), request.ticket))?)))
        {
            Ok(read) => read,
            Err(error) => return Ok(Some(unknown(payload, format!("无法读取交付快照：{error:#}")))),
        };
        let (status, unmet, reason) = delivery::assess_snapshot(&requirements, &frozen);
        payload["status"] = json!(status);
        payload["reason"] = json!(reason);
        payload["requirements"] = json!(frozen
            .requirements
            .iter()
            .filter(|item| !item.withdrawn)
            .map(|item| json!({
                "id": item.id,
                "revision": item.revision,
                "required": item.required,
                "status": item.status,
                "reason": item.evidence.as_ref().map(|evidence| short(&evidence.reason)),
            }))
            .collect::<Vec<_>>());
        if plan.gate.0 {
            match self.prepare_delivery(request.ticket, plan.contract, snapshot, plan.gate.1) {
                Ok(assessment) => payload["assessment_id"] = json!(assessment.id),
                Err(error) => payload["assessment_error"] = json!(short(&format!("{error:#}"))),
            }
        }
        let failure = plan
            .gate
            .0
            .then(|| unmet.iter().find(|evidence| evidence.status == "fail"))
            .flatten()
            .and_then(|evidence| frozen.requirements.iter().find(|item| item.id == evidence.requirement_id))
            .map(|item| {
                let heading = format!("Delivery requirement #{} (revision {}): {}\n", item.id, item.revision, short(&item.text));
                match item.behavior.as_ref().and_then(|check| check.receipt.as_ref().map(|receipt| (check, receipt))) {
                    Some((check, receipt)) if receipt.status == "fail" => (
                        check.argv.clone(),
                        receipt.exit_code,
                        format!("{heading}{}", receipt.detail),
                    ),
                    _ => (
                        vec!["delivery-criterion".into(), item.id.to_string()],
                        None,
                        format!("{heading}{}", item.evidence.as_ref().map(|e| e.reason.as_str()).unwrap_or("")),
                    ),
                }
            });
        Ok(Some(AutoOutcome { payload, failure }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_skips_git_metadata_duplicates_and_missing_files() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("src")).unwrap();
        std::fs::write(directory.path().join("src/a.txt"), b"a").unwrap();
        std::fs::write(directory.path().join("b.txt"), b"bb").unwrap();
        let (kept, total, oversized) = existing(
            directory.path(),
            ["src/a.txt", "SRC/A.txt", ".git/config", "missing.txt", "b.txt"]
                .map(String::from),
        );
        assert_eq!(kept, ["src/a.txt", "b.txt"]);
        assert_eq!(total, 3);
        assert!(!oversized);
        assert!(fits(&kept, total, oversized));
        assert!(!fits(&[], 0, false));
    }

    #[test]
    fn short_bounds_reason_text_on_char_boundaries() {
        let long = "需".repeat(REASON_CHARS + 5);
        let kept = short(&long);
        assert_eq!(kept.chars().count(), REASON_CHARS + 1);
        assert!(kept.ends_with('…'));
        assert_eq!(short("ok"), "ok");
    }

    fn git(path: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=Fixture", "-c", "user.email=fixture@localhost"])
            .args(["-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
            .args(args)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    fn count(engine: &Engine, table: &str) -> i64 {
        engine
            .db()
            .with(|q| q.int(&format!("SELECT COUNT(*) FROM {table}"), &[]))
            .unwrap()
    }

    #[tokio::test]
    async fn unchanged_inputs_reuse_the_snapshot_and_large_projects_fall_back_to_changes() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("expected.txt"), "42\n").unwrap();
        std::fs::write(project.join("result.txt"), "42\n").unwrap();
        git(&project, &["init"]);
        git(&project, &["add", "."]);
        git(&project, &["commit", "-m", "base"]);
        let engine = Engine::open(
            directory.path().join("ledger"),
            vec![],
            crate::EngineOptions::default(),
        )
        .unwrap();
        let cwd = project.to_string_lossy().to_string();
        engine.db().with(|q| {
            q.run("INSERT INTO tickets(id,goal,cwd,writes,lead,created) VALUES(1,'fixture',?,1,'codex',1)", &[&cwd.as_str()])?;
            Ok(())
        }).unwrap();
        let argv: Vec<String> = ["git", "diff", "--no-index", "--exit-code", "--", "expected.txt", "result.txt"]
            .map(String::from)
            .into();
        engine.set_project_checks(&cwd, vec![argv.clone()]).unwrap();
        let request = AutoRequest { ticket: 1, job: 7, turn: 1, cwd: cwd.clone() };
        assert!(engine.auto_delivery(&request, &|| false).unwrap().is_none());
        let id = engine
            .save_requirement(1, 0, None, "Result is 42".into(), None, None, None)
            .unwrap()
            .requirements[0]
            .id;
        engine
            .adopt_delivery_check_with_files(1, 1, id, argv, vec!["expected.txt".into()])
            .unwrap();

        let first = engine.auto_delivery(&request, &|| false).unwrap().unwrap();
        assert_eq!(first.payload["status"], "ready", "{}", first.payload);
        assert_eq!(first.payload["reused"], false);
        let again = engine.auto_delivery(&request, &|| false).unwrap().unwrap();
        assert_eq!(again.payload["reused"], true);
        assert_eq!(again.payload["snapshot_id"], first.payload["snapshot_id"]);
        assert_eq!(again.payload["status"], "ready");
        assert_eq!((count(&engine, "delivery_snapshots"), count(&engine, "delivery_behavior_attempts")), (1, 1));

        // Past the file bound: previous inputs plus changes, not the whole tree.
        for index in 0..delivery::MAX_FILES {
            std::fs::write(project.join(format!("bulk-{index}.txt")), "x").unwrap();
        }
        git(&project, &["add", "."]);
        git(&project, &["commit", "-m", "bulk"]);
        std::fs::write(project.join("result.txt"), "41\n").unwrap();
        let large = engine.auto_delivery(&request, &|| false).unwrap().unwrap();
        assert_eq!(large.payload["selection"], "previous_and_changed", "{}", large.payload);
        assert_eq!(large.payload["files"], 2);
        assert_eq!(large.payload["status"], "blocked");
        // Gate off: recorded, never bounced.
        assert!(large.failure.is_none());
        engine.set_delivery_gate(1, 0, true).unwrap();
        let gated = engine.auto_delivery(&request, &|| false).unwrap().unwrap();
        assert_eq!(gated.payload["reused"], true);
        assert!(gated.payload["assessment_id"].as_i64().is_some());
        let (argv, exit, tail) = gated.failure.unwrap();
        assert_eq!(argv[1], "diff");
        assert_eq!(exit, Some(1));
        assert!(tail.starts_with("Delivery requirement #"), "{tail}");
        engine.close().await;
    }
}
