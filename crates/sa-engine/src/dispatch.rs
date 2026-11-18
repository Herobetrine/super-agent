//! Streamed and final control commands (port of `runtime/engine-dispatch.cjs`).

use crate::adapter::AGENTS;
use crate::db::{NoteSource, Q, str_of};
use crate::engine::Turn;
use crate::knowledge;
use crate::protocol::{self as p, digest, stringify, utf16_len};
use crate::services::InvokeArgs;
use anyhow::{Result, anyhow};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::sync::{Arc, LazyLock, atomic::Ordering};

/// A command failure. `Syntax` is `JSON.parse` throwing a `SyntaxError`,
/// which a streamed preview tolerates (E:284).
#[derive(Debug)]
pub enum CmdError {
    Syntax(String),
    Rejected(String),
    Other(anyhow::Error),
}

impl CmdError {
    pub fn message(&self) -> String {
        match self {
            Self::Syntax(m) | Self::Rejected(m) => m.clone(),
            Self::Other(e) => e.to_string(),
        }
    }

    pub fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Syntax(m) | Self::Rejected(m) => anyhow!(m),
            Self::Other(e) => e,
        }
    }
}

impl From<anyhow::Error> for CmdError {
    fn from(e: anyhow::Error) -> Self {
        match e.downcast::<Rejection>() {
            Ok(rejection) => Self::Rejected(rejection.0),
            Err(error) => Self::Other(error),
        }
    }
}

/// Only explicit protocol/authority checks use this marker. Journal,
/// filesystem and extension failures retain their original error type.
#[derive(Debug)]
struct Rejection(String);

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Rejection {}

pub(crate) fn rejected(message: impl Into<String>) -> anyhow::Error {
    Rejection(message.into()).into()
}

/// Control, line/paragraph separator and bidi-override characters (D:152).
static UNSAFE_SUMMARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[\p{Cc}\p{Zl}\p{Zp}\x{202a}-\x{202e}\x{2066}-\x{2069}]").expect("valid regex")
});

/// `command(store, ctx, line)` (D:177-229). `streaming` is true for lines
/// projected from streamed deltas, false for the final reply.
pub async fn command(turn: &Arc<Turn>, line: &str, streaming: bool) -> Result<(), CmdError> {
    if turn.aborted() {
        return Ok(());
    }
    let Some(sep) = line.find(' ') else {
        return Ok(());
    };
    let (tag, rest) = (&line[..sep], &line[sep + 1..]);
    if tag == "SA_PROGRESS" {
        return progress(turn, rest).map_err(CmdError::from);
    }
    if tag == "SA_MEMO" {
        return memo(turn, rest).map_err(CmdError::from);
    }
    if tag == "SA_CANCEL" {
        let cancelled = cancel(turn, rest, streaming).map_err(CmdError::from)?;
        // Abort outside the journal transaction (scheduler lock order).
        turn.inner().abort_jobs(&cancelled);
        return Ok(());
    }
    if !matches!(tag, "SA_ROLE" | "SA_SEND" | "SA_NOTE" | "SA_TOOL") {
        return Ok(());
    }
    let cmd: Value = serde_json::from_str(rest).map_err(|e| CmdError::Syntax(e.to_string()))?;
    match tag {
        "SA_SEND" => dispatch(turn, &cmd).await.map_err(CmdError::from),
        "SA_TOOL" => tool(turn, &cmd).await.map_err(CmdError::from),
        _ => {
            let job = &turn.job;
            turn.inner().db.tx(|q| {
                if !q.live(job.who())? {
                    return Ok(());
                }
                if tag == "SA_ROLE" {
                    if !p::valid_text(cmd.get("role"), 120) {
                        return Err(rejected("无效的职能名称"));
                    }
                    let role = cmd["role"].as_str().unwrap_or_default();
                    if q.run("UPDATE jobs SET role=? WHERE id=? AND role!=?", &[&role, &job.id, &role])? > 0 {
                        q.log(job.who(), "role", &json!({"role": role}))?;
                    }
                } else {
                    if !p::valid_text(cmd.get("id"), 96) {
                        return Err(rejected("无效的信件 ID"));
                    }
                    let source = NoteSource {
                        who: job.who(),
                        agent: job.agent.clone(),
                        logical: job.logical(),
                    };
                    q.record_note(job.ticket, cmd.get("job"), cmd.get("body"), Some(&source), cmd["id"].as_str())?;
                }
                Ok(())
            })?;
            Ok(())
        }
    }
}

async fn tool(turn: &Arc<Turn>, cmd: &Value) -> Result<()> {
    let job = &turn.job;
    let inner = turn.inner();
    if turn.aborted() || !inner.db.with(|q| q.live(job.who()))? {
        return Ok(());
    }
    let ext = inner.extensions().filter(|e| e.can_invoke());
    let Some(ext) = ext.filter(|_| job.parent.is_none() && !matches!(job.profile.as_str(), "readonly" | "legacy"))
    else {
        return Err(rejected("当前工单或执行者不可调用 MCP 工具"));
    };
    if !p::is_obj(Some(cmd))
        || !p::valid_text(cmd.get("id"), 96)
        || !p::ident(cmd.get("server"))
        || !p::ident(cmd.get("tool"))
        || !p::is_obj(cmd.get("arguments"))
    {
        return Err(rejected("SA_TOOL 需要 id、server、tool 和 arguments 对象"));
    }
    let advertised = turn
        .packet()
        .get("project_extensions")
        .and_then(|x| x.get("mcp"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    let listed = advertised.as_array().into_iter().flatten().any(|s| {
        s.get("server") == cmd.get("server")
            && s.get("tools")
                .and_then(Value::as_array)
                .is_some_and(|t| t.iter().any(|t| t.get("name") == cmd.get("tool")))
    });
    if !listed {
        return Err(rejected("工具不在本次工单启用的服务目录内"));
    }
    let approver = turn.clone();
    let result = ext
        .invoke(InvokeArgs {
            ticket: job.ticket,
            job: job.id,
            turn: job.turn,
            admission: job.admission,
            logical_turn: job.logical(),
            profile: job.profile.clone(),
            cwd: job.cwd.clone(),
            cancel: turn.cancel.clone(),
            approve: Arc::new(move |action| {
                let t = approver.clone();
                Box::pin(async move { crate::engine::approve(t, action).await })
            }),
            command: cmd.clone(),
        })
        .await?;
    if result.new_receipt {
        turn.tool_follow.store(true, Ordering::SeqCst);
    }
    Ok(())
}

/// A validated SA_SEND that creates a child (D:56-67).
struct Child {
    selection: Option<Value>,
    effective: String,
    override_: Option<String>,
    logical: i64,
    id: String,
    to: String,
    body: String,
    write: bool,
    role: String,
    files: Vec<String>,
    needs: Vec<i64>,
}

/// `check(store, ctx, cmd)` (D:5-68). `None` means an exact replay of an
/// already recorded send.
fn check(q: &Q, turn: &Turn, cmd: &Value) -> Result<Option<Child>> {
    let job = &turn.job;
    let Some(o) = cmd.as_object() else {
        return Err(rejected("SA_SEND 需要 JSON 对象"));
    };
    if ["provider", "mode", "selection", "route", "route_override", "permission_override"]
        .iter()
        .any(|k| o.contains_key(*k))
    {
        return Err(rejected("不可通过派工更换供应商或执行端"));
    }
    let bad = || rejected("派工参数无效");
    let write = match o.get("write") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(bad()),
    };
    let role = match o.get("role") {
        None => String::new(),
        Some(Value::String(s)) if utf16_len(s) <= 120 => s.clone(),
        Some(_) => return Err(bad()),
    };
    let files_value = o.get("files").cloned().unwrap_or_else(|| json!([]));
    let Some(files_list) = files_value.as_array() else {
        return Err(bad());
    };
    let to = o.get("to").and_then(Value::as_str).unwrap_or_default();
    if !p::valid_text(o.get("id"), 96) || !AGENTS.contains(&to) || !p::valid_text(o.get("body"), usize::MAX) {
        return Err(bad());
    }
    let id = o["id"].as_str().unwrap_or_default();
    let body = o["body"].as_str().unwrap_or_default();
    let has_model = o.contains_key("model");
    let has_effort = o.contains_key("reasoning_effort");
    let has_profile = o.contains_key("permission_profile");
    let routing = q.int("SELECT allow_lead_routing FROM tickets WHERE id=?", &[&job.ticket])? != 0;
    if (has_model || has_effort || has_profile) && (job.parent.is_some() || !routing) {
        return Err(rejected("只有当前获授权的主控可指定子任务模型、思考强度和权限"));
    }
    let requested = o.get("permission_profile").and_then(Value::as_str);
    if has_profile && !requested.is_some_and(|r| p::child_profiles(&job.profile).contains(&r)) {
        return Err(rejected("子任务权限不能超出上级范围"));
    }
    if has_model && !p::ident(o.get("model")) {
        return Err(rejected("模型标识无效"));
    }
    if has_effort && !p::ident(o.get("reasoning_effort")) {
        return Err(rejected("思考强度无效"));
    }
    let effort = o.get("reasoning_effort").and_then(Value::as_str);
    let default_model = if has_effort && !has_model {
        turn.menus().get(to).and_then(|menu| menu["default_model"].as_str()).map(str::to_string)
    } else {
        None
    };
    let model = o.get("model").and_then(Value::as_str).or(default_model.as_deref());
    if has_effort && model.is_none() {
        return Err(rejected("所选执行端没有可用模型目录"));
    }
    let override_ = if has_profile {
        requested.map(str::to_string)
    } else {
        job.permission_override.clone()
    };
    let effective = override_.clone().unwrap_or_else(|| job.profile.clone());
    let logical = job.logical();
    let old = q.one(
        "SELECT * FROM jobs WHERE parent=? AND source_turn=? AND msgkey=?",
        &[&job.id, &logical, &id],
    )?;
    let replay = old.as_ref().and_then(|row| row["id"].as_i64()).unwrap_or(0);
    let needs = dependencies(q, job, cmd.get("after"), replay)?;
    if let Some(old) = old {
        let manifest = q.one("SELECT manifest FROM job_files WHERE job=?", &[&old["id"].as_i64()])?;
        let names: Vec<Value> = match manifest {
            Some(m) => serde_json::from_str::<Value>(str_of(&m, "manifest"))?
                .as_array()
                .into_iter()
                .flatten()
                .map(|f| f.get("name").cloned().unwrap_or(Value::Null))
                .collect(),
            None => vec![],
        };
        let old_route = old
            .get("route_override")
            .and_then(Value::as_str)
            .and_then(|r| serde_json::from_str::<Value>(r).ok());
        let old_model = old_route.as_ref().and_then(|route| route["model"].as_str());
        let old_effort = old_route.as_ref().and_then(|route| route["reasoning_effort"].as_str());
        let old_override = old.get("permission_override").and_then(Value::as_str);
        let old_needs: Vec<i64> = q.all("SELECT needs FROM job_deps WHERE job=? ORDER BY needs", &[&old["id"].as_i64()])?
            .iter().filter_map(|row| row["needs"].as_i64()).collect();
        if str_of(&old, "agent") != to
            || str_of(&old, "body") != body
            || (old["writing"].as_i64().unwrap_or(0) != 0) != write
            || str_of(&old, "assigned_role") != role
            || old_model != model
            || old_effort != effort
            || old_override != override_.as_deref()
            || old_needs != needs
            || stringify(&files_value) != stringify(&Value::Array(names))
        {
            return Err(rejected("同一消息 ID 携带不同的任务、模型、附件或权限"));
        }
        return Ok(None);
    }
    if job.is_root() && crate::pause::consolidating(q, job.ticket)? {
        return Err(rejected("工单正在最后一次暂停交接，请汇总已有结果并结束本轮"));
    }
    let mut selection = None;
    if let Some(model) = model {
        let menus = turn.menus();
        let menu = menus.get(to);
        let listed = menu
            .and_then(|m| m.get("models"))
            .and_then(Value::as_array)
            .is_some_and(|m| m.iter().any(|x| x.as_str() == Some(model)));
        if !listed {
            return Err(rejected("所选模型不在此任务的供应商和执行端目录"));
        }
        let menu = menu.cloned().unwrap_or(Value::Null);
        let mut picked = json!({"provider": menu["provider"], "mode": menu["mode"], "model": model});
        if let Some(effort) = effort {
            let supported = menu["reasoning_efforts"][model].as_array()
                .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(effort)));
            if !supported {
                return Err(rejected("所选模型未声明支持该思考强度，请从模型目录选择"));
            }
            picked["reasoning_effort"] = json!(effort);
        }
        let route = p::route(Some(&picked)).map_err(|e| anyhow!(e))?;
        let pinned: Value = serde_json::from_str(&job.agent_routes).unwrap_or(Value::Null);
        if let Some(pinned) = pinned.get(to).filter(|v| p::truthy_value(v))
            && (pinned.get("provider").and_then(Value::as_str) != Some(&route.provider)
                || pinned.get("mode").and_then(Value::as_str) != Some(&route.mode))
        {
            return Err(rejected("工单路由已变更"));
        }
        selection = Some(p::route_value(&route));
    }
    if write && (!job.writes || effective == "readonly" || !p::can_write(to, &effective)) {
        return Err(rejected("该子任务没有所请求的写权限"));
    }
    let files = files_list
        .iter()
        .map(|f| f.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| rejected("附件需要最多 16 个项目相对路径"))?;
    Ok(Some(Child {
        selection,
        effective,
        override_,
        logical,
        id: id.into(),
        to: to.into(),
        body: body.into(),
        write,
        role,
        files,
        needs,
    }))
}

/// Resolve only existing direct children, so a new dispatch cannot form a cycle.
fn dependencies(q: &Q, job: &crate::job::JobCtx, after: Option<&Value>, replay: i64) -> Result<Vec<i64>> {
    let Some(after) = after else { return Ok(vec![]) };
    let Some(entries) = after.as_array().filter(|entries| (1..=8).contains(&entries.len())) else {
        return Err(rejected("after 需要 1 到 8 个任务 ID 或消息 ID"));
    };
    let mut needs = Vec::new();
    for entry in entries {
        let child = if let Some(key) = entry.as_str() {
            // A continuation is also an earlier SA_SEND. Its alias needs both
            // the delivered note and the successful continuation receipt.
            let candidates = q.all(
                "SELECT j.id,c.source_turn FROM jobs j JOIN (
        SELECT id,source_turn FROM jobs WHERE msgkey=?
        UNION
        SELECT m.target,m.source_turn FROM mailbox m JOIN events n ON n.seq=m.seq
        WHERE m.source=? AND m.msgkey=? AND n.kind='note' AND n.job=? AND n.ticket=?
          AND EXISTS(SELECT 1 FROM events e WHERE e.ticket=n.ticket AND e.job=n.job
            AND e.turn=n.turn AND e.seq>n.seq AND e.kind='continued'
            AND json_extract(e.payload,'$.job')=m.target AND json_extract(e.payload,'$.key')=m.msgkey)
      ) c ON c.id=j.id WHERE j.parent=? AND j.ticket=? AND j.id>? AND j.id!=?
      ORDER BY CASE WHEN c.source_turn=? THEN 0 ELSE 1 END,j.id DESC",
                &[&key, &job.id, &key, &job.id, &job.ticket,
                    &job.id, &job.ticket, &job.floor, &replay, &job.logical()],
            )?;
            if let Some(first) = candidates.first()
                && candidates.iter().any(|row| row["source_turn"] == first["source_turn"] && row["id"] != first["id"])
            {
                return Err(rejected("after 消息 ID 在同一逻辑轮次指向不同的任务，请使用任务编号"));
            }
            candidates.into_iter().next()
        } else if let Some(id) = p::safe_int(Some(entry)) {
            q.one("SELECT id FROM jobs WHERE id=? AND parent=? AND ticket=? AND id>?", &[&id, &job.id, &job.ticket, &job.floor])?
        } else {
            None
        };
        let Some(id) = child.and_then(|row| row["id"].as_i64()) else {
            return Err(rejected("after 引用了不存在的任务"));
        };
        needs.push(id);
    }
    needs.sort_unstable();
    needs.dedup();
    Ok(needs)
}

/// Continuing uses the mailbox receipt as its logical-turn idempotency key.
fn continue_child(q: &Q, turn: &Turn, cmd: &Value) -> Result<()> {
    let job = &turn.job;
    let Some(fields) = cmd.as_object() else { return Err(rejected("SA_SEND 需要 JSON 对象")) };
    if ["to", "role", "write", "files", "model", "reasoning_effort", "permission_profile", "after",
        "provider", "mode", "selection", "route", "route_override", "permission_override"]
        .iter().any(|key| fields.contains_key(*key))
    {
        return Err(rejected("continue 不可与派工目标、职能、写入、附件、路由、权限或 after 同时使用"));
    }
    let Some(target) = p::safe_int(cmd.get("continue")).filter(|id| *id > 0) else {
        return Err(rejected("continue 需要有效的子任务编号"));
    };
    if !p::valid_text(cmd.get("id"), 96) || !p::valid_text(cmd.get("body"), usize::MAX) {
        return Err(rejected("续派需要有效的消息 ID 和非空正文"));
    }
    let key = cmd["id"].as_str().unwrap_or_default();
    let logical = job.logical();
    if let Some(old) = q.one(
        "SELECT m.target,e.payload,e.turn AS note_turn,e.seq AS note_seq FROM mailbox m JOIN events e ON e.seq=m.seq WHERE m.source=? AND m.source_turn=? AND m.msgkey=?",
        &[&job.id, &logical, &key],
    )? {
        let payload: Value = serde_json::from_str(str_of(&old, "payload"))?;
        if old["target"].as_i64() != Some(target) || payload.get("body") != cmd.get("body") {
            return Err(rejected("同一续派消息 ID 携带不同的任务或正文"));
        }
        // A matching SA_NOTE uses the same mailbox key but never requeued
        // the child. Only the paired continuation event proves this replay.
        if !q.exists(
            "SELECT 1 FROM events WHERE job=? AND turn=? AND seq>? AND kind='continued'
      AND json_extract(payload,'$.job')=? AND json_extract(payload,'$.key')=?",
            &[&job.id, &old["note_turn"].as_i64(), &old["note_seq"].as_i64(), &target, &key],
        )? {
            return Err(rejected("该消息 ID 已用于信件，请为续派使用新的 ID"));
        }
        return Ok(());
    }
    let Some(child) = q.one("SELECT * FROM jobs WHERE id=?", &[&target])? else {
        return Err(rejected("续派目标任务不存在"));
    };
    if child["ticket"].as_i64() != Some(job.ticket) {
        return Err(rejected("只能续派同一工单的任务"));
    }
    if child["parent"].as_i64() != Some(job.id) {
        return Err(rejected("只能续派自己的直接子任务"));
    }
    if target <= job.floor {
        return Err(rejected("不能续派当前阶段之前的子任务"));
    }
    if !matches!(str_of(&child, "state"), "done" | "partial") {
        return Err(rejected(format!("子任务状态为「{}」，只能续派已完成或部分完成的子任务", str_of(&child, "state"))));
    }
    if q.exists("SELECT 1 FROM job_failovers WHERE original=?", &[&target])? {
        return Err(rejected("该子任务已被自动改派替代，不能续派"));
    }
    if q.exists("SELECT 1 FROM job_workspaces WHERE job=? AND state='merged'", &[&target])? {
        return Err(rejected("该工作区已合入，请创建新任务继续修改"));
    }
    if job.is_root() && crate::pause::consolidating(q, job.ticket)? {
        return Err(rejected("工单正在最后一次暂停交接，请汇总已有结果并结束本轮"));
    }
    q.run("UPDATE jobs SET state='queued',turn=turn+1,ready_after=0,dispatch_turn=NULL WHERE id=?", &[&target])?;
    let source = NoteSource { who: job.who(), agent: job.agent.clone(), logical };
    let seq = q.record_note(job.ticket, Some(&json!(target)), cmd.get("body"), Some(&source), Some(key))?;
    // A mailbox-budget refusal must roll back the requeue as well.
    if !q.exists("SELECT 1 FROM mailbox WHERE seq=? AND target=?", &[&seq, &target])? {
        return Err(rejected("自动信箱达到预算，无法续派子任务"));
    }
    q.log(job.who(), "continued", &json!({"job": target, "key": key}))?;
    Ok(())
}

/// `dispatch(store, ctx, cmd)` (D:70-124).
async fn dispatch(turn: &Arc<Turn>, cmd: &Value) -> Result<()> {
    let job = &turn.job;
    let inner = turn.inner();
    let superseded = |q: &Q| -> Result<bool> {
        Ok(job.continuous && job.parent.is_none() && q.followed_after(job.ticket, job.admission)?)
    };
    if turn.aborted() || !inner.db.with(|q| q.live(job.who()))? || inner.db.with(|q| superseded(q))? {
        return Ok(());
    }
    if cmd.as_object().is_some_and(|fields| fields.contains_key("continue")) {
        return inner.db.tx(|q| {
            if !q.live(job.who())? || superseded(q)? { return Ok(()); }
            continue_child(q, turn, cmd)
        });
    }
    let Some(first) = inner.db.with(|q| check(q, turn, cmd))? else {
        return Ok(());
    };
    // Never hold the DB writer while the filesystem snapshot runs. Authority
    // and deduplication are checked again at the commit point.
    let manifest = inner.snapshot(&job.cwd, &first.files).await?;
    if turn.aborted() {
        return Ok(());
    }
    inner.db.tx(|q| {
        if !q.live(job.who())? || superseded(q)? {
            return Ok(());
        }
        let Some(c) = check(q, turn, cmd)? else {
            return Ok(());
        };
        let selection = c.selection.as_ref().map(stringify);
        let child = q.insert(
            "INSERT INTO jobs(ticket,parent,sender,agent,body,writing,depth,source_turn,
      msgkey,state,role,assigned_role,route_override,permission_override) VALUES(?,?,?,?,?,?,?,?,?,'queued',?,?,?,?)",
            &[
                &job.ticket,
                &job.id,
                &job.agent,
                &c.to,
                &c.body,
                &(c.write as i64),
                &(job.depth + 1),
                &c.logical,
                &c.id,
                &c.role,
                &c.role,
                &selection,
                &c.override_,
            ],
        )?;
        for needs in &c.needs {
            q.run("INSERT INTO job_deps(job,needs) VALUES(?,?)", &[&child, needs])?;
        }
        if c.write && q.exists("SELECT 1 FROM workspace_policy WHERE ticket=? AND isolated=1", &[&job.ticket])? {
            // Nested children branch from their parent's committed workspace.
            crate::workspaces::plan(q,&inner.data_dir,child,job.ticket,&job.cwd)?;
        } else {
            // A reviewer of an isolated writer must see that writer's files.
            q.run("INSERT INTO job_directories(job,cwd) VALUES(?,?)",&[&child,&job.cwd])?;
        }
        if !manifest.is_empty() {
            q.run(
                "INSERT INTO job_files VALUES(?,?)",
                &[&child, &stringify(&Value::Array(manifest.clone()))],
            )?;
        }
        q.log(
            job.who(),
            "send",
            &json!({
                "to": c.to,
                "child": child,
                "body": c.body,
                "sha256": digest(&c.body),
                "write": c.write,
                "files": manifest,
                "role": c.role,
                "selection": c.selection,
                "permission_profile": c.effective,
            }),
        )?;
        // P2: rule-based advice for the new child (record mode logs only).
        crate::execution_policy::on_dispatch(q, job, &crate::execution_policy::NewChild {
            id: child, to: &c.to, write: c.write, role: &c.role, body: &c.body, files: &c.files,
            needs: &c.needs, explicit_model: c.selection.is_some(),
        })?;
        Ok(())
    })
}

/// `progress(store, ctx, json)` (D:126-175).
fn progress(turn: &Turn, text: &str) -> Result<()> {
    let job = &turn.job;
    let who = job.who();
    turn.inner().db.tx(|q| {
        if turn.aborted() || !q.live(who)? || q.followed_after(job.ticket, job.admission)? {
            return Ok(());
        }
        // Reject optional updates without quoting the malformed content or
        // failing the agent's assignment.
        let reject = |reason: &str| q.log(who, "protocol.progress_invalid", &json!({"reason": reason})).map(|_| ());
        let Ok(cmd) = serde_json::from_str::<Value>(text) else {
            return reject("invalid_json");
        };
        let fields: Option<&Map<String, Value>> = cmd.as_object().filter(|o| o.len() == 2);
        let valid = fields.is_some()
            && p::ident(cmd.get("id"))
            && cmd["id"].as_str().is_some_and(|s| utf16_len(s) <= 96)
            && p::valid_text(cmd.get("summary"), 320)
            && !cmd["summary"].as_str().is_some_and(|s| UNSAFE_SUMMARY.is_match(s));
        if !valid {
            return reject("invalid_fields");
        }
        let (id, summary) = (cmd["id"].as_str().unwrap_or_default(), cmd["summary"].as_str().unwrap_or_default());
        let logical = job.logical();
        let old = q.one(
            "SELECT e.payload FROM progress_receipts p JOIN events e ON e.seq=p.seq WHERE p.job=? AND p.source_turn=? AND p.msgkey=?",
            &[&job.id, &logical, &id],
        )?;
        if let Some(old) = old {
            let payload: Value = serde_json::from_str(str_of(&old, "payload"))?;
            if payload.get("summary").and_then(Value::as_str) != Some(summary) {
                return reject("conflicting_id");
            }
            return Ok(());
        }
        let seq = q.log(who, "job.progress", &json!({"id": id, "summary": summary}))?;
        q.run(
            "INSERT INTO progress_receipts(job,source_turn,msgkey,seq) VALUES(?,?,?,?)",
            &[&job.id, &logical, &id, &seq],
        )?;
        Ok(())
    })
}

/// `SA_MEMO {"kind","text"}` (final reply only, spec §4.4). Like
/// SA_PROGRESS, a malformed or excess entry is logged as
/// `knowledge.rejected {reason}` without quoting it or failing the job.
/// Duplicates (and replays of a retried turn) are silent no-ops.
fn memo(turn: &Turn, text: &str) -> Result<()> {
    let job = &turn.job;
    let who = job.who();
    let seen = turn.memos.fetch_add(1, Ordering::SeqCst) + 1;
    turn.inner().db.tx(|q| {
        if turn.aborted() || !q.live(who)? {
            return Ok(());
        }
        let reject = |reason: &str| q.log(who, "knowledge.rejected", &json!({"reason": reason})).map(|_| ());
        if seen > knowledge::MAX_PER_TURN {
            return reject("too_many");
        }
        let Ok(cmd) = serde_json::from_str::<Value>(text) else {
            return reject("invalid_json");
        };
        let Some((kind, body)) = knowledge::parse_memo(&cmd) else {
            return reject("invalid_fields");
        };
        let entry = knowledge::NewEntry {
            cwd: &job.cwd,
            kind,
            body,
            ticket: Some(job.ticket),
            job: Some(job.id),
            agent: &job.agent,
        };
        knowledge::insert(q, &entry, Some(who))?;
        Ok(())
    })
}

/// `SA_CANCEL {"id","job"}` (spec §9.2 F): stop one of this job's own direct
/// children and its subtree, the way the operator's cancel treats a ticket.
/// Accepted from streamed and final lines like SA_SEND and idempotent on
/// `(job, logical turn, id)` via `cancel_receipts`. Like SA_NOTE, a refused
/// cancel never fails the issuing job: it logs
/// `cancel.rejected {id, job, reason}` where `reason` is `invalid_json`,
/// `invalid_fields` or `conflicting_id` (final reply only, so a streamed
/// preview is not reported twice), or `not_child` / `terminal` (once, with a
/// receipt). An accepted cancel logs `child.cancelled {id, job, cancelled}`
/// on the issuing job and `cancelled` on every stopped job. Returns the
/// cancelled job ids so the caller can abort their running turns.
fn cancel(turn: &Turn, text: &str, streaming: bool) -> Result<Vec<i64>> {
    let job = &turn.job;
    let who = job.who();
    turn.inner().db.tx(|q| {
        if turn.aborted() || !q.live(who)? {
            return Ok(vec![]);
        }
        // A redirected continuous root drops its stale turn's controls like SA_SEND.
        if job.continuous && job.parent.is_none() && q.followed_after(job.ticket, job.admission)? {
            return Ok(vec![]);
        }
        let reject = |payload: Value| q.log(who, "cancel.rejected", &payload);
        let Ok(cmd) = serde_json::from_str::<Value>(text) else {
            if !streaming {
                reject(json!({"id": Value::Null, "job": Value::Null, "reason": "invalid_json"}))?;
            }
            return Ok(vec![]);
        };
        let target = p::safe_int(cmd.get("job")).filter(|t| *t > 0);
        let (true, Some(target)) = (p::is_obj(Some(&cmd)) && p::valid_text(cmd.get("id"), 96), target) else {
            if !streaming {
                reject(json!({"id": Value::Null, "job": Value::Null, "reason": "invalid_fields"}))?;
            }
            return Ok(vec![]);
        };
        let id = cmd["id"].as_str().unwrap_or_default();
        let logical = job.logical();
        let receipt = q.one(
            "SELECT target FROM cancel_receipts WHERE job=? AND source_turn=? AND msgkey=?",
            &[&job.id, &logical, &id],
        )?;
        if let Some(receipt) = receipt {
            if receipt.get("target").and_then(Value::as_i64) != Some(target) && !streaming {
                reject(json!({"id": id, "job": target, "reason": "conflicting_id"}))?;
            }
            return Ok(vec![]);
        }
        let child = q.one(
            "SELECT state FROM jobs WHERE id=? AND parent=? AND ticket=?",
            &[&target, &job.id, &job.ticket],
        )?;
        let (seq, cancelled) = match child {
            None => (reject(json!({"id": id, "job": target, "reason": "not_child"}))?, vec![]),
            Some(c) if p::is_terminal(str_of(&c, "state")) => (
                reject(json!({"id": id, "job": target, "reason": "terminal", "state": str_of(&c, "state")}))?,
                vec![],
            ),
            Some(_) => {
                let reason = format!("上级任务 #{} 取消了此任务", job.id);
                let ids = q.cancel_subtree(target, &reason)?;
                let seq = q.log(who, "child.cancelled", &json!({"id": id, "job": target, "cancelled": ids}))?;
                (seq, ids)
            }
        };
        q.run(
            "INSERT INTO cancel_receipts(job,source_turn,msgkey,target,seq) VALUES(?,?,?,?,?)",
            &[&job.id, &logical, &id, &target, &seq],
        )?;
        Ok(cancelled)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependencies_prefer_logical_turn_and_compare_normalized_sets() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE jobs(id INTEGER PRIMARY KEY,parent INTEGER,ticket INTEGER,msgkey TEXT,source_turn INTEGER);
          CREATE TABLE events(seq INTEGER PRIMARY KEY,ticket INTEGER,job INTEGER,turn INTEGER,kind TEXT,payload TEXT);
          CREATE TABLE mailbox(seq INTEGER,target INTEGER,source INTEGER,source_turn INTEGER,msgkey TEXT);
          INSERT INTO jobs VALUES(11,1,2,'impl',3),(12,1,2,'impl',4),(13,1,2,'review',4),(14,9,2,'foreign',4);").unwrap();
        let q = Q(&conn);
        let job = crate::job::JobCtx::from_row(json!({"id":1,"ticket":2,"floor":10,"turn":3}).as_object().unwrap());
        assert_eq!(dependencies(&q, &job, Some(&json!(["impl",12,11])), 0).unwrap(), vec![11,12]);
        assert_eq!(dependencies(&q, &job, Some(&json!(["impl"])), 11).unwrap(), vec![12]);
        assert!(dependencies(&q, &job, Some(&json!(["missing"])), 0).is_err());
        assert!(dependencies(&q, &job, Some(&json!([14])), 0).is_err());
        assert!(dependencies(&q, &job, Some(&json!([])), 0).is_err());
    }

    #[test]
    fn continuation_alias_requires_paired_receipt_and_scoped_child() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE jobs(id INTEGER PRIMARY KEY,parent INTEGER,ticket INTEGER,msgkey TEXT,source_turn INTEGER);
          CREATE TABLE events(seq INTEGER PRIMARY KEY,ticket INTEGER,job INTEGER,turn INTEGER,kind TEXT,payload TEXT);
          CREATE TABLE mailbox(seq INTEGER,target INTEGER,source INTEGER,source_turn INTEGER,msgkey TEXT);
          INSERT INTO jobs VALUES(11,1,2,'impl',1),(12,1,2,'other',2),
            (13,9,2,'nonchild',2),(14,1,9,'foreign',2),(9,1,2,'old',1);").unwrap();
        // Physical turn 7 can replay logical turn 3. Only paired receipts
        // confer aliases, regardless of the child's original dispatch turn.
        for (seq, target, key, logical, paired) in [
            (1,11,"resume",3,true), (3,12,"resume",2,true),
            (5,11,"note-only",3,false), (7,13,"not-child",3,true),
            (9,14,"wrong-ticket",3,true), (11,9,"before-floor",3,true),
            (13,11,"collision",3,true),
        ] {
            conn.execute("INSERT INTO events VALUES(?,2,1,7,'note',?)",
                rusqlite::params![seq, stringify(&json!({"job":target,"id":key,"body":"continue"}))]).unwrap();
            conn.execute("INSERT INTO mailbox VALUES(?,?,1,?,?)", rusqlite::params![seq,target,logical,key]).unwrap();
            if paired {
                conn.execute("INSERT INTO events VALUES(?,2,1,7,'continued',?)",
                    rusqlite::params![seq+1,stringify(&json!({"job":target,"key":key}))]).unwrap();
            }
        }
        conn.execute_batch("INSERT INTO jobs VALUES(15,1,2,'collision',3);
          INSERT INTO events VALUES(20,2,1,7,'continued','{\"job\":11,\"key\":\"orphan\"}');
          INSERT INTO events VALUES(21,9,1,7,'note','{}'),
            (22,9,1,7,'continued','{\"job\":11,\"key\":\"foreign-receipt\"}');
          INSERT INTO mailbox VALUES(21,11,1,3,'foreign-receipt');").unwrap();
        let q = Q(&conn);
        let job = crate::job::JobCtx::from_row(json!({"id":1,"ticket":2,"floor":10,"turn":3}).as_object().unwrap());
        assert_eq!(dependencies(&q,&job,Some(&json!(["resume"])),0).unwrap(),vec![11]);
        assert_eq!(dependencies(&q,&job,Some(&json!(["resume"])),11).unwrap(),vec![12]);
        for key in ["note-only","not-child","wrong-ticket","before-floor","collision","orphan","foreign-receipt"] {
            assert!(dependencies(&q,&job,Some(&json!([key])),0).is_err(),"{key}");
        }
    }
}
