//! Codex app-server adapter (port of `runtime/adapters/codex.cjs`, spec §7.3).
//!
//! JSON-RPC over `codex app-server --listen stdio://`: initialize, start or
//! resume the thread, start the turn, stream items, answer server requests,
//! steer same-ticket notes into the live turn, and return the last
//! non-commentary agent message.

use crate::codex_config::{Discovery, discover, routing};
use crate::common::{Emit, denied, probe_failed, protocol, route_gone, select, session_mismatch};
use crate::custom::{self, CustomProvider, Registry};
use crate::proc::{Lines, Proc, cancelled, capture, start};
use crate::sys::Sys;
use futures::future::BoxFuture;
use sa_engine::adapter::{Adapter, AdapterError, Catalog, ErrorClass, EventSink, RunOutcome, RunRequest, TurnHost};
use sa_engine::resilience::classify::{classified, codex_error};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;

const SETUP_TIMEOUT: Duration = Duration::from_secs(90);
const INTERRUPT_GRACE: Duration = Duration::from_millis(2400);
const STEER_POLL: Duration = Duration::from_millis(400);
const MAX_STEER_BYTES: usize = 65_536;

pub struct Codex {
    sys: Sys,
    registry: Registry,
}

impl Codex {
    pub fn new(sys: Sys) -> Self {
        Self::with_registry(sys, custom::registry(vec![]))
    }

    pub fn with_registry(sys: Sys, registry: Registry) -> Self {
        Self { sys, registry }
    }
}

struct Policy {
    sandbox: &'static str,
    review: &'static str,
    native: bool,
}

/// `policy(profile, writable, approve)` (CX:4-20).
fn policy(profile: &str, writable: bool) -> Result<Policy, AdapterError> {
    if !matches!(profile, "legacy" | "readonly" | "ask" | "partial" | "auto" | "full") {
        return Err(denied("未知 Codex 权限档位"));
    }
    if matches!(profile, "full" | "auto") {
        if !writable {
            return Err(denied("完全访问与只读授权冲突"));
        }
        return Ok(Policy {
            sandbox: "danger-full-access",
            review: "never",
            native: true,
        });
    }
    if profile == "readonly" && writable {
        return Err(denied("只读档不允许项目写入"));
    }
    let interactive = writable && matches!(profile, "ask" | "partial");
    Ok(Policy {
        sandbox: if writable && profile != "ask" { "workspace-write" } else { "read-only" },
        review: if interactive { "on-request" } else { "never" },
        native: false,
    })
}

fn policy_type(sandbox: &str) -> &'static str {
    match sandbox {
        "read-only" => "readOnly",
        "workspace-write" => "workspaceWrite",
        _ => "dangerFullAccess",
    }
}

fn request_title(method: &str) -> Option<&'static str> {
    match method {
        "item/commandExecution/requestApproval" => Some("执行命令或访问网络"),
        "item/fileChange/requestApproval" => Some("修改文件"),
        "item/permissions/requestApproval" => Some("扩展本轮权限"),
        _ => None,
    }
}

/// `reviewable(method, p, fileItem)` (CX:31-41).
fn reviewable(method: &str, p: &Value, file_item: Option<&Value>) -> bool {
    match method {
        "item/commandExecution/requestApproval" => {
            p["command"].as_str().is_some_and(|c| !c.is_empty())
                || p["networkApprovalContext"]["host"].as_str().is_some_and(|h| !h.is_empty())
        }
        "item/fileChange/requestApproval" => file_item
            .and_then(|f| f["changes"].as_array())
            .is_some_and(|c| !c.is_empty()),
        _ => p["permissions"]
            .as_object()
            .is_some_and(|m| !m.is_empty() && m.keys().all(|k| k == "fileSystem" || k == "network")),
    }
}

/// `decision(method, p, allowed)` (CX:42-46).
fn decision(method: &str, p: &Value, allowed: bool) -> Value {
    if method == "item/permissions/requestApproval" {
        json!({"permissions": if allowed { p["permissions"].clone() } else { json!({}) }, "scope": "turn"})
    } else {
        json!({"decision": if allowed { "accept" } else { "decline" }})
    }
}

/// `appReply(method, p, yes)` (CX:47-73).
fn app_reply(method: &str, p: &Value, yes: bool) -> Value {
    if method == "mcpServer/elicitation/request" {
        let schema = &p["requestedSchema"];
        let once = yes
            && p["mode"] == json!("form")
            && p["_meta"]["codex_approval_kind"] == json!("mcp_tool_call")
            && !p["_meta"]["codex_requires_user_input"].as_bool().unwrap_or(false)
            && schema["type"] == json!("object")
            && schema.as_object().is_some_and(|s| s.len() == 2)
            && schema["properties"].as_object().is_some_and(|p| p.is_empty());
        return if once {
            json!({"action": "accept", "content": {}})
        } else {
            json!({"action": "decline", "content": null})
        };
    }
    let q = &p["questions"][0];
    let labels: HashSet<&str> = q["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| o["label"].as_str())
        .collect();
    let item = p["itemId"].as_str().unwrap_or_default();
    let once = yes
        && p["questions"].as_array().is_some_and(|a| a.len() == 1)
        && q["id"].as_str() == Some(&format!("mcp_tool_call_approval_{item}"))
        && q["header"] == json!("Approve app tool call?")
        && q["isSecret"] == json!(false)
        && q["isOther"] == json!(false)
        && labels.contains("Allow")
        && labels.contains("Cancel");
    if once {
        let id = q["id"].as_str().unwrap_or_default();
        json!({"answers": {id: {"answers": ["Allow"]}}})
    } else {
        json!({"answers": {}})
    }
}

/// `rpcError(e)` (CX:74-80) refined by the structured classifier.
fn rpc_error(error: &Value) -> AdapterError {
    let mut e = codex_error(error);
    let message = error["message"].as_str().unwrap_or_default();
    let status = regex::Regex::new(r"\b(401|403|429|500|502|503|504)\b")
        .ok()
        .and_then(|r| r.captures(message).map(|c| c[1].to_string()));
    e.message = format!(
        "Codex 原生请求失败{}，请核对路由、权限或网络状态",
        status.map(|s| format!("（HTTP {s}）")).unwrap_or_default()
    );
    e
}

fn is_tool_item(item: &Value) -> bool {
    matches!(item["type"].as_str(), Some("commandExecution" | "mcpToolCall" | "fileChange"))
}

impl Adapter for Codex {
    fn agent(&self) -> &'static str {
        "codex"
    }

    fn catalog(&self) -> BoxFuture<'static, anyhow::Result<Catalog>> {
        let sys = self.sys.clone();
        let customs = custom::snapshot(&self.registry, "codex");
        Box::pin(async move {
            let mut d = discover(&sys, &CancellationToken::new()).await?;
            custom::merge(&mut d.catalog, &customs, &sys);
            Ok(Catalog { value: d.catalog })
        })
    }

    fn run(
        &self,
        request: RunRequest,
        host: Arc<dyn TurnHost>,
        events: EventSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<RunOutcome, AdapterError>> {
        let sys = self.sys.clone();
        let customs = custom::snapshot(&self.registry, "codex");
        Box::pin(async move { run(sys, customs, request, host, Emit(events), cancel).await })
    }
}

async fn run(
    sys: Sys,
    customs: Vec<CustomProvider>,
    req: RunRequest,
    host: Arc<dyn TurnHost>,
    emit: Emit,
    cancel: CancellationToken,
) -> Result<RunOutcome, AdapterError> {
    crate::images::check(&req)?;
    let perm = policy(&req.profile, req.writable)?;
    let mut d: Discovery = discover(&sys, &cancel).await.map_err(|e| probe_failed(&e.to_string()))?;
    custom::merge(&mut d.catalog, &customs, &sys);
    let s = select(&d.catalog, &req.route)?;
    let exe = d.exe.clone().ok_or_else(|| route_gone("本机未安装所选原生执行端"))?;
    let (mut args, settings) = match custom::lookup(&d.catalog, &customs, &s.provider) {
        // `-c` overrides name the key variable; Codex reads the key itself.
        Some(p) => custom::codex_routing(p, d.profile.is_some()),
        None => routing(&d, &s.provider).map_err(|e| route_gone(&e.to_string()))?,
    };
    if perm.review == "on-request" {
        for setting in [
            "network_access=false",
            "writable_roots=[]",
            "exclude_tmpdir_env_var=true",
            "exclude_slash_tmp=true",
        ] {
            args.push("-c".into());
            args.push(format!("sandbox_workspace_write.{setting}"));
        }
    }
    if !perm.native {
        for f in ["apps", "plugins", "hooks", "multi_agent"] {
            args.push("--disable".into());
            args.push(f.into());
        }
        let mut list = args.clone();
        list.extend(["mcp", "list", "--json"].map(String::from));
        let out = capture(&exe, &list, &req.cwd, &sys, "", Duration::from_secs(25), &cancel).await?;
        let servers: Value = serde_json::from_str(&out).map_err(|_| protocol("Codex MCP 列表无效"))?;
        for server in servers.as_array().into_iter().flatten() {
            let name = server["name"].as_str().unwrap_or_default();
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                return Err(denied("MCP 名称无法安全覆盖"));
            }
            args.push("-c".into());
            args.push(format!("mcp_servers.{name}.enabled=false"));
        }
    }
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    args.extend(["app-server", "--listen", "stdio://"].map(String::from));
    let proc = start(&exe, &args, &req.cwd, &sys)?;
    converse(proc, &req, &s, perm, settings, host, emit, cancel).await
}

/// Drive one JSON-RPC session over an already started app-server.
#[allow(clippy::too_many_arguments)]
async fn converse(
    mut proc: Proc,
    req: &RunRequest,
    s: &sa_engine::adapter::Route,
    perm: Policy,
    settings: Map<String, Value>,
    host: Arc<dyn TurnHost>,
    emit: Emit,
    cancel: CancellationToken,
) -> Result<RunOutcome, AdapterError> {
    let mut turn = Turn::new(req, s, perm, settings, host, emit, cancel, &mut proc);
    let result = turn.drive(&mut proc).await;
    turn.finish(&mut proc).await;
    result
}

struct Turn {
    input: Value,
    cwd: String,
    profile: String,
    session_id: Option<String>,
    model: String,
    provider: String,
    perm: Policy,
    settings: Map<String, Value>,
    host: Arc<dyn TurnHost>,
    emit: Emit,
    cancel: CancellationToken,
    out: mpsc::UnboundedSender<Value>,
    writer: Option<tokio::task::JoinHandle<()>>,
    live: Arc<AtomicBool>,
    pending: Arc<Mutex<HashSet<String>>>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    last_text: Option<String>,
    fault: Option<AdapterError>,
    steer: Option<(String, i64)>,
    poll_notes: bool,
    file_items: HashMap<String, Value>,
    delta_items: HashSet<String>,
}

impl Turn {
    #[allow(clippy::too_many_arguments)]
    fn new(
        req: &RunRequest,
        s: &sa_engine::adapter::Route,
        perm: Policy,
        settings: Map<String, Value>,
        host: Arc<dyn TurnHost>,
        emit: Emit,
        cancel: CancellationToken,
        proc: &mut Proc,
    ) -> Self {
        let (out, mut rx) = mpsc::unbounded_channel::<Value>();
        let mut stdin = proc.stdin.take();
        let writer = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let Some(pipe) = stdin.as_mut() else { break };
                let line = format!("{}\n", serde_json::to_string(&msg).unwrap_or_default());
                if pipe.write_all(line.as_bytes()).await.is_err() || pipe.flush().await.is_err() {
                    break;
                }
            }
            if let Some(mut pipe) = stdin {
                let _ = pipe.shutdown().await;
            }
        });
        Self {
            input: crate::images::codex(req),
            cwd: req.cwd.to_string_lossy().into_owned(),
            profile: req.profile.clone(),
            session_id: req.session_id.clone(),
            model: s.model.clone(),
            provider: s.provider.clone(),
            perm,
            settings,
            host,
            emit,
            cancel,
            out,
            writer: Some(writer),
            live: Arc::new(AtomicBool::new(true)),
            pending: Arc::default(),
            thread_id: None,
            turn_id: None,
            last_text: None,
            fault: None,
            steer: None,
            poll_notes: true,
            file_items: HashMap::new(),
            delta_items: HashSet::new(),
        }
    }

    fn send(&self, msg: Value) {
        if self.live.load(Ordering::SeqCst) {
            let _ = self.out.send(msg);
        }
    }

    async fn drive(&mut self, proc: &mut Proc) -> Result<RunOutcome, AdapterError> {
        let stdout = proc.stdout.take().ok_or_else(|| protocol("Codex 输出不可读"))?;
        let mut lines = Lines::new(stdout);
        let setup = Instant::now() + SETUP_TIMEOUT;
        let mut kill_at: Option<Instant> = None;
        let mut cancelling = false;
        let mut poll = tokio::time::interval(STEER_POLL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        self.send(json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {"name": "superagent_local", "title": "Super Agent", "version": "1.0"},
                "capabilities": {"experimentalApi": self.perm.review == "on-request"}
            }
        }));
        loop {
            tokio::select! {
                biased;
                () = self.cancel.cancelled(), if !cancelling => {
                    cancelling = true;
                    match (&self.thread_id, &self.turn_id) {
                        (Some(thread), Some(turn)) => {
                            self.send(json!({"id": 4, "method": "turn/interrupt", "params": {"threadId": thread, "turnId": turn}}));
                            kill_at = Some(Instant::now() + INTERRUPT_GRACE);
                        }
                        _ => proc.kill_tree().await,
                    }
                }
                () = sleep_until(kill_at.unwrap_or_else(Instant::now)), if kill_at.is_some() => {
                    kill_at = None;
                    proc.kill_tree().await;
                }
                () = sleep_until(setup), if self.turn_id.is_none() && self.fault.is_none() => {
                    let mut e = classified(ErrorClass::Stalled, "codex:setup_timeout");
                    e.message = "Codex 建立轮次超时".into();
                    self.fault = Some(e);
                    proc.kill_tree().await;
                }
                _ = poll.tick() => self.poll().await,
                next = lines.next() => match next? {
                    Some(event) => {
                        if let Some(done) = self.on_event(event).await? {
                            return Ok(done);
                        }
                    }
                    None => break,
                },
            }
        }
        if self.cancel.is_cancelled() {
            return Err(cancelled());
        }
        if let Some(f) = self.fault.take() {
            return Err(f);
        }
        let code = proc.wait_or_kill(Duration::from_millis(500)).await;
        let mut e = proc.exit_error(code);
        if e.class == ErrorClass::ProcessCrashed {
            e.message = "Codex 在完成前断开了会话".into();
        }
        e.session_id = self.thread_id.clone();
        Err(e)
    }

    /// Steer the first pending note into the live turn (CX:141-176).
    async fn poll(&mut self) {
        let (Some(thread), Some(turn)) = (&self.thread_id, &self.turn_id) else { return };
        if !self.poll_notes || self.cancel.is_cancelled() || self.steer.is_some() {
            return;
        }
        let Some(note) = self.host.pending_notes().into_iter().next() else { return };
        let seq = note["seq"].as_i64().unwrap_or_default();
        let text = format!("Same-ticket update from the named sender:\n{}", serde_json::to_string(&note).unwrap_or_default());
        if text.len() > MAX_STEER_BYTES {
            self.poll_notes = false;
            self.emit.journal("note.deferred", json!({"seq": seq, "reason": "超过实时插入大小"}));
            return;
        }
        let id = format!("steer-{seq}");
        let msg = json!({
            "id": id,
            "method": "turn/steer",
            "params": {"threadId": thread, "expectedTurnId": turn, "input": [{"type": "text", "text": text}]}
        });
        self.steer = Some((id, seq));
        self.send(msg);
    }

    async fn on_event(&mut self, event: Value) -> Result<Option<RunOutcome>, AdapterError> {
        let method = event["method"].as_str().map(str::to_string);
        let id = event.get("id").cloned().filter(|v| !v.is_null());
        let p = event.get("params").cloned().unwrap_or_else(|| json!({}));
        if let (Some((steer_id, seq)), None, Some(id)) = (&self.steer, &method, &id)
            && id.as_str() == Some(steer_id.as_str())
        {
            let seq = *seq;
            self.steer = None;
            let accepted = event.get("error").is_none_or(Value::is_null)
                && event["result"]["turnId"].as_str() == self.turn_id.as_deref()
                && self.turn_id.is_some();
            if accepted {
                self.host.accept_notes(&[seq]);
            } else {
                self.poll_notes = false;
                self.emit.journal("note.deferred", json!({"seq": seq, "reason": "原生轮次未确认收件"}));
            }
            return Ok(None);
        }
        if event.get("error").is_some_and(|e| !e.is_null()) {
            let mut e = rpc_error(&event["error"]);
            e.session_id = self.thread_id.clone();
            return Err(e);
        }
        if let (Some(method), Some(id)) = (&method, &id) {
            self.server_request(method, id.clone(), &p);
            return Ok(None);
        }
        match id.as_ref().and_then(Value::as_i64) {
            Some(1) => {
                self.send(json!({"method": "initialized", "params": {}}));
                let mut opts = json!({
                    "cwd": self.cwd,
                    "sandbox": self.perm.sandbox,
                    "approvalPolicy": self.perm.review,
                    "model": self.model,
                    "modelProvider": self.provider,
                });
                if self.perm.review == "on-request" {
                    opts["approvalsReviewer"] = json!("user");
                }
                if !self.settings.is_empty() {
                    opts["config"] = Value::Object(self.settings.clone());
                }
                if let Some(sid) = &self.session_id {
                    opts["threadId"] = json!(sid);
                    opts["excludeTurns"] = json!(true);
                }
                let method = if self.session_id.is_some() { "thread/resume" } else { "thread/start" };
                self.send(json!({"id": 2, "method": method, "params": opts}));
                return Ok(None);
            }
            Some(2) => {
                let r = &event["result"];
                let reviewed = self.perm.review == "on-request";
                if r["sandbox"]["type"].as_str() != Some(policy_type(self.perm.sandbox))
                    || r["approvalPolicy"].as_str() != Some(self.perm.review)
                    || (reviewed
                        && (r["approvalsReviewer"] != json!("user")
                            || r["sandbox"]["networkAccess"] != json!(false)
                            || r["sandbox"]["writableRoots"].as_array().is_some_and(|w| !w.is_empty())))
                {
                    return Err(denied("Codex 未确认请求的原生权限"));
                }
                if r["model"].as_str() != Some(&self.model) || r["modelProvider"].as_str() != Some(&self.provider) {
                    return Err(route_gone("Codex 未采用所选供应商和模型"));
                }
                let thread = r["thread"]["id"].as_str().map(str::to_string);
                match (&thread, &self.session_id) {
                    (None, _) => return Err(session_mismatch("Codex 会话不匹配")),
                    (Some(t), Some(s)) if t != s => return Err(session_mismatch("Codex 会话不匹配")),
                    _ => {}
                }
                let thread = thread.unwrap_or_default();
                self.emit.session(&thread);
                self.thread_id = Some(thread.clone());
                self.emit.journal(
                    "permission.effective",
                    json!({
                        "profile": self.profile,
                        "sandbox": r["sandbox"],
                        "approvalPolicy": r["approvalPolicy"],
                        "reviewer": r.get("approvalsReviewer").cloned().unwrap_or(Value::Null),
                    }),
                );
                self.emit.journal(
                    "route.selected",
                    json!({"provider": self.provider, "model": self.model, "mode": "cli"}),
                );
                self.send(json!({
                    "id": 3,
                    "method": "turn/start",
                    "params": {"threadId": thread, "input": self.input}
                }));
                return Ok(None);
            }
            Some(3) => {
                if let Some(t) = event["result"]["turn"]["id"].as_str() {
                    self.turn_id = Some(t.to_string());
                }
                return Ok(None);
            }
            _ => {}
        }
        let Some(method) = method else { return Ok(None) };
        let Some(thread) = self.thread_id.clone() else { return Ok(None) };
        if p["threadId"].as_str() != Some(&thread) && p["thread"]["id"].as_str() != Some(&thread) {
            return Ok(None);
        }
        self.emit.journal("native", event.clone());
        if method == "item/agentMessage/delta"
            && let Some(delta) = p["delta"].as_str()
        {
            self.emit.journal("delta", json!(delta));
            if !delta.is_empty()
                && let Some(item) = p["itemId"].as_str()
            {
                self.delta_items.insert(item.to_string());
            }
        }
        let item = &p["item"];
        let item_id = item["id"].as_str().unwrap_or_default().to_string();
        if method == "item/completed" && item["type"] == json!("agentMessage") && self.delta_items.remove(&item_id) {
            self.emit.journal("message.boundary", Value::Null);
        }
        match method.as_str() {
            "turn/started" => {
                if let Some(t) = p["turn"]["id"].as_str() {
                    self.turn_id = Some(t.to_string());
                }
            }
            "serverRequest/resolved" => {
                let key = request_key(&p["requestId"]);
                self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
            }
            "item/started" => {
                if item["type"] == json!("fileChange") {
                    self.file_items.insert(item_id.clone(), item.clone());
                }
                if is_tool_item(item) {
                    self.emit.tool_started(&item_id);
                }
            }
            "item/completed" => {
                if item["type"] == json!("agentMessage") && item["phase"] != json!("commentary") {
                    self.last_text = item["text"].as_str().map(str::to_string);
                }
                self.file_items.remove(&item_id);
                if is_tool_item(item) {
                    self.emit.tool_finished(&item_id);
                }
            }
            // The native client announced its own retry; keep waiting.
            "error" if p["willRetry"] == json!(true) => self.emit.native_retry(None),
            "turn/completed" if p["turn"]["id"].as_str() == self.turn_id.as_deref() => {
                if self.cancel.is_cancelled() {
                    return Err(cancelled());
                }
                if p["turn"]["status"] != json!("completed") {
                    let mut e = rpc_error(&p["turn"]["error"]);
                    e.session_id = Some(thread);
                    return Err(e);
                }
                let Some(text) = self.last_text.clone() else {
                    let mut e = protocol("Codex 轮次缺少终稿");
                    e.session_id = Some(thread);
                    return Err(e);
                };
                return Ok(Some(RunOutcome {
                    text,
                    session_id: Some(thread),
                }));
            }
            _ => {}
        }
        Ok(None)
    }

    /// Server-initiated requests (CX:200-244).
    fn server_request(&mut self, method: &str, id: Value, p: &Value) {
        let same = self.thread_id.is_some()
            && self.turn_id.is_some()
            && p["threadId"].as_str() == self.thread_id.as_deref()
            && p["turnId"].as_str() == self.turn_id.as_deref()
            && !self.cancel.is_cancelled();
        if matches!(method, "mcpServer/elicitation/request" | "tool/requestUserInput") {
            self.send(json!({"id": id, "result": app_reply(method, p, self.perm.native && same)}));
            return;
        }
        let Some(title) = request_title(method) else {
            self.send(json!({"id": id, "error": {"code": -32601, "message": "Unsupported server request"}}));
            return;
        };
        let key = request_key(&id);
        let item_id = p["itemId"].as_str().unwrap_or_default();
        let file_item = self.file_items.get(item_id).cloned();
        let accept_offered = p["availableDecisions"]
            .as_array()
            .is_none_or(|d| d.iter().any(|x| x == "accept"));
        let busy = self.pending.lock().unwrap_or_else(|e| e.into_inner()).contains(&key);
        if !same
            || item_id.is_empty()
            || busy
            || (!self.perm.native && self.perm.review != "on-request")
            || !accept_offered
            || !reviewable(method, p, file_item.as_ref())
        {
            self.send(json!({"id": id, "result": decision(method, p, false)}));
            return;
        }
        if self.perm.native {
            self.send(json!({"id": id, "result": decision(method, p, true)}));
            return;
        }
        let mut action = json!({
            "adapter": "codex",
            "method": method,
            "title": title,
            "request_id": id,
            "params": p,
        });
        if let Some(item) = file_item {
            action["item"] = item;
        }
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone());
        let (host, out, live, pending, cancel) = (
            self.host.clone(),
            self.out.clone(),
            self.live.clone(),
            self.pending.clone(),
            self.cancel.clone(),
        );
        let (method, p) = (method.to_string(), p.clone());
        tokio::spawn(async move {
            let allow = host.approve(action).await;
            let still = pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
            if live.load(Ordering::SeqCst) && still {
                let _ = out.send(json!({"id": id, "result": decision(&method, &p, allow && !cancel.is_cancelled())}));
            }
        });
    }

    /// `finally` (CX:319-332): close stdin, wait briefly, then kill.
    async fn finish(&mut self, proc: &mut Proc) {
        self.live.store(false, Ordering::SeqCst);
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).clear();
        if let Some(w) = self.writer.take() {
            w.abort();
            let _ = w.await;
        }
        proc.wait_or_kill(Duration::from_millis(1800)).await;
        if let Some((_, seq)) = self.steer.take() {
            self.emit.journal("note.deferred", json!({"seq": seq, "reason": "通道结束前未收到回执"}));
        }
    }
}

fn request_key(id: &Value) -> String {
    serde_json::to_string(id).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// Fake app-server (T: adapters-codex.test.cjs peer).
    const PEER: &str = r#"
const lines = require('node:readline').createInterface({ input: process.stdin, crlfDelay: Infinity })[Symbol.asyncIterator]();
const recv = async () => JSON.parse((await lines.next()).value), send = (o) => process.stdout.write(JSON.stringify(o) + '\n');
(async () => {
  await recv(); send({ id: 1, result: {} });
  await recv();
  const thread = await recv();
  if (thread.params.sandbox !== 'workspace-write' || thread.params.approvalPolicy !== 'on-request') process.exit(3);
  send({ id: 2, result: { sandbox: { type: 'workspaceWrite', networkAccess: false, writableRoots: [] }, model: 'm', modelProvider: 'p',
    approvalPolicy: 'on-request', approvalsReviewer: 'user', thread: { id: 'thread' } } });
  const turn = await recv();
  if (turn.params.input[0].text !== 'test' || turn.params.input[1].type !== 'image' ||
      !turn.params.input[1].url.startsWith('data:image/png;base64,') ||
      Buffer.from(turn.params.input[1].url.split(',')[1], 'base64').subarray(1,4).toString() !== 'PNG') process.exit(7);
  send({ id: 3, result: { turn: { id: 'turn' } } });
  const steer = await recv();
  if (steer.method !== 'turn/steer' || steer.params.expectedTurnId !== 'turn' || !steer.params.input[0].text.includes('消息')) process.exit(4);
  send({ id: steer.id, result: { turnId: 'turn' } });
  const params = { threadId: 'thread', turnId: 'turn', itemId: 'item', command: 'review this command' };
  send({ method: 'item/commandExecution/requestApproval', id: 24, params });
  if ((await recv()).result.decision !== 'accept') process.exit(5);
  send({ method: 'item/commandExecution/requestApproval', id: 25, params: { ...params, turnId: 'old' } });
  if ((await recv()).result.decision !== 'decline') process.exit(6);
  send({ method: 'item/agentMessage/delta', params: { ...params, delta: '正文\t😀\r\n' } });
  send({ method: 'item/completed', params: { ...params, item: { id: 'item', type: 'agentMessage', text: '正文\t😀\r\n' } } });
  send({ method: 'turn/completed', params: { threadId: 'thread', turn: { id: 'turn', status: 'completed' } } });
})();
"#;

    #[derive(Default)]
    struct Host {
        actions: StdMutex<Vec<Value>>,
        accepted: StdMutex<Vec<i64>>,
    }

    impl TurnHost for Host {
        fn approve(&self, action: Value) -> BoxFuture<'static, bool> {
            self.actions.lock().unwrap().push(action);
            Box::pin(async { true })
        }
        fn pending_notes(&self) -> Vec<Value> {
            if self.accepted.lock().unwrap().is_empty() {
                vec![json!({"seq": 7, "sender": "claude", "body": "消息"})]
            } else {
                vec![]
            }
        }
        fn accept_notes(&self, seqs: &[i64]) {
            self.accepted.lock().unwrap().extend_from_slice(seqs);
        }
    }

    #[tokio::test]
    async fn stdio_framing_approval_stale_denial_steer_and_exact_final() {
        let sys = Sys::from_process();
        let Some(node) = sys.which(if cfg!(windows) { "node.exe" } else { "node" }) else {
            eprintln!("node not on PATH; skipped");
            return;
        };
        let cwd = std::env::current_dir().unwrap();
        let proc = start(&node, &["-e".to_string(), PEER.to_string()], &cwd, &sys).unwrap();
        let req = RunRequest {
            ticket: 1,
            job: 1,
            turn: 1,
            agent: "codex".into(),
            prompt: "test".into(),
            images: vec![sa_engine::images::ImageInput::from_bytes("图.png".into(), {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=").unwrap()
            }).unwrap()],
            cwd: cwd.clone(),
            route: sa_engine::adapter::Route { provider: "p".into(), model: "m".into(), mode: "cli".into() },
            profile: "partial".into(),
            writable: true,
            session_id: None,
        };
        let perm = policy("partial", true).unwrap();
        let host = Arc::new(Host::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            converse(proc, &req, &req.route, perm, Map::new(), host.clone(), Emit(tx), CancellationToken::new()),
        )
        .await
        .expect("turn finished")
        .expect("turn succeeded");
        assert_eq!(out.text, "正文\t😀\r\n");
        assert_eq!(out.session_id.as_deref(), Some("thread"));
        assert_eq!(*host.accepted.lock().unwrap(), vec![7]);
        let actions = host.actions.lock().unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0]["params"]["command"], json!("review this command"));
        let mut text = vec![];
        let mut session = None;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                sa_engine::adapter::AdapterEvent::Journal { kind, data } if kind == "delta" || kind == "message.boundary" => {
                    text.push((kind, data))
                }
                sa_engine::adapter::AdapterEvent::Session(s) => session = Some(s),
                _ => {}
            }
        }
        assert_eq!(text, vec![("delta".to_string(), json!(out.text)), ("message.boundary".to_string(), Value::Null)]);
        assert_eq!(session.as_deref(), Some("thread"));
    }

    #[test]
    fn policies_match_the_js_matrix() {
        assert!(policy("readonly", true).is_err());
        assert!(policy("full", false).is_err());
        let p = policy("ask", true).unwrap();
        assert_eq!((p.sandbox, p.review, p.native), ("read-only", "on-request", false));
        let p = policy("auto", true).unwrap();
        assert_eq!((p.sandbox, p.review, p.native), ("danger-full-access", "never", true));
        let p = policy("legacy", false).unwrap();
        assert_eq!((p.sandbox, p.review), ("read-only", "never"));
    }
}
