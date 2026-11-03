//! Streamed and final control commands (port of `runtime/engine-dispatch.cjs`).

use crate::adapter::AGENTS;
use crate::db::{NoteSource, Q, str_of};
use crate::engine::Turn;
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
    Other(anyhow::Error),
}

impl CmdError {
    pub fn message(&self) -> String {
        match self {
            Self::Syntax(m) => m.clone(),
            Self::Other(e) => e.to_string(),
        }
    }

    pub fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Syntax(m) => anyhow!(m),
            Self::Other(e) => e,
        }
    }
}

impl From<anyhow::Error> for CmdError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}

/// Control, line/paragraph separator and bidi-override characters (D:152).
static UNSAFE_SUMMARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[\p{Cc}\p{Zl}\p{Zp}\x{202a}-\x{202e}\x{2066}-\x{2069}]").expect("valid regex")
});

/// `command(store, ctx, line)` (D:177-229).
pub async fn command(turn: &Arc<Turn>, line: &str) -> Result<(), CmdError> {
    if turn.aborted() {
        return Ok(());
    }
    let Some(sep) = line.find(' ') else {
        return Ok(());
    };
    let (tag, rest) = (&line[..sep], &line[sep + 1..]);
    if tag == "SA_PROGRESS" {
        return progress(turn, rest).map_err(CmdError::Other);
    }
    if !matches!(tag, "SA_ROLE" | "SA_SEND" | "SA_NOTE" | "SA_TOOL") {
        return Ok(());
    }
    let cmd: Value = serde_json::from_str(rest).map_err(|e| CmdError::Syntax(e.to_string()))?;
    match tag {
        "SA_SEND" => dispatch(turn, &cmd).await.map_err(CmdError::Other),
        "SA_TOOL" => tool(turn, &cmd).await.map_err(CmdError::Other),
        _ => {
            let job = &turn.job;
            turn.inner().db.tx(|q| {
                if !q.live(job.who())? {
                    return Ok(());
                }
                if tag == "SA_ROLE" {
                    if !p::valid_text(cmd.get("role"), 120) {
                        return Err(anyhow!("无效的职能名称"));
                    }
                    let role = cmd["role"].as_str().unwrap_or_default();
                    if q.run("UPDATE jobs SET role=? WHERE id=? AND role!=?", &[&role, &job.id, &role])? > 0 {
                        q.log(job.who(), "role", &json!({"role": role}))?;
                    }
                } else {
                    if !p::valid_text(cmd.get("id"), 96) {
                        return Err(anyhow!("无效的信件 ID"));
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
    if !turn.live() || turn.aborted() {
        return Ok(());
    }
    let ext = inner.extensions().filter(|e| e.can_invoke());
    let Some(ext) = ext.filter(|_| job.parent.is_none() && !matches!(job.profile.as_str(), "readonly" | "legacy"))
    else {
        return Err(anyhow!("当前工单或执行者不可调用 MCP 工具"));
    };
    if !p::is_obj(Some(cmd))
        || !p::valid_text(cmd.get("id"), 96)
        || !p::ident(cmd.get("server"))
        || !p::ident(cmd.get("tool"))
        || !p::is_obj(cmd.get("arguments"))
    {
        return Err(anyhow!("SA_TOOL 需要 id、server、tool 和 arguments 对象"));
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
        return Err(anyhow!("工具不在本次工单启用的服务目录内"));
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
}

/// `check(store, ctx, cmd)` (D:5-68). `None` means an exact replay of an
/// already recorded send.
fn check(q: &Q, turn: &Turn, cmd: &Value) -> Result<Option<Child>> {
    let job = &turn.job;
    let Some(o) = cmd.as_object() else {
        return Err(anyhow!("SA_SEND 需要 JSON 对象"));
    };
    if ["provider", "mode", "selection", "route", "route_override", "permission_override"]
        .iter()
        .any(|k| o.contains_key(*k))
    {
        return Err(anyhow!("不可通过派工更换供应商或执行端"));
    }
    let bad = || anyhow!("派工参数无效");
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
    let has_profile = o.contains_key("permission_profile");
    if (has_model || has_profile) && (job.parent.is_some() || !job.allow_lead_routing) {
        return Err(anyhow!("只有获授权的主控可指定子任务模型和权限"));
    }
    let requested = o.get("permission_profile").and_then(Value::as_str);
    if has_profile && !requested.is_some_and(|r| p::child_profiles(&job.profile).contains(&r)) {
        return Err(anyhow!("子任务权限不能超出上级范围"));
    }
    if has_model && !p::ident(o.get("model")) {
        return Err(anyhow!("模型标识无效"));
    }
    let model = o.get("model").and_then(Value::as_str);
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
        let old_model = old
            .get("route_override")
            .and_then(Value::as_str)
            .and_then(|r| serde_json::from_str::<Value>(r).ok())
            .and_then(|r| r.get("model").and_then(Value::as_str).map(str::to_string));
        let old_override = old.get("permission_override").and_then(Value::as_str);
        if str_of(&old, "agent") != to
            || str_of(&old, "body") != body
            || (old["writing"].as_i64().unwrap_or(0) != 0) != write
            || str_of(&old, "assigned_role") != role
            || old_model.as_deref() != model
            || old_override != override_.as_deref()
            || stringify(&files_value) != stringify(&Value::Array(names))
        {
            return Err(anyhow!("同一消息 ID 携带不同的任务、模型、附件或权限"));
        }
        return Ok(None);
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
            return Err(anyhow!("所选模型不在此任务的供应商和执行端目录"));
        }
        let menu = menu.cloned().unwrap_or(Value::Null);
        let picked = json!({"provider": menu["provider"], "mode": menu["mode"], "model": model});
        let route = p::route(Some(&picked)).map_err(|e| anyhow!(e))?;
        let pinned: Value = serde_json::from_str(&job.agent_routes).unwrap_or(Value::Null);
        if let Some(pinned) = pinned.get(to).filter(|v| p::truthy_value(v))
            && (pinned.get("provider").and_then(Value::as_str) != Some(&route.provider)
                || pinned.get("mode").and_then(Value::as_str) != Some(&route.mode))
        {
            return Err(anyhow!("工单路由已变更"));
        }
        selection = Some(p::route_value(&route));
    }
    if write && (!job.writes || effective == "readonly" || !p::can_write(to, &effective)) {
        return Err(anyhow!("该子任务没有所请求的写权限"));
    }
    let files = files_list
        .iter()
        .map(|f| f.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| anyhow!("附件需要最多 16 个项目相对路径"))?;
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
    }))
}

/// `dispatch(store, ctx, cmd)` (D:70-124).
async fn dispatch(turn: &Arc<Turn>, cmd: &Value) -> Result<()> {
    let job = &turn.job;
    let inner = turn.inner();
    let superseded = |q: &Q| -> Result<bool> {
        Ok(job.continuous && job.parent.is_none() && q.followed_after(job.ticket, job.admission)?)
    };
    if turn.aborted() || !turn.live() || inner.db.with(|q| superseded(q))? {
        return Ok(());
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
