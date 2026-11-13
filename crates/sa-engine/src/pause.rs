//! Durable, ticket-scoped admission gate. Existing adapter calls are never cancelled.

use crate::db::{Q, int_of, str_of, who_of};
use crate::job::JobCtx;
use crate::protocol;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

pub(crate) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS ticket_pause(
  ticket INTEGER PRIMARY KEY,state TEXT NOT NULL,consolidation INTEGER NOT NULL DEFAULT 0);";

const RESUME_INSTRUCTION: &str = "用户已明确恢复工单，之前的小结交接后暂停指令已撤销。请按当前工单目标与最新要求继续工作。恢复操作本身只解除准入暂停，是否启动下一轮仍遵循现有任务状态。";

/// Admission state, independent of the existing job lifecycle and result.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PauseStatus {
    Running,
    Draining,
    Handoff,
    Paused,
}

pub(crate) fn status(q: &Q, ticket: i64) -> Result<PauseStatus> {
    match q.one("SELECT state FROM ticket_pause WHERE ticket=?", &[&ticket])? {
        None => Ok(PauseStatus::Running),
        Some(row) => match str_of(&row, "state") {
            "draining" => Ok(PauseStatus::Draining),
            "handoff" => Ok(PauseStatus::Handoff),
            "paused" => Ok(PauseStatus::Paused),
            state => bail!("未知暂停状态 {state}"),
        },
    }
}

/// Interrupted children require operator action and cannot make further progress.
fn children_running(q: &Q, ticket: i64) -> Result<bool> {
    q.exists(
        "SELECT 1 FROM jobs WHERE ticket=? AND parent IS NOT NULL
         AND state IN ('queued','running','waiting','draining') LIMIT 1",
        &[&ticket],
    )
}

fn set(q: &Q, root: &Map<String, Value>, state: &str, consolidation: bool, event: &str) -> Result<()> {
    let ticket = int_of(root, "ticket");
    q.run(
        "INSERT INTO ticket_pause(ticket,state,consolidation) VALUES(?,?,?)
         ON CONFLICT(ticket) DO UPDATE SET state=excluded.state,consolidation=excluded.consolidation",
        &[&ticket, &state, &(consolidation as i64)],
    )?;
    q.log(who_of(root), event, &json!({"state": state, "consolidation": consolidation}))?;
    Ok(())
}

pub(crate) fn request(q: &Q, ticket: i64) -> Result<PauseStatus> {
    let root = q.root(ticket)?;
    if status(q, ticket)? != PauseStatus::Running {
        return status(q, ticket);
    }
    let state = str_of(&root, "state");
    let (phase, consolidation) = if state == "running" {
        ("handoff", false)
    } else if children_running(q, ticket)? {
        ("draining", false)
    } else if matches!(state, "queued" | "waiting") {
        ("handoff", true)
    } else {
        ("paused", false)
    };
    set(q, &root, phase, consolidation, "ticket.pause_requested")?;
    if state == "queued" && queued_recovery(q, &root)? {
        settle_failure(q, &root, "暂停请求终止已排队的恢复尝试。")?;
        return status(q, ticket);
    }
    if phase != "paused" && matches!(state, "queued" | "running" | "waiting") {
        q.record_note(ticket, Some(&root["id"]), Some(&json!(
            "用户请求在小结交接后暂停。请汇总已完成工作、验证证据、未完成事项与下一步。已有子任务可完成，平台将在收齐回报后仅允许一次主控交接轮，之后等待用户明确恢复。"
        )), None, None)?;
    }
    status(q, ticket)
}

pub(crate) fn resume(q: &Q, ticket: i64) -> Result<PauseStatus> {
    let root = q.root(ticket)?;
    if q.run("DELETE FROM ticket_pause WHERE ticket=?", &[&ticket])? > 0 {
        q.log(who_of(&root), "ticket.resumed", &json!({"state": "running", "instruction": RESUME_INSTRUCTION}))?;
        if matches!(str_of(&root, "state"), "queued" | "running" | "waiting") {
            q.record_note(ticket, Some(&root["id"]), Some(&json!(RESUME_INSTRUCTION)), None, None)?;
        }
    }
    Ok(PauseStatus::Running)
}

/// Terminal roots cannot accept mailbox notes. Deliver the durable revocation
/// with their next ordinary admission, including after a native-session reset.
pub(crate) fn resumed_control(q: &Q, ticket: i64) -> Result<Option<Value>> {
    if status(q, ticket)? != PauseStatus::Running {
        return Ok(None);
    }
    q.one("SELECT payload FROM events WHERE ticket=? AND kind='ticket.resumed' ORDER BY seq DESC LIMIT 1", &[&ticket])?
        .map(|row| Ok(serde_json::from_str(str_of(&row, "payload"))?))
        .transpose()
}

/// A queued recovery has a recovery event after its latest ordinary boundary.
/// Do not infer this from turn numbers, which also advance on normal follow-up.
fn queued_recovery(q: &Q, root: &Map<String, Value>) -> Result<bool> {
    Ok(q.one(
        "SELECT kind,payload FROM events WHERE job=? AND kind IN
         ('turn.retry_scheduled','route.rotated','turn.route_wait','turn.deferred','auto.retry',
          'reply','user.follow','retry','turn.admitted','turn.pause_failure','ticket.handoff_started') ORDER BY seq DESC LIMIT 1",
        &[&int_of(root, "id")],
    )?.is_some_and(|event| {
        if str_of(&event, "kind") == "auto.retry" {
            let payload = serde_json::from_str::<Value>(str_of(&event, "payload")).unwrap_or(Value::Null);
            return payload["reason"] != "按新追加的要求重新核对。";
        }
        matches!(str_of(&event, "kind"),
            "turn.retry_scheduled" | "route.rotated" | "turn.route_wait" | "turn.deferred")
    }))
}

/// Persist the failed boundary while allowing existing children to finish.
fn settle_failure(q: &Q, root: &Map<String, Value>, reason: &str) -> Result<()> {
    let recovery_reason = if str_of(root, "state") == "queued" {
        q.one("SELECT payload FROM events WHERE job=? AND kind IN ('auto.retry','turn.retry_scheduled','route.rotated','turn.route_wait') AND seq>
            coalesce((SELECT max(seq) FROM events WHERE job=? AND kind IN ('reply','user.follow','retry')),0)
            ORDER BY seq DESC LIMIT 1", &[&int_of(root, "id"), &int_of(root, "id")])?
            .and_then(|row| serde_json::from_str::<Value>(str_of(&row, "payload")).ok())
            .and_then(|payload| payload.get("error_message").or_else(|| payload.get("reason")).and_then(Value::as_str).map(str::to_owned))
    } else { None };
    let reason = recovery_reason.as_deref().unwrap_or(reason);
    let pending = children_running(q, int_of(root, "ticket"))?;
    let state = if pending { "waiting" } else { "failed" };
    q.run("UPDATE jobs SET state=?,result=? WHERE id=?", &[&state, &reason, &int_of(root, "id")])?;
    q.log(who_of(root), "turn.pause_failure", &json!({"reason": reason, "state": state}))?;
    set(q, root, if pending { "draining" } else { "paused" }, false,
        if pending { "ticket.pause_draining" } else { "ticket.paused" })
}

pub(crate) fn after_failure(q: &Q, job: &JobCtx, reason: &str) -> Result<bool> {
    if !job.is_root() || status(q, job.ticket)? == PauseStatus::Running {
        return Ok(false);
    }
    settle_failure(q, &q.root(job.ticket)?, reason)?;
    Ok(true)
}

/// Called after normal wakeup, in the same transaction as admission.
pub(crate) fn reconcile(q: &Q) -> Result<()> {
    for row in q.all("SELECT ticket,state FROM ticket_pause WHERE state!='paused'", &[])? {
        let ticket = int_of(&row, "ticket");
        let root = q.root(ticket)?;
        let root_state = str_of(&root, "state");
        if root_state == "running" {
            continue;
        }
        if root_state == "queued" && queued_recovery(q, &root)? {
            settle_failure(q, &root, "暂停请求终止已排队的恢复尝试。")?;
            continue;
        }
        let pending = children_running(q, ticket)?;
        if pending {
            continue;
        }
        if protocol::is_terminal(root_state) || matches!(root_state, "interrupted" | "draining") {
            set(q, &root, "paused", false, "ticket.paused")?;
        } else if str_of(&row, "state") == "draining" {
            set(q, &root, "handoff", true, "ticket.handoff_started")?;
            // A child interrupted by restart cannot wake its parent normally.
            if root_state == "waiting" {
                q.run("UPDATE jobs SET state='queued',turn=turn+1 WHERE id=?", &[&root["id"].as_i64()])?;
            }
        }
    }
    Ok(())
}

pub(crate) fn consolidating(q: &Q, ticket: i64) -> Result<bool> {
    q.exists("SELECT 1 FROM ticket_pause WHERE ticket=? AND state='handoff' AND consolidation=1", &[&ticket])
}

pub(crate) fn after_reply(q: &Q, job: &JobCtx, outstanding: bool) -> Result<()> {
    if !job.is_root() || status(q, job.ticket)? != PauseStatus::Handoff {
        return Ok(());
    }
    let root = q.root(job.ticket)?;
    if consolidating(q, job.ticket)? || !outstanding {
        set(q, &root, "paused", false, "ticket.paused")
    } else {
        set(q, &root, "draining", false, "ticket.pause_draining")
    }
}
