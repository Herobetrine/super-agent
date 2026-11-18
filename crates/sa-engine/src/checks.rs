//! Deterministic completion evidence, separate from the worker's own report.
use crate::db::{Q, int_of, now, str_of};
use crate::job::JobCtx;
use crate::protocol::{self as p, digest, stringify};
use anyhow::Result;
use serde_json::{Value, json};

pub(crate) fn configured(q: &Q, project: &str) -> Result<Vec<Vec<String>>> {
    let Some(row) = q.one("SELECT checks FROM project_checks WHERE project=?", &[&project])? else {
        return Ok(vec![]);
    };
    Ok(serde_json::from_str(str_of(&row, "checks"))?)
}

pub(crate) fn writer(job: &JobCtx) -> bool {
    job.writing || (job.is_root() && p::is_full(&job.profile))
}

pub(crate) struct Plan {
    pub project: String,
    pub directory: String,
    pub commands: Vec<Vec<String>>,
    pub checks_sha: String,
    pub attempt: i64,
    pub has_bounce: bool,
    previous: Option<(String, String)>,
}

/// The entire claim decision uses one snapshot, including settings and cache.
/// Correction notes already shown to this adapter will be consumed at commit.
pub(crate) fn plan(
    q: &Q, job: &JobCtx, project: &str, text: &str, tool_follow: bool, corrections: &[i64],
) -> Result<Option<Plan>> {
    if !writer(job) || !q.live(job.who())? { return Ok(None); }
    let commands = configured(q, project)?;
    if commands.is_empty() || tool_follow || q.pending_children(job.id)?
        || q.followed_after(job.ticket, job.admission)?
        || p::last_final_control(text, "SA_ASK").is_some()
        || p::last_final_control(text, "SA_REPORT").is_some_and(|report|
            matches!(report["status"].as_str(), Some("partial" | "blocked")))
    {
        return Ok(None);
    }
    let mail = q.all("SELECT seq FROM mailbox WHERE target=? AND consumed_turn IS NULL", &[&job.id])?;
    if mail.iter().any(|row| !corrections.contains(&int_of(row, "seq"))) { return Ok(None); }
    if job.auto_root() && !p::controls(text, job.primary()).ok()
        .and_then(|actions| actions.get("SA_FINISH").cloned())
        .is_some_and(|finish| finish["status"] == "done")
    {
        return Ok(None);
    }
    let directory = crate::workspaces::scope(&job.cwd);
    let checks_sha = digest(&stringify(&json!(commands)));
    let previous = q.one("SELECT fingerprint,checks_sha FROM check_passes WHERE project=? AND directory=?",
        &[&project, &directory])?.map(|row| (str_of(&row, "fingerprint").into(), str_of(&row, "checks_sha").into()));
    let bounce = q.one("SELECT attempt FROM check_bounces WHERE job=?", &[&job.id])?;
    let has_bounce = bounce.is_some();
    let attempt = bounce.map(|row| int_of(&row, "attempt")).unwrap_or(0) + 1;
    Ok(Some(Plan { project: project.into(), directory, commands, checks_sha, attempt, has_bounce, previous }))
}

impl Plan {
    pub fn cached(&self, fingerprint: Option<&str>) -> bool {
        fingerprint.is_some_and(|fingerprint| self.previous.as_ref()
            .is_some_and(|(previous, sha)| previous == fingerprint && sha == &self.checks_sha))
    }
}

pub(crate) struct Outcome {
    pub plan: Plan,
    pub fingerprint: Option<String>,
    pub payload: Value,
    /// The failing argv and output tail are kept outside the compact receipt.
    pub failure: Option<(Vec<String>, Option<i32>, String)>,
}

impl Outcome {
    pub fn failed(&self) -> bool { self.failure.is_some() }
    pub fn bounced(&self) -> bool { self.failed() && self.plan.attempt <= 2 }

    pub fn note(&self) -> String {
        let (argv, exit, tail) = self.failure.as_ref().expect("failed check");
        let exit = exit.map(|code| code.to_string()).unwrap_or_else(|| "unavailable (timeout or not run)".into());
        format!("Completion check failed (attempt {} of 2): {}\nexit {exit}\n{tail}\nFix the cause if your change produced it. If the failure is unrelated to your work, do not hide it: end with SA_REPORT status \"partial\" and say so with evidence.", self.plan.attempt, argv.join(" "))
    }

    pub fn save(&self, q: &Q, job: &JobCtx) -> Result<()> {
        q.run("INSERT OR REPLACE INTO job_checks(job,turn,attempt,payload) VALUES(?,?,?,?)",
            &[&job.id, &job.turn, &self.plan.attempt, &stringify(&self.payload)])?;
        if self.failed() {
            q.run("INSERT OR REPLACE INTO check_bounces(job,attempt) VALUES(?,?)", &[&job.id, &self.plan.attempt.min(2)])?;
        } else {
            q.run("DELETE FROM check_bounces WHERE job=?", &[&job.id])?;
            if let Some(fingerprint) = &self.fingerprint {
                q.run("INSERT OR REPLACE INTO check_passes(project,directory,fingerprint,checks_sha,at) VALUES(?,?,?,?,?)",
                    &[&self.plan.project, &self.plan.directory, fingerprint, &self.plan.checks_sha, &now()])?;
            }
        }
        Ok(())
    }
}
