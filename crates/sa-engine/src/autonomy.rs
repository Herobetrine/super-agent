//! Continuous autonomy state machine (`advance`, PR:136-216).
//!
//! Called inside the commit transaction. A discovery epoch cannot reuse
//! evidence or a native session belonging to an older milestone.

use crate::db::{Q, int_of, now, str_of};
use crate::job::JobCtx;
use crate::protocol::{self as p, seconds_json, unresolved};
use anyhow::Result;
use serde_json::{Map, Value, json};

pub struct Decision {
    pub state: &'static str,
    /// Start a fresh native session for the next turn.
    pub reset: bool,
}

fn with_milestone(finish: &Value, milestone: i64) -> Value {
    let mut m: Map<String, Value> = finish.as_object().cloned().unwrap_or_default();
    m.insert("milestone".into(), json!(milestone));
    Value::Object(m)
}

pub fn advance(
    q: &Q,
    job: &JobCtx,
    run: &Map<String, Value>,
    actions: &Map<String, Value>,
    reports: &[Value],
    settled: bool,
    correction: Option<String>,
) -> Result<Decision> {
    let ticket = job.ticket;
    let who = job.who();
    let goal = actions.get("SA_GOAL");
    let learn = actions.get("SA_LEARN");
    let next = actions.get("SA_NEXT");
    let finish = actions.get("SA_FINISH");
    let end = actions.get("SA_EXHAUSTED");
    if let Some(learn) = learn {
        let lesson = learn.get("lesson").and_then(Value::as_str).unwrap_or_default();
        if lesson != str_of(run, "learning") {
            q.run(
                "UPDATE continuous_runs SET learning=? WHERE ticket=?",
                &[&lesson, &ticket],
            )?;
            q.log(who, "auto.learn", learn)?;
        }
    }
    let checkpoint = |phase: &str, kind: &str, data: &Value| -> Result<()> {
        let seq = q.log(who, kind, data)?;
        q.run(
            "UPDATE continuous_runs SET phase=?, boundary_job=(SELECT max(id) FROM jobs WHERE ticket=?),
      boundary_seq=?,probe=0,success=0,stalls=0,last_signal='',milestone=milestone+? WHERE ticket=?",
            &[&phase, &ticket, &seq, &i64::from(kind == "auto.milestone"), &ticket],
        )?;
        Ok(())
    };
    let probe = int_of(run, "probe");
    let success = int_of(run, "success");
    let mut hint = correction;
    if str_of(run, "phase") == "work" {
        if let Some(goal) = goal {
            q.run(
                "UPDATE tickets SET objective=? WHERE id=?",
                &[&goal.get("objective").and_then(Value::as_str), &ticket],
            )?;
            q.log(who, "auto.goal", goal)?;
        }
        if let (Some(finish), true) = (finish, settled) {
            let missing = unresolved(reports, finish);
            let done = finish.get("status").and_then(Value::as_str) == Some("done");
            if done && !missing.is_empty() {
                let ids: Vec<String> = missing.iter().map(i64::to_string).collect();
                hint = Some(format!("核对失败的子任务并记录解决证据：{}", ids.join(",")));
            } else {
                checkpoint(
                    "discover",
                    "auto.milestone",
                    &with_milestone(finish, int_of(run, "milestone") + 1),
                )?;
                return Ok(Decision {
                    state: "waiting",
                    reset: true,
                });
            }
        } else if end.is_some() {
            hint = Some("请先用 SA_FINISH 结束阶段，再实际检查当前项目。".into());
        }
    } else if !settled {
        hint = Some("等待协作者回报并复核。".into());
    } else if probe == 0 && (goal.is_some() || end.is_some()) {
        hint = Some("需要当前探查阶段的实际工具结果。".into());
    } else if let Some(end) = end {
        let done = end.get("status").and_then(Value::as_str) == Some("done");
        if done && (success == 0 || !unresolved(reports, end).is_empty()) {
            hint = Some("成功耗尽需要有效的工具结果，并要核对失败或取消的任务。".into());
        } else {
            let mut data: Map<String, Value> = end.as_object().cloned().unwrap_or_default();
            data.insert("last_probe_seq".into(), json!(probe));
            data.insert("last_success_seq".into(), json!(success));
            q.log(who, "auto.exhausted", &Value::Object(data))?;
            q.run(
                "UPDATE continuous_runs SET phase='exhausted',stalls=0 WHERE ticket=?",
                &[&ticket],
            )?;
            return Ok(Decision {
                state: if done { "done" } else { "failed" },
                reset: false,
            });
        }
    } else if let (Some(goal), Some(next), true) = (
        goal,
        next,
        goal.is_some_and(|g| p::valid_text(g.get("evidence"), usize::MAX)),
    ) {
        let objective = goal.get("objective").and_then(Value::as_str);
        let evidence = goal.get("evidence").and_then(Value::as_str);
        let old = q.exists(
            "SELECT 1 FROM events WHERE ticket=? AND kind='auto.discovered'
      AND json_extract(payload,'$.objective')=? AND json_extract(payload,'$.evidence')=?",
            &[&ticket, &objective, &evidence],
        )?;
        if old {
            hint = Some("此目标和证据已处理，请用新的事实选择后续工作。".into());
        } else {
            checkpoint(
                "work",
                "auto.discovered",
                &json!({
                    "objective": goal.get("objective"),
                    "evidence": goal.get("evidence"),
                    "body": next.get("body"),
                    "probe_seq": probe,
                }),
            )?;
            q.run(
                "UPDATE tickets SET objective=? WHERE id=?",
                &[&objective, &ticket],
            )?;
            q.log(who, "auto.next", next)?;
            return Ok(Decision {
                state: "waiting",
                reset: true,
            });
        }
    } else if goal.is_some() || finish.is_some() {
        hint = Some("探查阶段需要有 evidence 的 SA_GOAL 和 SA_NEXT，或完整的 SA_EXHAUSTED。".into());
    }
    let step = match (hint, next) {
        (Some(h), _) => json!({"body": h}),
        (None, Some(next)) => next.clone(),
        (None, None) => json!({"body": if settled { "请依据当前阶段和实际结果继续。" } else { "等待协作者回报并复核。" }}),
    };
    let body = step.get("body").and_then(Value::as_str).unwrap_or_default();
    let stalls = if settled && body == str_of(run, "last_signal") {
        int_of(run, "stalls") + 1
    } else {
        0
    };
    q.run(
        "UPDATE continuous_runs SET stalls=?,last_signal=? WHERE ticket=?",
        &[&stalls, &body, &ticket],
    )?;
    if stalls > 0 {
        let delay = 300f64.min(2f64.powi(stalls.min(9) as i32));
        q.run(
            "UPDATE jobs SET ready_after=? WHERE id=?",
            &[&(now() + delay), &job.id],
        )?;
        q.log(
            who,
            "auto.backoff",
            &json!({"reason": "重复的下一步，退避后重新评估。", "after_seconds": seconds_json(delay)}),
        )?;
    }
    q.log(who, "auto.next", &step)?;
    Ok(Decision {
        state: "waiting",
        reset: false,
    })
}
