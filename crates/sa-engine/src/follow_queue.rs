//! Persistent operator follow-ups. Mutations share the engine write transaction.
use crate::Engine;
use crate::db::{Q, int_of, str_of, who_of};
use crate::protocol::{self as p, stringify};
use anyhow::{Result, bail, ensure};
use sa_store::HoldReason;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowOutcome {
    Sent { seq: i64 },
    Queued { id: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Promotion {
    Steered,
    NextTurn,
}

fn supports_mid_turn_notes(agent: &str) -> bool {
    // M1.4's adapter capability table will replace this mapping.
    matches!(agent, "codex" | "claude")
}

#[derive(Debug)]
pub struct NotQueued;

impl std::fmt::Display for NotQueued {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("这条已开始发送，无法删除")
    }
}

impl std::error::Error for NotQueued {}

#[derive(Debug)]
pub struct ResumeConfirmationRequired;

impl std::fmt::Display for ResumeConfirmationRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("恢复已停止的持续自动工单需要确认")
    }
}

impl std::error::Error for ResumeConfirmationRequired {}

fn changed(rows: usize) -> Result<()> {
    if rows == 1 {
        Ok(())
    } else {
        Err(NotQueued.into())
    }
}

fn queued(q: &Q, ticket: i64, id: i64) -> Result<Map<String, Value>> {
    q.one(
        "SELECT * FROM follow_queue WHERE ticket=? AND id=? AND state='queued'",
        &[&ticket, &id],
    )?
    .ok_or_else(|| NotQueued.into())
}

pub(crate) fn submit(
    q: &Q,
    ticket: i64,
    root: &Map<String, Value>,
    body: &str,
    resume: bool,
    additions: &[Value],
) -> Result<FollowOutcome> {
    let job = int_of(root, "id");
    let state = str_of(root, "state");
    if state == "running"
        || (matches!(state, "queued" | "waiting")
            && q.exists(
                "SELECT 1 WHERE NOT EXISTS
                 (SELECT 1 FROM follow_queue_status WHERE job=?1 AND hold IS NOT NULL)
                 AND (EXISTS(SELECT 1 FROM follow_queue_status WHERE job=?1 AND awaiting_admission=1)
                      OR (?2='queued' AND EXISTS(SELECT 1 FROM follow_queue WHERE job=?1)))",
                &[&job, &state],
            )?)
    {
        let manifest = serde_json::to_string(additions)?;
        let id = q.insert(
            "INSERT INTO follow_queue(ticket,job,position,body,manifest,resume,state)
            SELECT ?,?,coalesce(max(position),0)+1,?,?,?,'queued' FROM follow_queue WHERE job=?",
            &[&ticket, &job, &body, &manifest, &resume, &job],
        )?;
        if !additions.is_empty() {
            validate_manifests(q, job)?;
        }
        Ok(FollowOutcome::Queued { id })
    } else {
        let seq = send(q, ticket, root, body, resume, additions)?;
        if !additions.is_empty() {
            validate_manifests(q, job)?;
        }
        Ok(FollowOutcome::Sent { seq })
    }
}

fn manifest(q: &Q, job: i64) -> Result<Vec<Value>> {
    match q.one("SELECT manifest FROM job_files WHERE job=?", &[&job])? {
        Some(row) => Ok(serde_json::from_str(str_of(&row, "manifest"))?),
        None => Ok(Vec::new()),
    }
}

fn merge_files(files: &mut Vec<Value>, additions: &[Value]) -> Result<()> {
    let image = |f: &Value| f["name"].as_str().is_some_and(crate::images::is_image_name);
    let new_images = additions.iter().any(image);
    files.retain(|old| {
        !(new_images && image(old) || additions.iter().any(|new| new["name"] == old["name"]))
    });
    files.extend_from_slice(additions);
    check_size(files.len(), files.iter().map(file_bytes).sum())
}

fn file_bytes(file: &Value) -> u128 {
    u128::from(file["bytes"].as_u64().unwrap_or(u64::MAX))
}

fn check_size(count: usize, bytes: u128) -> Result<()> {
    ensure!(
        count <= 16 && bytes <= 64 * 1024 * 1024,
        "当前附件最多 16 个文件，总量不得超过 64 MiB"
    );
    Ok(())
}

// Text replaces files of the same name; one new image batch replaces all
// previous images. Account for both possible outcomes of each in-flight note.
fn project_files(
    sizes: &mut HashMap<Option<String>, (usize, u128)>,
    files: &[Value],
    optional: bool,
) -> Result<()> {
    let mut replacements = HashMap::new();
    for file in files {
        let name = file["name"].as_str().unwrap_or_default();
        let key = (!crate::images::is_image_name(name)).then(|| name.to_owned());
        let (count, bytes) = replacements.entry(key).or_insert((0, 0));
        *count += 1;
        *bytes += file_bytes(file);
    }
    for (key, (count, bytes)) in replacements {
        let size = sizes.entry(key).or_default();
        *size = if optional {
            (size.0.max(count), size.1.max(bytes))
        } else {
            (count, bytes)
        };
    }
    check_size(
        sizes.values().map(|x| x.0).sum(),
        sizes.values().map(|x| x.1).sum(),
    )
}

// Check the complete delivery order while it is being changed. An invalid
// cumulative attachment set must never roll back a later, completed reply.
fn validate_manifests(q: &Q, job: i64) -> Result<()> {
    let rows = q.all(
        "SELECT manifest,state FROM follow_queue WHERE job=? AND manifest!='[]' ORDER BY position,id",
        &[&job],
    )?;
    if !rows.is_empty() {
        let mut sizes = HashMap::new();
        project_files(&mut sizes, &manifest(q, job)?, false)?;
        for row in rows {
            let additions: Vec<Value> = serde_json::from_str(str_of(&row, "manifest"))?;
            project_files(&mut sizes, &additions, str_of(&row, "state") == "steering")?;
        }
    }
    Ok(())
}

fn send(
    q: &Q,
    ticket: i64,
    root: &Map<String, Value>,
    body: &str,
    resume: bool,
    additions: &[Value],
) -> Result<i64> {
    let state = str_of(root, "state");
    let cont = q.exists("SELECT 1 FROM continuous_runs WHERE ticket=?", &[&ticket])?;
    if !cont && matches!(state, "interrupted" | "draining") {
        bail!("请先处理上级中断或失败");
    }
    if cont && state == "cancelled" && !resume {
        bail!("停止的工单需要明确恢复");
    }
    let id = int_of(root, "id");
    if !additions.is_empty() {
        let new_images = additions.iter().any(|file| {
            file["name"]
                .as_str()
                .is_some_and(crate::images::is_image_name)
        });
        if new_images && !crate::images::supports_native_images(str_of(root, "agent")) {
            bail!(crate::images::UNSUPPORTED);
        }
        let mut manifest = manifest(q, id)?;
        merge_files(&mut manifest, additions)?;
        q.run(
            "INSERT OR REPLACE INTO job_files(job,manifest) VALUES(?,?)",
            &[&id, &stringify(&json!(manifest))],
        )?;
        q.log(who_of(root), "files", &json!(manifest))?;
    }
    q.withdraw_question(who_of(root))?;
    let seq = q.log(who_of(root), "user.follow", &json!(body))?;
    if cont {
        q.run(
            "UPDATE continuous_runs SET phase='work',probe=0,success=0,stalls=0,last_signal='',
  boundary_seq=? WHERE ticket=?",
            &[&seq, &ticket],
        )?;
        q.run("UPDATE jobs SET ready_after=0 WHERE id=?", &[&id])?;
    }
    let auto = q
        .one(
            "SELECT permission_profile FROM tickets WHERE id=?",
            &[&ticket],
        )?
        .is_some_and(|t| str_of(&t, "permission_profile") == "auto");
    if auto {
        q.run(
            "UPDATE tickets SET auto_cycles=0,objective=? WHERE id=?",
            &[&body, &ticket],
        )?;
    }
    if p::is_terminal(state)
        || (auto && (state == "waiting" || (cont && matches!(state, "interrupted" | "draining"))))
    {
        q.run(
            "UPDATE jobs SET state='queued',turn=turn+1,dispatch_turn=NULL WHERE id=?",
            &[&id],
        )?;
    }
    Ok(seq)
}

impl Engine {
    pub fn edit_queued(&self, ticket: i64, id: i64, body: &str) -> Result<()> {
        self.db().tx(|q| {
            changed(q.run(
                "UPDATE follow_queue SET body=? WHERE ticket=? AND id=? AND state='queued'",
                &[&body, &ticket, &id],
            )?)?;
            ensure!(p::valid_str(body, usize::MAX), "追加指令为空");
            Ok(())
        })
    }

    pub fn delete_queued(&self, ticket: i64, id: i64) -> Result<()> {
        self.db().tx(|q| {
            let row = queued(q, ticket, id)?;
            changed(q.run(
                "DELETE FROM follow_queue WHERE ticket=? AND id=? AND state='queued'",
                &[&ticket, &id],
            )?)?;
            if str_of(&row, "manifest") != "[]" {
                validate_manifests(q, int_of(&row, "job"))?;
            }
            Ok(())
        })
    }

    pub fn move_queued(&self, ticket: i64, id: i64, before: Option<i64>) -> Result<()> {
        self.db().tx(|q| {
            let row = queued(q, ticket, id)?;
            let job = int_of(&row, "job");
            let position = if let Some(anchor) = before {
                let target = queued(q, ticket, anchor)?;
                if int_of(&target, "job") != job {
                    return Err(NotQueued.into());
                }
                let position = int_of(&target, "position");
                if anchor != id {
                    q.run(
                        "UPDATE follow_queue SET position=position+1 WHERE job=? AND position>=?",
                        &[&job, &position],
                    )?;
                }
                position
            } else {
                q.int(
                    "SELECT max(position)+1 FROM follow_queue WHERE job=?",
                    &[&job],
                )?
            };
            changed(q.run(
                "UPDATE follow_queue SET position=? WHERE ticket=? AND id=? AND state='queued'",
                &[&position, &ticket, &id],
            )?)?;
            validate_manifests(q, job)
        })
    }

    pub fn promote_queued(&self, ticket: i64, id: i64) -> Result<Promotion> {
        self.db().tx(|q| {
            let row = queued(q, ticket, id)?;
            let root = q.root(ticket)?;
            let job = int_of(&root, "id");
            if str_of(&root, "state") == "running"
                && supports_mid_turn_notes(str_of(&root, "agent"))
                && str_of(&row, "manifest") == "[]"
            {
                let seq = q.steering_note(ticket, job, &row["body"])?;
                changed(q.run("UPDATE follow_queue SET state='steering',note_seq=?
                    WHERE ticket=? AND id=? AND state='queued'", &[&seq, &ticket, &id])?)?;
                return Ok(Promotion::Steered);
            }
            changed(q.run("UPDATE follow_queue SET position=(SELECT min(position)-1 FROM follow_queue WHERE job=?)
                WHERE ticket=? AND id=? AND state='queued'", &[&job, &ticket, &id])?)?;
            validate_manifests(q, job)?;
            Ok(Promotion::NextTurn)
        })
    }

    pub fn resume_queue(&self, ticket: i64, confirmed_resume: bool) -> Result<()> {
        self.db().tx(|q| {
            let mut root = q.root(ticket)?;
            let job = int_of(&root, "id");
            let state = str_of(&root, "state");
            if q.run(
                "UPDATE follow_queue_status SET hold=NULL WHERE job=? AND hold IS NOT NULL",
                &[&job],
            )? == 0 {
                return Ok(());
            }
            if state == "cancelled"
                && !confirmed_resume
                && q.exists("SELECT 1 FROM continuous_runs WHERE ticket=?", &[&ticket])?
            {
                return Err(ResumeConfirmationRequired.into());
            }
            if state == "running"
                || !q.exists(
                    "SELECT 1 FROM follow_queue WHERE job=? AND state='queued'",
                    &[&job],
                )?
            {
                return Ok(());
            }
            if state == "draining" {
                // retry accepts only failed/interrupted. Wait until children settle.
                q.run(
                    "INSERT INTO follow_queue_status(job,resume_after_drain) VALUES(?,1)
                       ON CONFLICT(job) DO UPDATE SET resume_after_drain=1",
                    &[&job],
                )?;
                return Ok(());
            }
            if state == "interrupted"
                && !q.exists("SELECT 1 FROM continuous_runs WHERE ticket=?", &[&ticket])?
            {
                Self::retry_in(q, job)?;
                root = q.root(ticket)?;
            }
            consume(q, &root, true)
        })
    }

    /// Reserved for per-job cancel (M1.2).
    pub fn clear_queue(&self, job: i64) -> Result<()> {
        self.db().tx(|q| {
            restore_steering(q, job)?;
            q.run("DELETE FROM follow_queue WHERE job=?", &[&job])?;
            q.run("DELETE FROM follow_queue_status WHERE job=?", &[&job])?;
            Ok(())
        })
    }
}

fn consume(q: &Q, root: &Map<String, Value>, explicit_resume: bool) -> Result<()> {
    let job = int_of(root, "id");
    let Some(row) = q.one(
        "SELECT * FROM follow_queue WHERE job=?1 AND state='queued'
        AND NOT EXISTS(SELECT 1 FROM follow_queue_status WHERE job=?1 AND hold IS NOT NULL)
        AND NOT EXISTS(SELECT 1 FROM ticket_pause WHERE ticket=?2)
        AND NOT EXISTS(SELECT 1 FROM questions WHERE job=?1 AND origin='platform' AND state='pending')
        ORDER BY position,id LIMIT 1",
        &[&job, &int_of(root, "ticket")],
    )?
    else {
        return Ok(());
    };
    let additions: Vec<Value> = serde_json::from_str(str_of(&row, "manifest"))?;
    send(
        q,
        int_of(root, "ticket"),
        root,
        str_of(&row, "body"),
        explicit_resume || int_of(&row, "resume") != 0,
        &additions,
    )?;
    changed(q.run(
        "DELETE FROM follow_queue WHERE id=? AND state='queued'",
        &[&int_of(&row, "id")],
    )?)?;
    // A second open before the upcoming admission must not fold in another item.
    q.run(
        "INSERT INTO follow_queue_status(job,awaiting_admission) VALUES(?,1)
        ON CONFLICT(job) DO UPDATE SET awaiting_admission=1",
        &[&job],
    )?;
    Ok(())
}

pub(crate) fn scheduling_resumed(q: &Q, root: &Map<String, Value>) -> Result<()> {
    let state = str_of(root, "state");
    let job = int_of(root, "id");
    if matches!(state, "running" | "draining")
        || (state == "interrupted"
            && !q.exists(
                "SELECT 1 FROM continuous_runs WHERE ticket=?",
                &[&int_of(root, "ticket")],
            )?)
        || (state == "queued" && !root["dispatch_turn"].is_null())
        || q.exists(
            "SELECT 1 FROM follow_queue_status WHERE job=? AND awaiting_admission=1",
            &[&job],
        )?
    {
        return Ok(());
    }
    // Stop always sets a job-level hold. Only a confirmed queue resume can
    // release it on a stopped continuous root, even while scheduling is paused.
    let released_stop = state == "cancelled"
        && q.exists(
            "SELECT 1 FROM follow_queue_status WHERE job=? AND hold IS NULL",
            &[&job],
        )?;
    consume(q, root, released_stop)
}

pub(crate) fn restore_steering(q: &Q, job: i64) -> Result<()> {
    // Retire the old mailbox delivery before making the follow editable again.
    // A late native/bus acknowledgment can no longer deliver it a second time.
    if !q.exists(
        "SELECT 1 FROM follow_queue WHERE job=? AND state='steering'",
        &[&job],
    )? {
        return Ok(());
    }
    q.run(
        "DELETE FROM follow_queue WHERE job=? AND state='steering'
        AND note_seq IN (SELECT seq FROM mailbox WHERE target=? AND consumed_turn IS NOT NULL)",
        &[&job, &job],
    )?;
    q.run(
        "DELETE FROM mailbox WHERE consumed_turn IS NULL AND seq IN
        (SELECT note_seq FROM follow_queue WHERE job=? AND state='steering')",
        &[&job],
    )?;
    q.run(
        "UPDATE follow_queue SET state='queued',note_seq=NULL WHERE job=? AND state='steering'",
        &[&job],
    )?;
    Ok(())
}

pub(crate) fn hold(q: &Q, job: i64, reason: HoldReason) -> Result<()> {
    restore_steering(q, job)?;
    let reason = match reason {
        HoldReason::Stopped => "stopped",
        HoldReason::Failed => "failed",
        HoldReason::Interrupted => "interrupted",
    };
    q.run(
        "INSERT INTO follow_queue_status(job,hold) VALUES(?,?)
        ON CONFLICT(job) DO UPDATE SET hold=excluded.hold,resume_after_drain=0",
        &[&job, &reason],
    )?;
    Ok(())
}

pub(crate) fn finish(q: &Q, ticket: i64) -> Result<()> {
    let root = q.root(ticket)?;
    let job = int_of(&root, "id");
    match str_of(&root, "state") {
        "failed" => hold(q, job, HoldReason::Failed),
        "cancelled" => hold(q, job, HoldReason::Stopped),
        "interrupted" => hold(q, job, HoldReason::Interrupted),
        "queued" | "done" | "partial" | "waiting" => consume(q, &root, false),
        _ => Ok(()),
    }
}

pub(crate) fn drain_finished(q: &Q, ticket: i64) -> Result<()> {
    let root = q.root(ticket)?;
    if q.run(
        "UPDATE follow_queue_status SET resume_after_drain=0 WHERE job=? AND resume_after_drain=1",
        &[&int_of(&root, "id")],
    )? == 1
    {
        consume(q, &root, false)
    } else {
        hold(q, int_of(&root, "id"), HoldReason::Failed)
    }
}

pub(crate) fn recover(q: &Q) -> Result<()> {
    for row in q.all(
        "SELECT DISTINCT job FROM follow_queue WHERE state='steering'",
        &[],
    )? {
        restore_steering(q, int_of(&row, "job"))?;
    }
    // dispatch_turn is retained across attempts of the same logical turn.
    // start() separately interrupts any abandoned running root.
    for root in q.all(
        "SELECT j.* FROM jobs j LEFT JOIN follow_queue_status s ON s.job=j.id
        WHERE j.parent IS NULL AND j.state NOT IN ('running','draining')
        AND NOT EXISTS(SELECT 1 FROM ticket_pause WHERE ticket=j.ticket)
        AND (j.state!='queued' OR j.dispatch_turn IS NULL)
        AND s.hold IS NULL AND coalesce(s.awaiting_admission,0)=0
        AND j.id IN (SELECT job FROM follow_queue)",
        &[],
    )? {
        let job = int_of(&root, "id");
        let ticket = int_of(&root, "ticket");
        match str_of(&root, "state") {
            "interrupted"
                if !q.exists("SELECT 1 FROM continuous_runs WHERE ticket=?", &[&ticket])? =>
            {
                hold(q, job, HoldReason::Interrupted)?;
            }
            "cancelled"
                if q.exists("SELECT 1 FROM continuous_runs WHERE ticket=?", &[&ticket])?
                    && q.int(
                        "SELECT resume FROM follow_queue WHERE job=? ORDER BY position,id LIMIT 1",
                        &[&job],
                    )? == 0 =>
            {
                hold(q, job, HoldReason::Stopped)?;
            }
            _ => consume(q, &root, false)?,
        }
    }
    Ok(())
}
