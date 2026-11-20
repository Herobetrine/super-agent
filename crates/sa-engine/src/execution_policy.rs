//! Rule-based execution policy (architecture-evolution §8.2, phase P2).
//!
//! Deterministic rules that advise on dispatch, automatic retry and repair.
//! No planner model is called. Every decision is stored in
//! `policy_decisions` and mirrored as a `policy.decision` event in the same
//! transaction as the state change that triggered it.
//!
//! Modes (per project, `project_policies`; no row means `record`):
//! - `record`: decisions are logged, behavior never changes.
//! - `apply`: only three effects exist. A serialization dependency between
//!   overlapping isolated writers, giving up a job instead of an automatic
//!   retry, and ending a completion/integration repair loop as partial.
//!   Authorizations are never widened and nothing is resurrected.

use crate::db::{Q, Who, int_of, str_of};
use crate::job::JobCtx;
use crate::protocol::stringify;
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub const POLICY_VERSION: &str = "rules-1";
/// Automatic recoveries a ticket may spend: platform retries, route
/// rotations, auto retries, completion-check bounces and integration bounces.
pub const TICKET_AUTOMATIC_ATTEMPTS: i64 = 40;
/// Consecutive failures of one job with the same error class that stop
/// verbatim retrying.
pub const SAME_CLASS_LIMIT: i64 = 3;
/// Size cap of the lead's `policy_feedback` key.
pub const FEEDBACK_MAX_BYTES: usize = 4096;

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS project_policies(project TEXT PRIMARY KEY,
  mode TEXT NOT NULL CHECK(mode IN('record','apply')),updated REAL NOT NULL);
CREATE TABLE IF NOT EXISTS policy_decisions(id INTEGER PRIMARY KEY,ticket INTEGER NOT NULL,job INTEGER,turn INTEGER,
  point TEXT NOT NULL,policy_version TEXT NOT NULL,mode TEXT NOT NULL,inputs TEXT NOT NULL,alternatives TEXT NOT NULL,
  selected TEXT NOT NULL,reason TEXT NOT NULL,applied INTEGER NOT NULL,created REAL NOT NULL);
CREATE INDEX IF NOT EXISTS policy_decisions_ticket ON policy_decisions(ticket,id);";

/// Events that spend the ticket's automatic recovery budget. One automatic
/// recovery of a job turn may journal several of them (a retry can log both
/// `turn.retry_scheduled` and `auto.retry`), so the budget counts distinct
/// job turns per kind of recovery (retry or repair), not raw events.
const ATTEMPT_KINDS: &str =
    "'turn.retry_scheduled','route.rotated','auto.retry','check.bounced','integration.bounced'";

/// Selections that mean "nothing unusual"; the lead is not shown these.
const DEFAULT_SELECTIONS: &[&str] = &["single", "limited_parallel", "retry", "repair"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Record,
    Apply,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Record => "record",
            Self::Apply => "apply",
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "record" => Ok(Self::Record),
            "apply" => Ok(Self::Apply),
            _ => Err(anyhow::anyhow!("策略模式只能是 record 或 apply")),
        }
    }
}

pub fn project_mode(q: &Q, project: &str) -> Result<Mode> {
    let row = q.one("SELECT mode FROM project_policies WHERE project=?", &[&project])?;
    Ok(match row.as_ref().map(|r| str_of(r, "mode")) {
        Some("apply") => Mode::Apply,
        _ => Mode::Record,
    })
}

pub fn ticket_mode(q: &Q, ticket: i64) -> Result<Mode> {
    let row = q.one(
        "SELECT p.mode FROM tickets t JOIN project_policies p ON p.project=t.cwd WHERE t.id=?",
        &[&ticket],
    )?;
    Ok(match row.as_ref().map(|r| str_of(r, "mode")) {
        Some("apply") => Mode::Apply,
        _ => Mode::Record,
    })
}

pub fn set_mode(q: &Q, project: &str, mode: &str, at: f64) -> Result<()> {
    let mode = Mode::parse(mode)?;
    ensure!(!project.is_empty(), "项目目录不能为空");
    q.run(
        "INSERT INTO project_policies(project,mode,updated) VALUES(?,?,?)
         ON CONFLICT(project) DO UPDATE SET mode=excluded.mode,updated=excluded.updated",
        &[&project, &mode.as_str(), &at],
    )?;
    Ok(())
}

/// One stored decision.
pub struct Decision<'a> {
    pub point: &'a str,
    pub inputs: Value,
    pub alternatives: &'a [&'a str],
    pub selected: &'a str,
    pub reason: &'a str,
    pub applied: bool,
}

/// Persist a decision and log the matching `policy.decision` event. Returns
/// the decision id. Call inside the transaction of the state change.
pub fn record(q: &Q, who: Who, mode: Mode, d: &Decision<'_>) -> Result<i64> {
    let created = crate::db::now();
    let inputs = stringify(&d.inputs);
    let alternatives = stringify(&json!(d.alternatives));
    let id = q.insert(
        "INSERT INTO policy_decisions(ticket,job,turn,point,policy_version,mode,inputs,alternatives,selected,reason,applied,created)
         VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
        &[
            &who.ticket,
            &who.id,
            &who.turn,
            &d.point,
            &POLICY_VERSION,
            &mode.as_str(),
            &inputs,
            &alternatives,
            &d.selected,
            &d.reason,
            &i64::from(d.applied),
            &created,
        ],
    )?;
    q.log(
        who,
        "policy.decision",
        &json!({
            "id": id, "ticket": who.ticket, "job": who.id, "turn": who.turn, "point": d.point,
            "policy_version": POLICY_VERSION, "mode": mode.as_str(), "inputs": d.inputs,
            "alternatives": d.alternatives, "selected": d.selected, "reason": d.reason, "applied": d.applied,
        }),
    )?;
    Ok(id)
}

// ---------------------------------------------------------------- dispatch

/// An active (queued, running or waiting) child of the same caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sibling {
    pub job: i64,
    pub write: bool,
    pub files: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct DispatchInput {
    pub to: String,
    pub write: bool,
    pub role: String,
    pub body: String,
    pub files: Vec<String>,
    pub after: usize,
    pub explicit_model: bool,
    pub isolated: bool,
    pub siblings: Vec<Sibling>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatchVerdict {
    pub selected: &'static str,
    pub reason: String,
    /// Active sibling writers whose declared files overlap this writer's.
    pub overlapping: Vec<i64>,
    pub high_risk: bool,
}

pub const DISPATCH_CHOICES: &[&str] = &["single", "limited_parallel", "serialize", "add_review"];
const READ_ONLY_CHOICES: &[&str] = &["limited_parallel", "single"];

const RISK_WORDS: &[&str] = &[
    "concurren", "permission", "migration", "security", "recovery", "并发", "权限", "迁移", "安全", "恢复",
];

pub fn high_risk(role: &str, body: &str) -> bool {
    let text = format!("{role}\n{body}").to_lowercase();
    RISK_WORDS.iter().any(|word| text.contains(word))
}

fn normal(path: &str) -> String {
    let p = path.replace('\\', "/");
    let p = p.trim_start_matches("./").trim_end_matches('/');
    if cfg!(windows) { p.to_lowercase() } else { p.to_string() }
}

/// Same path, or one is a directory prefix of the other.
fn paths_overlap(a: &str, b: &str) -> bool {
    let (a, b) = (normal(a), normal(b));
    a == b || a.strip_prefix(&b).is_some_and(|s| s.starts_with('/')) || b.strip_prefix(&a).is_some_and(|s| s.starts_with('/'))
}

pub fn files_overlap(a: &[String], b: &[String]) -> bool {
    a.iter().any(|x| b.iter().any(|y| paths_overlap(x, y)))
}

/// §8.2 rules for a newly created child. Pure and deterministic.
pub fn decide_dispatch(input: &DispatchInput) -> DispatchVerdict {
    let risky = input.write && high_risk(&input.role, &input.body);
    let verdict = |selected, reason: String, overlapping: Vec<i64>| DispatchVerdict { selected, reason, overlapping, high_risk: risky };
    if !input.write {
        return verdict(
            "limited_parallel",
            "只读任务不修改项目，可与其他协作者并行；并发槽位仍由调度器限制。".into(),
            vec![],
        );
    }
    let writers: Vec<&Sibling> = input.siblings.iter().filter(|s| s.write).collect();
    let overlapping: Vec<i64> = writers
        .iter()
        .filter(|s| !input.files.is_empty() && files_overlap(&input.files, &s.files))
        .map(|s| s.job)
        .collect();
    let suffix = if risky { " 该任务涉及并发、权限、迁移、安全或恢复，另建议随后派一个不同执行端的只读独立审查。" } else { "" };
    if !overlapping.is_empty() {
        let jobs = overlapping.iter().map(|j| format!("#{j}")).collect::<Vec<_>>().join("、");
        let how = if input.isolated {
            "隔离写入者各用独立工作区，应在重叠文件上先后执行，避免合入冲突。"
        } else {
            "未启用隔离写入者，调度器本来就会按路径串行重叠的写入者。"
        };
        return verdict("serialize", format!("声明文件与活动中的写入者 {jobs} 重叠；{how}{suffix}"), overlapping);
    }
    // Without isolation every writer of a ticket shares the project directory,
    // and the scheduler admits one writer per directory whatever files they name.
    if !input.isolated && !writers.is_empty() {
        let jobs = writers.iter().map(|s| format!("#{}", s.job)).collect::<Vec<_>>().join("、");
        return verdict(
            "serialize",
            format!("未启用隔离写入，同一项目目录中的写入者由调度器逐个执行（活动中的写入者：{jobs}），与 files 无关。需要并行写入时请操作员开启隔离写入；只读调查可照常并行。{suffix}"),
            vec![],
        );
    }
    let blind: Vec<i64> = writers.iter().filter(|s| s.files.is_empty()).map(|s| s.job).collect();
    if risky {
        return verdict(
            "add_review",
            "高风险写入（并发、权限、迁移、安全或恢复）：建议在其完成后用 after 派一个不同执行端的只读独立审查，并把关键点写入审查任务。".into(),
            vec![],
        );
    }
    if writers.is_empty() {
        return verdict("single", "单个写入者，没有活动中的同级写入者。".into(), vec![]);
    }
    let unknown = input.files.is_empty() || !blind.is_empty();
    let reason = if unknown {
        "存在活动写入者但声明文件不足以证明重叠；暂按有限并行处理，未声明的文件冲突由工作区隔离或调度器处理。".to_string()
    } else {
        "声明文件互不重叠，可有限并行。".to_string()
    };
    verdict("limited_parallel", reason, vec![])
}

pub fn active_siblings(q: &Q, parent: i64, exclude: i64) -> Result<Vec<Sibling>> {
    let rows = q.all(
        "SELECT j.id,j.writing,f.manifest FROM jobs j LEFT JOIN job_files f ON f.job=j.id
         WHERE j.parent=? AND j.id!=? AND j.state IN ('queued','running','waiting') ORDER BY j.id",
        &[&parent, &exclude],
    )?;
    Ok(rows
        .iter()
        .map(|r| Sibling {
            job: int_of(r, "id"),
            write: int_of(r, "writing") != 0,
            files: serde_json::from_str::<Value>(str_of(r, "manifest"))
                .ok()
                .and_then(|m| m.as_array().map(|a| a.iter().filter_map(|f| f["name"].as_str().map(str::to_string)).collect()))
                .unwrap_or_default(),
        })
        .collect())
}

/// What a new child looks like to the policy.
pub struct NewChild<'a> {
    pub id: i64,
    pub to: &'a str,
    pub write: bool,
    pub role: &'a str,
    pub body: &'a str,
    pub files: &'a [String],
    pub needs: &'a [i64],
    pub explicit_model: bool,
}

/// Decision point `dispatch`: called inside the SA_SEND transaction right
/// after a NEW child (not a replay, not a continuation) was inserted.
pub fn on_dispatch(q: &Q, caller: &JobCtx, child: &NewChild<'_>) -> Result<()> {
    let mode = ticket_mode(q, caller.ticket)?;
    let isolated = q.exists("SELECT 1 FROM workspace_policy WHERE ticket=? AND isolated=1", &[&caller.ticket])?;
    let siblings = active_siblings(q, caller.id, child.id)?;
    let input = DispatchInput {
        to: child.to.to_string(),
        write: child.write,
        role: child.role.to_string(),
        body: child.body.to_string(),
        files: child.files.to_vec(),
        after: child.needs.len(),
        explicit_model: child.explicit_model,
        isolated,
        siblings,
    };
    let verdict = decide_dispatch(&input);
    let mut applied = false;
    if mode == Mode::Apply && verdict.selected == "serialize" && isolated && !verdict.overlapping.is_empty() {
        for needs in &verdict.overlapping {
            applied |= q.run("INSERT OR IGNORE INTO job_deps(job,needs) VALUES(?,?)", &[&child.id, needs])? > 0;
        }
    }
    let inputs = json!({
        "child": child.id, "to": input.to, "write": input.write, "role": input.role, "files": input.files,
        "after": input.after, "explicit_model": input.explicit_model, "isolated_writers": input.isolated,
        "high_risk": verdict.high_risk,
        "active_children": input.siblings.iter().map(|s| json!({"job": s.job, "write": s.write, "files": s.files})).collect::<Vec<_>>(),
    });
    let choices = if input.write { DISPATCH_CHOICES } else { READ_ONLY_CHOICES };
    record(
        q,
        caller.who(),
        mode,
        &Decision { point: "dispatch", inputs, alternatives: choices, selected: verdict.selected, reason: &verdict.reason, applied },
    )?;
    Ok(())
}

// ------------------------------------------------------------ retry/repair

pub const STOP_REASON_BUDGET: &str = "工单自动重试与修复已达上限";
pub const REDIAGNOSE_REASON: &str = "同类失败连续 3 次，平台不再原样重试；请重新诊断后再派发";

pub fn budget_reason(used: i64) -> String {
    format!("{STOP_REASON_BUDGET}（{used} 次），请人工处理")
}

/// Automatic recoveries already spent by `ticket`.
pub fn ticket_attempts(q: &Q, ticket: i64) -> Result<i64> {
    q.int(&format!("SELECT count(*) FROM (SELECT DISTINCT job,turn,
        kind IN ('check.bounced','integration.bounced') FROM events WHERE ticket=? AND kind IN ({ATTEMPT_KINDS}))"), &[&ticket])
}

/// Retry rule: ticket budget first, then repeated same-class failures.
/// `consecutive` counts the current failure.
pub fn decide_retry(used: i64, consecutive: i64) -> (&'static str, String) {
    if used >= TICKET_AUTOMATIC_ATTEMPTS {
        ("stop", budget_reason(used))
    } else if consecutive >= SAME_CLASS_LIMIT {
        ("rediagnose", REDIAGNOSE_REASON.to_string())
    } else {
        ("retry", format!("工单已用自动恢复 {used}/{TICKET_AUTOMATIC_ATTEMPTS} 次，同类失败连续 {consecutive} 次，继续按平台策略重试。"))
    }
}

/// Repair rule (completion-check and integration bounces).
pub fn decide_repair(used: i64) -> (&'static str, String) {
    if used >= TICKET_AUTOMATIC_ATTEMPTS {
        ("stop", budget_reason(used))
    } else {
        ("repair", format!("工单已用自动恢复 {used}/{TICKET_AUTOMATIC_ATTEMPTS} 次，继续退回修复。"))
    }
}

/// The current failure plus the failures of this job since its last reply,
/// follow, manual retry or give-up that were recovered with the same class.
fn consecutive_same_class(q: &Q, job: i64, class: &str) -> Result<i64> {
    let rows = q.all(
        "SELECT kind,payload FROM events WHERE job=? AND kind IN ('turn.retry_scheduled','route.rotated')
         AND seq>coalesce((SELECT max(seq) FROM events WHERE job=? AND kind IN ('reply','user.follow','retry','turn.gave_up')),0)
         ORDER BY seq DESC",
        &[&job, &job],
    )?;
    let mut run = 1;
    for row in rows {
        let payload: Value = serde_json::from_str(str_of(&row, "payload")).unwrap_or(Value::Null);
        let seen = match str_of(&row, "kind") {
            "route.rotated" => payload["reason"].as_str(),
            _ => payload["class"].as_str(),
        };
        if seen != Some(class) {
            break;
        }
        run += 1;
    }
    Ok(run)
}

fn job_turn(q: &Q, job: i64) -> Result<i64> {
    Ok(q.one("SELECT turn FROM jobs WHERE id=?", &[&job])?.map_or(0, |r| int_of(&r, "turn")))
}

/// Decision point `retry`, evaluated where the platform is about to schedule
/// an automatic retry, rotation or auto retry. Returns the give-up reason
/// when mode `apply` stops it. `class` is `None` where the error class is
/// unknown; only the ticket budget applies then.
pub fn gate_retry(q: &Q, job: &JobCtx, class: Option<&str>) -> Result<Option<String>> {
    let mode = ticket_mode(q, job.ticket)?;
    let used = ticket_attempts(q, job.ticket)?;
    let consecutive = match class {
        Some(class) => consecutive_same_class(q, job.id, class)?,
        None => 1,
    };
    let (selected, reason) = decide_retry(used, consecutive);
    let applied = mode == Mode::Apply && selected != "retry";
    let inputs = json!({
        "automatic_attempts": used, "budget": TICKET_AUTOMATIC_ATTEMPTS, "error_class": class,
        "consecutive_same_class": consecutive, "same_class_limit": SAME_CLASS_LIMIT,
        "root": job.is_root(), "continuous": job.continuous,
    });
    record(
        q,
        job.who(),
        mode,
        &Decision { point: "retry", inputs, alternatives: &["retry", "rediagnose", "stop"], selected, reason: &reason, applied },
    )?;
    Ok(applied.then_some(reason))
}

/// Decision points `check` and `integration` (repair bounces). Returns true
/// when mode `apply` says the bounce must not happen.
pub fn gate_repair(q: &Q, ticket: i64, job: i64, point: &str) -> Result<bool> {
    let mode = ticket_mode(q, ticket)?;
    let used = ticket_attempts(q, ticket)?;
    let (selected, reason) = decide_repair(used);
    let applied = mode == Mode::Apply && selected == "stop";
    let who = Who { ticket, id: job, turn: job_turn(q, job)? };
    let inputs = json!({"automatic_attempts": used, "budget": TICKET_AUTOMATIC_ATTEMPTS, "bounce": point});
    record(
        q,
        who,
        mode,
        &Decision { point: "repair", inputs, alternatives: &["repair", "stop"], selected, reason: &reason, applied },
    )?;
    Ok(applied)
}

// ---------------------------------------------------------------- feedback

fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max { text.to_string() } else { format!("{}…", text.chars().take(max).collect::<String>()) }
}

/// The lead's `policy_feedback` packet key. `None` when there is nothing to
/// say. Bounded by [`FEEDBACK_MAX_BYTES`].
pub fn feedback(q: &Q, job: &JobCtx) -> Result<Option<Value>> {
    let mode = ticket_mode(q, job.ticket)?;
    let used = ticket_attempts(q, job.ticket)?;
    let skip = DEFAULT_SELECTIONS.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(",");
    let recent: Vec<Value> = q
        .all(
            &format!("SELECT job,point,selected,reason,applied FROM policy_decisions WHERE ticket=? AND selected NOT IN ({skip}) ORDER BY id DESC LIMIT 5"),
            &[&job.ticket],
        )?
        .iter()
        .map(|r| json!({"job": r["job"], "point": str_of(r, "point"), "selected": str_of(r, "selected"),
            "reason": cut(str_of(r, "reason"), 160), "applied": int_of(r, "applied") != 0}))
        .collect();
    let rows = q.all(
        "SELECT j.agent,j.route_override,j.state,
           EXISTS(SELECT 1 FROM events e WHERE e.job=j.id AND e.kind='check.bounced') AS bounced
         FROM jobs j JOIN tickets t ON t.id=j.ticket
         WHERE t.cwd=(SELECT cwd FROM tickets WHERE id=?) AND j.parent IS NOT NULL AND j.state IN ('done','partial','failed')
         ORDER BY j.id DESC LIMIT 50",
        &[&job.ticket],
    )?;
    let mut routes: Vec<(String, String, [i64; 4])> = vec![];
    for r in &rows {
        let model = serde_json::from_str::<Value>(str_of(r, "route_override"))
            .ok()
            .and_then(|v| v["model"].as_str().map(str::to_string))
            .unwrap_or_else(|| "default".into());
        let agent = str_of(r, "agent").to_string();
        let index = match routes.iter().position(|(a, m, _)| *a == agent && *m == model) {
            Some(i) => i,
            None => {
                routes.push((agent, model, [0; 4]));
                routes.len() - 1
            }
        };
        let counts = &mut routes[index].2;
        match str_of(r, "state") {
            "done" => counts[0] += 1,
            "partial" => counts[1] += 1,
            _ => counts[2] += 1,
        }
        counts[3] += int_of(r, "bounced");
    }
    if mode == Mode::Record && used == 0 && recent.is_empty() && routes.is_empty() {
        return Ok(None);
    }
    let routes: Vec<Value> = routes
        .into_iter()
        .map(|(agent, model, c)| json!({"agent": agent, "model": model, "done": c[0], "partial": c[1], "failed": c[2], "check_bounced": c[3]}))
        .collect();
    let mut value = json!({
        "mode": mode.as_str(), "policy_version": POLICY_VERSION,
        "automatic_attempts": {"used": used, "budget": TICKET_AUTOMATIC_ATTEMPTS},
        "recent": recent, "routes": routes,
    });
    // Drop the least important parts until the key fits.
    while stringify(&value).len() > FEEDBACK_MAX_BYTES {
        let routes = value["routes"].as_array_mut().expect("routes");
        if !routes.is_empty() {
            routes.pop();
            continue;
        }
        let recent = value["recent"].as_array_mut().expect("recent");
        if recent.pop().is_none() {
            break;
        }
    }
    Ok(Some(value))
}

/// Stored decisions of `ticket`, newest first.
pub fn decisions(q: &Q, ticket: i64, limit: usize) -> Result<Vec<Value>> {
    let limit = limit.min(1000) as i64;
    let parse = |text: &str| serde_json::from_str::<Value>(text).unwrap_or(Value::Null);
    Ok(q.all("SELECT * FROM policy_decisions WHERE ticket=? ORDER BY id DESC LIMIT ?", &[&ticket, &limit])?
        .iter()
        .map(|r| {
            json!({
                "id": r["id"], "ticket": r["ticket"], "job": r["job"], "turn": r["turn"], "point": str_of(r, "point"),
                "policy_version": str_of(r, "policy_version"), "mode": str_of(r, "mode"),
                "inputs": parse(str_of(r, "inputs")), "alternatives": parse(str_of(r, "alternatives")),
                "selected": str_of(r, "selected"), "reason": str_of(r, "reason"),
                "applied": int_of(r, "applied") != 0, "created": r["created"],
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(write: bool, files: &[&str], siblings: Vec<Sibling>, isolated: bool) -> DispatchInput {
        DispatchInput {
            to: "codex".into(),
            write,
            role: String::new(),
            body: "整理文档".into(),
            files: files.iter().map(|f| f.to_string()).collect(),
            after: 0,
            explicit_model: false,
            isolated,
            siblings,
        }
    }

    fn writer(job: i64, files: &[&str]) -> Sibling {
        Sibling { job, write: true, files: files.iter().map(|f| f.to_string()).collect() }
    }

    #[test]
    fn read_only_work_runs_in_limited_parallel() {
        let v = decide_dispatch(&input(false, &[], vec![writer(1, &["a"])], false));
        assert_eq!(v.selected, "limited_parallel");
    }

    #[test]
    fn a_lone_writer_is_single_and_disjoint_writers_run_in_limited_parallel() {
        assert_eq!(decide_dispatch(&input(true, &[], vec![], false)).selected, "single");
        let reader = Sibling { job: 1, write: false, files: vec![] };
        assert_eq!(decide_dispatch(&input(true, &[], vec![reader], false)).selected, "single");
        let v = decide_dispatch(&input(true, &["b.txt"], vec![writer(1, &["a.txt"])], true));
        assert_eq!(v.selected, "limited_parallel");
        assert!(v.overlapping.is_empty());
    }

    #[test]
    fn overlapping_declared_files_serialize_including_directory_prefixes() {
        let v = decide_dispatch(&input(true, &["src/a.rs"], vec![writer(7, &["src"]), writer(8, &["docs"])], true));
        assert_eq!((v.selected, v.overlapping), ("serialize", vec![7]));
        let v = decide_dispatch(&input(true, &["./A.txt"], vec![writer(7, &["a.txt"])], true));
        assert_eq!(v.selected, if cfg!(windows) { "serialize" } else { "limited_parallel" });
    }

    #[test]
    fn undeclared_writers_serialize_only_when_isolation_is_off() {
        let v = decide_dispatch(&input(true, &[], vec![writer(1, &[])], false));
        assert_eq!(v.selected, "serialize");
        assert!(v.overlapping.is_empty());
        assert!(v.reason.contains("逐个执行") && v.reason.contains("开启隔离写入"));
        // Disjoint declared files do not run in parallel in a shared directory either.
        let v = decide_dispatch(&input(true, &["src/a.rs"], vec![writer(1, &["docs/b.md"])], false));
        assert_eq!(v.selected, "serialize");
        assert!(v.overlapping.is_empty());
        let v = decide_dispatch(&input(true, &[], vec![writer(1, &[])], true));
        assert_eq!(v.selected, "limited_parallel");
    }

    #[test]
    fn high_risk_writers_get_independent_review_advice() {
        for text in ["fix concurrency bug", "Update PERMISSIONS model", "schema migration", "security patch", "crash recovery", "并发写入", "权限", "迁移", "安全", "恢复流程"] {
            let mut i = input(true, &[], vec![], false);
            i.body = text.into();
            assert_eq!(decide_dispatch(&i).selected, "add_review", "{text}");
        }
        let mut i = input(false, &[], vec![], false);
        i.body = "security audit".into();
        assert_eq!(decide_dispatch(&i).selected, "limited_parallel");
        assert!(!high_risk("docs", "整理 README"));
    }

    #[test]
    fn serialize_outranks_review_but_keeps_the_risk_note() {
        let mut i = input(true, &["a"], vec![writer(3, &["a"])], true);
        i.role = "security".into();
        let v = decide_dispatch(&i);
        assert_eq!(v.selected, "serialize");
        assert!(v.high_risk && v.reason.contains("独立审查"));
    }

    #[test]
    fn retry_rule_orders_budget_before_repeated_failures() {
        assert_eq!(decide_retry(0, 1).0, "retry");
        assert_eq!(decide_retry(39, 2).0, "retry");
        assert_eq!(decide_retry(0, 3).0, "rediagnose");
        assert_eq!(decide_retry(40, 1), ("stop", "工单自动重试与修复已达上限（40 次），请人工处理".to_string()));
        assert_eq!(decide_retry(41, 9).0, "stop");
        assert_eq!(decide_retry(5, 3).1, REDIAGNOSE_REASON);
    }

    #[test]
    fn repair_rule_stops_only_at_the_budget() {
        assert_eq!(decide_repair(39).0, "repair");
        assert_eq!(decide_repair(40).0, "stop");
    }

    #[test]
    fn mode_parsing_rejects_unknown_values() {
        assert_eq!(Mode::parse("apply").unwrap(), Mode::Apply);
        assert!(Mode::parse("auto").is_err());
        assert!(Mode::parse("").is_err());
    }
}
