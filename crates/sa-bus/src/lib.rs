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
const ROSTER_QUERY: &str = "WITH roster AS (
    SELECT id,parent,agent,coalesce(nullif(assigned_role,''),role,'') AS role,state,depth
    FROM jobs WHERE ticket=? ORDER BY id DESC LIMIT ?
), latest AS (
    SELECT job,max(seq) AS seq FROM events
    WHERE ticket=? AND kind='job.progress' AND job IN (SELECT id FROM roster)
    GROUP BY job
)
SELECT r.*,e.payload AS progress FROM roster r
LEFT JOIN latest p ON p.job=r.id LEFT JOIN events e ON e.seq=p.seq
ORDER BY r.id";
/// `ask` waits below the 60 s default MCP tool timeout of Codex and DSH.
const ASK_DEFAULT_SECS: u64 = 45;
const ASK_MAX_SECS: u64 = 55;
/// Paths one task may hold at once.
const MAX_CLAIMS: usize = 64;

const INSTRUCTIONS: &str = "Super Agent collaboration bus for the current ticket. Use it while you work instead of waiting for turn-end reports: \
call `roster` to see who else works on this ticket, `send` to message a collaborator (or `lead`/`parent`) when you need input or have news they depend on, \
`inbox` to read messages addressed to you, and `publish` to share an intermediate result (commit, artifact, test numbers) as soon as it exists. \
Use `advise` with a self-contained brief to get a second opinion from the ticket's advisor model (another harness) before committing to an approach, when stuck, or before declaring the work done. \
Use `ask` to put a question to one task and wait for its `reply` within your turn. \
Before editing shared files, `claim` them (and `release` when done) so two tasks do not overwrite each other. \
`tools` lists the MCP tools the operator enabled for this ticket and `call` runs one of them; name a server and tool in `tools` to read its full input schema first. \
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
    /// Ticket MCP tools proxied by `tools`/`call`; `None` hides them.
    ext: Option<sa_ext::ProjectExtensions>,
    /// Asking job → asked job while an `ask` waits; used to refuse cycles.
    waiting: Mutex<HashMap<i64, i64>>,
    /// Advisory edit claims of running turns, released when the turn ends.
    claims: Mutex<Vec<Claim>>,
    /// Runs `advise` turns; set once the engine exists.
    consultant: std::sync::OnceLock<Arc<dyn sa_engine::services::Consultant>>,
}

#[derive(Clone, Debug)]
struct Claim {
    ticket: i64,
    job: i64,
    agent: String,
    /// Normalized project-relative path; empty means the whole project.
    path: String,
}

/// The running bus. Dropping it stops the listener.
pub struct CollabBus {
    shared: Arc<Shared>,
    url: String,
    cancel: CancellationToken,
}

impl CollabBus {
    /// Bind `127.0.0.1:<random>` and serve. Requires a Tokio runtime.
    pub async fn start(data_dir: &Path, ext: Option<sa_ext::ProjectExtensions>) -> Result<Arc<Self>> {
        let shared = Arc::new(Shared {
            db: Db::open(data_dir).context("协作总线无法打开工单库")?,
            grants: Mutex::default(),
            ext,
            waiting: Mutex::default(),
            claims: Mutex::default(),
            consultant: std::sync::OnceLock::new(),
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

    /// Enable `advise`. The engine is created after the bus, so it is wired here.
    pub fn set_consultant(&self, consultant: Arc<dyn sa_engine::services::Consultant>) {
        let _ = self.shared.consultant.set(consultant);
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
        let ended = self.lock().remove(token);
        if let Some(ended) = ended {
            let still = self.lock().values().any(|g| g.job == ended.job);
            if !still {
                self.shared.claims.lock().unwrap_or_else(|e| e.into_inner()).retain(|c| c.job != ended.job);
            }
        }
    }
}

#[derive(Clone)]
struct Handler(Arc<Shared>);

impl Handler {
    /// `advise`: one read-only advisor turn on the ticket's advisor route.
    async fn advise(&self, caller: &Grantee, args: &Value, cancel: &CancellationToken) -> Result<Value> {
        let consultant = self.0.consultant.get().cloned().ok_or_else(|| anyhow!("本工作台未启用顾问"))?;
        let brief = args["brief"].as_str().unwrap_or_default().to_string();
        let mut out = consultant.consult(caller.ticket, caller.job, caller.turn, brief, cancel.clone()).await?;
        out["unread"] = json!(self.0.db.with(|q| unread(q, caller))?);
        Ok(out)
    }

    fn waiting(&self) -> std::sync::MutexGuard<'_, HashMap<i64, i64>> {
        self.0.waiting.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `ask`: send a question, then poll the journal for its reply. The wait
    /// is registered so a reverse question cannot deadlock both turns.
    async fn ask(&self, caller: &Grantee, args: &Value, cancel: &CancellationToken) -> Result<Value> {
        let wait = args["wait_seconds"].as_u64().unwrap_or(ASK_DEFAULT_SECS).clamp(1, ASK_MAX_SECS);
        let question = format!("q{}", uuid::Uuid::new_v4().simple());
        let dest = {
            let dest = self.0.db.tx(|q| {
                ensure!(q.live(caller.who())?, "本轮已结束，协作总线不再接受该任务的调用");
                target(q, caller, &args["to"])
            })?;
            let mut waiting = self.waiting();
            let mut cursor = dest;
            while let Some(&next) = waiting.get(&cursor) {
                ensure!(next != caller.job, "任务 #{dest} 正在等待你的回答，互相等待会卡住双方；请先 `reply` 或改用 `send`");
                cursor = next;
            }
            waiting.insert(caller.job, dest);
            dest
        };
        let outcome = async {
            let text = args["question"].as_str().unwrap_or_default();
            ensure!(p::valid_text(args.get("question"), MAX_BODY), "问题为空或超过 {MAX_BODY} 个字符");
            let body = format!("Question {question} (answer with sa_bus `reply`): {text}");
            self.0.db.tx(|q| {
                let source = NoteSource { who: caller.who(), agent: caller.agent.clone(), logical: caller.turn };
                let key = format!("bus-{question}");
                let seq = q.record_note_within(caller.ticket, Some(&json!(dest)), Some(&json!(body)), Some(&source), Some(&key), BUDGET)?;
                ensure!(
                    q.exists("SELECT 1 FROM mailbox WHERE seq=? AND target IS NOT NULL", &[&seq])?,
                    "目标任务不属于本工单、已停止收信，或协作消息达到预算"
                );
                Ok(())
            })?;
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(wait);
            let reply_key = format!("bus-r-{question}");
            loop {
                let answer = self.0.db.tx(|q| {
                    let row = q.one(
                        "SELECT m.seq,e.payload FROM mailbox m JOIN events e ON e.seq=m.seq WHERE m.target=? AND m.msgkey=?",
                        &[&caller.job, &reply_key],
                    )?;
                    let Some(row) = row else { return Ok(None) };
                    let seq = int_of(&row, "seq");
                    q.accept_via(caller.who(), &[seq], "bus")?;
                    Ok(Some(serde_json::from_str::<Value>(str_of(&row, "payload"))?))
                })?;
                if let Some(payload) = answer {
                    return Ok(json!({"answered": true, "question_id": question, "from": payload["sender"], "answer": payload["body"]}));
                }
                if tokio::time::Instant::now() >= deadline {
                    return Ok(json!({"answered": false, "question_id": question, "detail": "等待超时；回答到达后会进入你的收件箱"}));
                }
                tokio::select! {
                    () = cancel.cancelled() => return Err(anyhow!("调用已取消")),
                    () = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
                }
            }
        }
        .await;
        self.waiting().remove(&caller.job);
        let mut out = outcome?;
        out["unread"] = json!(self.0.db.with(|q| unread(q, caller))?);
        Ok(out)
    }

    /// `tools`/`call`: ticket MCP tools, outside the journal transaction
    /// because a call awaits the remote server.
    async fn extension(&self, caller: &Grantee, name: &str, args: &Value, cancel: &CancellationToken) -> Result<Value> {
        ensure!(self.0.db.with(|q| q.live(caller.who()))?, "本轮已结束，协作总线不再接受该任务的调用");
        let ext = self.0.ext.as_ref().ok_or_else(|| anyhow!("本工作台未启用 MCP 与技能服务"))?;
        let direct = sa_ext::DirectCaller { ticket: caller.ticket, job: caller.job, turn: caller.turn };
        let text = |key: &str| args.get(key).and_then(Value::as_str);
        let mut out = if name == "tools" {
            ext.catalog_for(&direct, text("server"), text("tool"))?
        } else {
            let (Some(server), Some(tool)) = (text("server"), text("tool")) else {
                return Err(anyhow!("`call` 需要 server 和 tool"));
            };
            let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
            json!({"result": ext.call_direct(&direct, server, tool, &arguments, cancel).await?})
        };
        out["unread"] = json!(self.0.db.with(|q| unread(q, caller))?);
        Ok(out)
    }

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
            "advise",
            "Consult the ticket's advisor: a stronger model on another harness that reads your brief, inspects the project read-only and returns guidance. Write a self-contained brief: the goal, what you have done and found, the decision or problem, and relevant file paths or evidence. Use it before committing to an approach, when an error keeps recurring, and before declaring the work done. It can take several minutes.",
            schema(json!({"type":"object","properties":{
                "brief":{"type":"string","description":"Self-contained brief for the advisor"}
            },"required":["brief"],"additionalProperties":false})),
        ),
        Tool::new(
            "ask",
            "Ask one task a question and wait for its answer within your turn (default 45 s, at most 55 s). The question reaches the other task like `send`; it answers with `reply`. If the wait ends first, the answer later arrives in your inbox. Asking a task that is itself waiting on you is refused.",
            schema(json!({"type":"object","properties":{
                "to":{"type":["integer","string"],"description":"Task id, `lead` or `parent`"},
                "question":{"type":"string"},
                "wait_seconds":{"type":"integer","minimum":1,"maximum":55}
            },"required":["to","question"],"additionalProperties":false})),
        ),
        Tool::new(
            "reply",
            "Answer a question you received from `ask`, using the question id shown in that message.",
            schema(json!({"type":"object","properties":{
                "question_id":{"type":"string"},
                "body":{"type":"string"}
            },"required":["question_id","body"],"additionalProperties":false})),
        ),
        Tool::new(
            "inbox",
            "Read and acknowledge the messages addressed to your task that you have not read yet.",
            schema(json!({"type":"object","properties":{},"additionalProperties":false})),
        ),
        Tool::new(
            "claim",
            "Declare the files or directories you are about to edit (project-relative or absolute paths; a directory covers everything below it). Paths that overlap another running task's claim are not taken; the result names the holder, who is notified. Claims end with `release` or when your turn ends.",
            schema(json!({"type":"object","properties":{
                "paths":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":64}
            },"required":["paths"],"additionalProperties":false})),
        ),
        Tool::new(
            "release",
            "Release your claims: the listed paths, or all of them when `paths` is omitted.",
            schema(json!({"type":"object","properties":{
                "paths":{"type":"array","items":{"type":"string"}}
            },"additionalProperties":false})),
        ),
        Tool::new(
            "tools",
            "List the MCP tools the operator enabled for this ticket (Super Agent extensions), with description previews. Pass `server` and `tool` to get that tool's full description and input schema before calling it.",
            schema(json!({"type":"object","properties":{
                "server":{"type":"string","description":"Server id from a previous listing"},
                "tool":{"type":"string","description":"Tool name from a previous listing"}
            },"additionalProperties":false})),
        ),
        Tool::new(
            "call",
            "Call one MCP tool from `tools` and return its result within your turn. Available on full-access and auto tasks.",
            schema(json!({"type":"object","properties":{
                "server":{"type":"string"},
                "tool":{"type":"string"},
                "arguments":{"type":"object","description":"Arguments matching the tool's input schema"}
            },"required":["server","tool"],"additionalProperties":false})),
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
        let outcome = match request.name.as_ref() {
            "tools" | "call" => self.extension(&caller, &request.name, &args, &context.ct).await,
            "ask" => self.ask(&caller, &args, &context.ct).await,
            "advise" => self.advise(&caller, &args, &context.ct).await,
            "roster" => {
                let mut claims = self.0.claims.lock().unwrap_or_else(|e| e.into_inner());
                self.0.db.with(|q| {
                    let snapshot = q.conn().unchecked_transaction()?;
                    let roster = claims_call(&Q(&snapshot), &caller, "roster", &args, &mut claims)?;
                    snapshot.commit()?;
                    Ok(roster)
                })
            }
            "claim" | "release" => {
                let mut claims = self.0.claims.lock().unwrap_or_else(|e| e.into_inner());
                self.0.db.tx(|q| claims_call(q, &caller, &request.name, &args, &mut claims))
            }
            name => self.0.db.tx(|q| call(q, &caller, name, &args)),
        };
        let result = match outcome {
            Ok(value) => CallToolResult::success(vec![ContentBlock::text(value.to_string())]),
            Err(error) => CallToolResult::error(vec![ContentBlock::text(error.to_string())]),
        };
        Ok(result.into())
    }
}

/// Comparison key: Windows paths are case-insensitive.
fn key(path: &str) -> String {
    if cfg!(windows) { path.to_lowercase() } else { path.to_string() }
}

/// Project-relative, `/`-separated path in its original case.
fn normalize(project: &str, raw: &str) -> Result<String> {
    let project = project.trim().replace('\\', "/").trim_end_matches('/').to_string();
    let mut path = raw.trim().replace('\\', "/");
    let (pk, k) = (key(&project), key(&path));
    if !project.is_empty() && (k == pk || k.starts_with(&format!("{pk}/"))) {
        path = path[project.len()..].to_string();
    }
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    ensure!(!parts.contains(&".."), "声明路径不能包含 ..：{raw}");
    ensure!(!path.contains(':') || parts.is_empty(), "声明路径不在工单项目内：{raw}");
    Ok(parts.join("/"))
}

fn overlaps(a: &str, b: &str) -> bool {
    let (a, b) = (key(a), key(b));
    let (a, b) = (a.as_str(), b.as_str());
    a.is_empty() || b.is_empty() || a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

/// `claim`, `release` and `roster` (which lists live claims).
fn claims_call(q: &Q, caller: &Grantee, tool: &str, args: &Value, claims: &mut Vec<Claim>) -> Result<Value> {
    if tool == "roster" {
        let mut out = call(q, caller, "roster", args)?;
        for task in out["tasks"].as_array_mut().into_iter().flatten() {
            let held: Vec<&str> = claims
                .iter()
                .filter(|c| c.ticket == caller.ticket && task["id"].as_i64() == Some(c.job))
                .map(|c| if c.path.is_empty() { "." } else { c.path.as_str() })
                .collect();
            task["claims"] = json!(held);
        }
        return Ok(out);
    }
    ensure!(q.live(caller.who())?, "本轮已结束，协作总线不再接受该任务的调用");
    let project = q.one("SELECT cwd FROM tickets WHERE id=?", &[&caller.ticket])?.map(|r| str_of(&r, "cwd").to_string()).unwrap_or_default();
    let requested: Vec<String> = match args.get("paths") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|p| p.as_str().ok_or_else(|| anyhow!("`paths` 必须是字符串数组")).and_then(|p| normalize(&project, p)))
            .collect::<Result<_>>()?,
        None if tool == "release" => vec![],
        _ => return Err(anyhow!("`paths` 必须是字符串数组")),
    };
    let mut out = if tool == "release" {
        let before = claims.len();
        claims.retain(|c| c.job != caller.job || (!requested.is_empty() && !requested.iter().any(|r| key(r) == key(&c.path))));
        json!({"released": before - claims.len()})
    } else {
        ensure!(!requested.is_empty(), "`paths` 不能为空");
        let mut taken = vec![];
        let mut conflicts = vec![];
        for path in requested {
            let holder = claims
                .iter()
                .find(|c| c.ticket == caller.ticket && c.job != caller.job && overlaps(&c.path, &path))
                .cloned();
            match holder {
                Some(holder) => {
                    let body = format!(
                        "Task #{} ({}) also needs to edit `{}`, which overlaps your claim `{}`. Coordinate with it before either of you changes these files.",
                        caller.job, caller.agent, if path.is_empty() { "." } else { &path }, if holder.path.is_empty() { "." } else { &holder.path }
                    );
                    let source = NoteSource { who: caller.who(), agent: caller.agent.clone(), logical: caller.turn };
                    let key = format!("bus-{}", uuid::Uuid::new_v4().simple());
                    q.record_note_within(caller.ticket, Some(&json!(holder.job)), Some(&json!(body)), Some(&source), Some(&key), BUDGET)?;
                    conflicts.push(json!({"path": path, "held_by": holder.job, "agent": holder.agent, "claim": holder.path}));
                }
                None => {
                    if !claims.iter().any(|c| c.job == caller.job && key(&c.path) == key(&path)) {
                        ensure!(claims.iter().filter(|c| c.job == caller.job).count() < MAX_CLAIMS, "声明路径超过 {MAX_CLAIMS} 个");
                        claims.push(Claim { ticket: caller.ticket, job: caller.job, agent: caller.agent.clone(), path: path.clone() });
                    }
                    taken.push(path);
                }
            }
        }
        q.log(caller.who(), "bus.claim", &json!({"claimed": taken, "conflicts": conflicts}))?;
        json!({"claimed": taken, "conflicts": conflicts})
    };
    out["unread"] = json!(unread(q, caller)?);
    Ok(out)
}

/// One tool call inside a journal transaction. Errors are caller-visible.
pub fn call(q: &Q, caller: &Grantee, tool: &str, args: &Value) -> Result<Value> {
    ensure!(q.live(caller.who())?, "本轮已结束，协作总线不再接受该任务的调用");
    let mut out = match tool {
        "roster" => roster(q, caller)?,
        "send" => send(q, caller, args)?,
        "inbox" => inbox(q, caller)?,
        "reply" => reply(q, caller, args)?,
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
        ROSTER_QUERY,
        &[&caller.ticket, &ROSTER_LIMIT, &caller.ticket],
    )?;
    let mut tasks = vec![];
    for row in &rows {
        let id = int_of(row, "id");
        let progress = serde_json::from_str::<Value>(str_of(row, "progress")).ok();
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

/// Answer an `ask`: the reply goes to the asker under a key its wait loop
/// looks for, so it is delivered once even if the asker stopped waiting.
fn reply(q: &Q, caller: &Grantee, args: &Value) -> Result<Value> {
    let question = args["question_id"].as_str().unwrap_or_default();
    ensure!(
        question.starts_with('q') && question.len() <= 64 && question.chars().all(|c| c.is_ascii_alphanumeric()),
        "无效的问题编号"
    );
    ensure!(p::valid_text(args.get("body"), MAX_BODY), "回答为空或超过 {MAX_BODY} 个字符");
    let asked = q
        .one(
            "SELECT source FROM mailbox WHERE target=? AND msgkey=?",
            &[&caller.job, &format!("bus-{question}")],
        )?
        .ok_or_else(|| anyhow!("没有发给你的这个问题"))?;
    let asker = int_of(&asked, "source");
    let source = NoteSource { who: caller.who(), agent: caller.agent.clone(), logical: caller.turn };
    let seq = q.record_note_within(
        caller.ticket,
        Some(&json!(asker)),
        args.get("body"),
        Some(&source),
        Some(&format!("bus-r-{question}")),
        BUDGET,
    )?;
    let delivered = q.exists("SELECT 1 FROM mailbox WHERE seq=? AND target IS NOT NULL", &[&seq])?;
    ensure!(delivered, "提问的任务已停止收信，或协作消息达到预算");
    Ok(json!({"replied": true, "to": asker, "seq": seq}))
}

fn inbox(q: &Q, caller: &Grantee) -> Result<Value> {
    let notes = q.notes(caller.job, true)?;
    let seqs: Vec<i64> = notes.iter().filter_map(|n| n["seq"].as_i64()).collect();
    q.accept_via(caller.who(), &seqs, "bus")?;
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
