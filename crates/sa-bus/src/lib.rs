//! Collaboration bus (`docs/dev/collab-bus.md`): a loopback Streamable HTTP MCP
//! server that every harness of a ticket mounts. Each admitted turn receives a
//! bearer token bound to `(ticket, job, turn)`; the engine revokes it when the
//! turn ends. Messages reuse the journal mailbox and `job.progress` events, so
//! restart, deduplication, pause admission and the UI keep their behaviour.

use anyhow::{Context, Result, anyhow, ensure};
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use sa_engine::adapter::BusGrant;
use sa_engine::db::{Db, NoteSource, Q, Who, int_of, str_of};
use sa_engine::protocol as p;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// Longest message body, in UTF-16 units like the other protocol limits.
const MAX_BODY: usize = 16_384;
/// Automated mailbox budget per ticket for bus traffic (`SA_NOTE` keeps 128).
const BUDGET: i64 = 4_096;
const ROSTER_LIMIT: i64 = 100;

const INSTRUCTIONS: &str = "Super Agent collaboration bus for the current ticket. Use it while you work instead of waiting for turn-end reports: \
call `roster` to see who else works on this ticket, `send` to message a collaborator (or `lead`/`parent`) when you need input or have news they depend on, \
`inbox` to read messages addressed to you, and `publish` to share an intermediate result (commit, artifact, test numbers) as soon as it exists. \
Every result reports your unread count; read your inbox when it is not zero, before long operations and before you finish.";

/// Identity behind one bearer token.
#[derive(Clone, Debug)]
pub struct Grantee {
    pub ticket: i64,
    pub job: i64,
    pub turn: i64,
    pub agent: String,
}

impl Grantee {
    fn who(&self) -> Who {
        Who {
            ticket: self.ticket,
            id: self.job,
            turn: self.turn,
        }
    }
}

struct Shared {
    db: Db,
    grants: Mutex<HashMap<String, Grantee>>,
}

/// The running bus. Dropping it stops the listener.
pub struct CollabBus {
    shared: Arc<Shared>,
    url: String,
    cancel: CancellationToken,
}

impl CollabBus {
    /// Bind `127.0.0.1:<random>` and serve. Requires a Tokio runtime.
    pub async fn start(data_dir: &Path) -> Result<Arc<Self>> {
        let shared = Arc::new(Shared {
            db: Db::open(data_dir).context("协作总线无法打开工单库")?,
            grants: Mutex::default(),
        });
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let url = format!("http://127.0.0.1:{}/mcp", listener.local_addr()?.port());
        let cancel = CancellationToken::new();
        let handler = Handler(shared.clone());
        let service = StreamableHttpService::new(
            move || Ok(handler.clone()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default().with_cancellation_token(cancel.child_token()),
        );
        let stop = cancel.clone();
        tokio::spawn(async move {
            loop {
                let stream = tokio::select! {
                    _ = stop.cancelled() => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => stream,
                        Err(_) => continue,
                    },
                };
                let svc = TowerToHyperService::new(service.clone());
                tokio::spawn(async move {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });
        Ok(Arc::new(Self { shared, url, cancel }))
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Grantee>> {
        self.shared.grants.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for CollabBus {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl sa_engine::services::Bus for CollabBus {
    fn grant(&self, ticket: i64, job: i64, turn: i64, agent: &str) -> Option<BusGrant> {
        let token = format!("sab-{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let grantee = Grantee {
            ticket,
            job,
            turn,
            agent: agent.to_string(),
        };
        self.lock().insert(token.clone(), grantee);
        Some(BusGrant {
            url: self.url.clone(),
            token,
        })
    }

    fn revoke(&self, token: &str) {
        self.lock().remove(token);
    }
}

#[derive(Clone)]
struct Handler(Arc<Shared>);

impl Handler {
    fn caller(&self, context: &RequestContext<RoleServer>) -> Option<Grantee> {
        let parts = context.extensions.get::<http::request::Parts>()?;
        let header = parts.headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
        let token = header.strip_prefix("Bearer ")?.trim();
        let grants = self.0.grants.lock().unwrap_or_else(|e| e.into_inner());
        grants.get(token).cloned()
    }
}

fn schema(value: Value) -> Arc<Map<String, Value>> {
    match value {
        Value::Object(map) => Arc::new(map),
        _ => Arc::default(),
    }
}

/// Claude Code keeps tools marked this way out of deferred tool search.
fn always_loaded(mut tool: Tool) -> Tool {
    tool.meta = serde_json::from_value(json!({"anthropic/alwaysLoad": true})).ok();
    tool
}

pub fn tools() -> Vec<Tool> {
    [
        Tool::new(
            "roster",
            "List the tasks of this ticket: id, parent, harness, role, state and latest published progress. Marks your own task.",
            schema(json!({"type":"object","properties":{},"additionalProperties":false})),
        ),
        Tool::new(
            "send",
            "Send a message to another task of this ticket while you work. `to` is a task id from `roster`, or `lead` (the ticket's primary task) or `parent` (the task that assigned yours). Running Codex tasks receive it mid-turn; a waiting task is woken.",
            schema(json!({"type":"object","properties":{
                "to":{"type":["integer","string"],"description":"Task id, `lead` or `parent`"},
                "body":{"type":"string","description":"Message text"}
            },"required":["to","body"],"additionalProperties":false})),
        ),
        Tool::new(
            "inbox",
            "Read and acknowledge the messages addressed to your task that you have not read yet.",
            schema(json!({"type":"object","properties":{},"additionalProperties":false})),
        ),
        Tool::new(
            "publish",
            "Publish an intermediate result so the operator and collaborators see it now. Reusing an `id` within this turn with a different summary is rejected.",
            schema(json!({"type":"object","properties":{
                "id":{"type":"string","description":"Short stable identifier, e.g. `tests`"},
                "summary":{"type":"string","description":"One-line result, at most 320 characters"}
            },"required":["id","summary"],"additionalProperties":false})),
        ),
    ]
    .into_iter()
    .map(always_loaded)
    .collect()
}

impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some(caller) = self.caller(&context) else {
            return Err(ErrorData::invalid_request("协作总线令牌无效或本轮已结束", None));
        };
        let args = Value::Object(request.arguments.unwrap_or_default());
        let outcome = self.0.db.tx(|q| call(q, &caller, &request.name, &args));
        let result = match outcome {
            Ok(value) => CallToolResult::success(vec![ContentBlock::text(value.to_string())]),
            Err(error) => CallToolResult::error(vec![ContentBlock::text(error.to_string())]),
        };
        Ok(result.into())
    }
}

/// One tool call inside a journal transaction. Errors are caller-visible.
pub fn call(q: &Q, caller: &Grantee, tool: &str, args: &Value) -> Result<Value> {
    ensure!(q.live(caller.who())?, "本轮已结束，协作总线不再接受该任务的调用");
    let mut out = match tool {
        "roster" => roster(q, caller)?,
        "send" => send(q, caller, args)?,
        "inbox" => inbox(q, caller)?,
        "publish" => publish(q, caller, args)?,
        other => return Err(anyhow!("未知的协作总线工具：{other}")),
    };
    out["unread"] = json!(unread(q, caller)?);
    Ok(out)
}

fn unread(q: &Q, caller: &Grantee) -> Result<i64> {
    q.int(
        "SELECT count(*) AS n FROM mailbox WHERE target=? AND consumed_turn IS NULL",
        &[&caller.job],
    )
}

fn roster(q: &Q, caller: &Grantee) -> Result<Value> {
    let rows = q.all(
        "SELECT id,parent,agent,coalesce(nullif(assigned_role,''),role,'') AS role,state,depth FROM jobs
         WHERE ticket=? ORDER BY id DESC LIMIT ?",
        &[&caller.ticket, &ROSTER_LIMIT],
    )?;
    let mut tasks = vec![];
    for row in rows.iter().rev() {
        let id = int_of(row, "id");
        let progress = q
            .one(
                "SELECT payload FROM events WHERE job=? AND kind='job.progress' ORDER BY seq DESC LIMIT 1",
                &[&id],
            )?
            .and_then(|r| serde_json::from_str::<Value>(str_of(&r, "payload")).ok());
        tasks.push(json!({
            "id": id,
            "parent": row.get("parent").cloned().unwrap_or(Value::Null),
            "harness": str_of(row, "agent"),
            "role": str_of(row, "role"),
            "state": str_of(row, "state"),
            "depth": int_of(row, "depth"),
            "latest_progress": progress.and_then(|p| p.get("summary").cloned()),
            "you": id == caller.job,
        }));
    }
    Ok(json!({"ticket": caller.ticket, "tasks": tasks}))
}

fn target(q: &Q, caller: &Grantee, to: &Value) -> Result<i64> {
    let id = match to {
        Value::String(s) if s == "lead" => int_of(&q.root(caller.ticket)?, "id"),
        Value::String(s) if s == "parent" => q
            .one("SELECT parent FROM jobs WHERE id=?", &[&caller.job])?
            .and_then(|r| r.get("parent").and_then(Value::as_i64))
            .ok_or_else(|| anyhow!("当前任务没有上级任务"))?,
        Value::String(s) => s.trim().parse().map_err(|_| anyhow!("`to` 必须是任务编号、lead 或 parent"))?,
        Value::Number(n) => n.as_i64().ok_or_else(|| anyhow!("无效的任务编号"))?,
        _ => return Err(anyhow!("`to` 必须是任务编号、lead 或 parent")),
    };
    ensure!(id != caller.job, "不能给自己发消息");
    Ok(id)
}

fn send(q: &Q, caller: &Grantee, args: &Value) -> Result<Value> {
    let dest = target(q, caller, &args["to"])?;
    ensure!(p::valid_text(args.get("body"), MAX_BODY), "消息正文为空或超过 {MAX_BODY} 个字符");
    let source = NoteSource {
        who: caller.who(),
        agent: caller.agent.clone(),
        logical: caller.turn,
    };
    let key = format!("bus-{}", uuid::Uuid::new_v4().simple());
    let seq = q.record_note_within(
        caller.ticket,
        Some(&json!(dest)),
        args.get("body"),
        Some(&source),
        Some(&key),
        BUDGET,
    )?;
    let delivered = q.exists("SELECT 1 FROM mailbox WHERE seq=? AND target IS NOT NULL", &[&seq])?;
    ensure!(delivered, "目标任务不属于本工单、已停止收信，或协作消息达到预算");
    Ok(json!({"sent": true, "to": dest, "seq": seq}))
}

fn inbox(q: &Q, caller: &Grantee) -> Result<Value> {
    let notes = q.notes(caller.job, true)?;
    let seqs: Vec<i64> = notes.iter().filter_map(|n| n["seq"].as_i64()).collect();
    q.accept(caller.who(), &seqs)?;
    let messages: Vec<Value> = notes
        .iter()
        .map(|n| json!({"seq": n["seq"], "from": n["sender"], "body": n["body"]}))
        .collect();
    Ok(json!({"messages": messages}))
}

fn publish(q: &Q, caller: &Grantee, args: &Value) -> Result<Value> {
    ensure!(
        p::ident(args.get("id")) && args["id"].as_str().is_some_and(|s| s.encode_utf16().count() <= 96),
        "`id` 必须是不超过 96 个字符的单行标识"
    );
    ensure!(
        p::valid_text(args.get("summary"), 320)
            && !args["summary"].as_str().is_some_and(|s| s.contains(['\n', '\r'])),
        "`summary` 必须是不超过 320 个字符的单行文本"
    );
    let (id, summary) = (args["id"].as_str().unwrap_or_default(), args["summary"].as_str().unwrap_or_default());
    let old = q.one(
        "SELECT e.payload FROM progress_receipts r JOIN events e ON e.seq=r.seq WHERE r.job=? AND r.source_turn=? AND r.msgkey=?",
        &[&caller.job, &caller.turn, &id],
    )?;
    if let Some(old) = old {
        let payload: Value = serde_json::from_str(str_of(&old, "payload"))?;
        ensure!(payload["summary"].as_str() == Some(summary), "同一轮内该 id 已发布过不同内容");
        return Ok(json!({"published": true, "duplicate": true}));
    }
    let seq = q.log(caller.who(), "job.progress", &json!({"id": id, "summary": summary, "via": "bus"}))?;
    q.run(
        "INSERT INTO progress_receipts(job,source_turn,msgkey,seq) VALUES(?,?,?,?)",
        &[&caller.job, &caller.turn, &id, &seq],
    )?;
    Ok(json!({"published": true, "seq": seq}))
}

#[cfg(test)]
mod tests;
