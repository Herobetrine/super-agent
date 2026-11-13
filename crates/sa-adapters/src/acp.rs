//! Operator-configured ACP v1 agents, using the official Rust SDK.
//!
//! ACP describes transport and approval, not an OS sandbox. Restricted runs
//! require an explicit operator declaration and an advertised matching mode.
use crate::{
    common::{Emit, modes, route_gone, select},
    proc,
    sys::Sys,
};
use agent_client_protocol::{self as sdk, Agent, ByteStreams, Client};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::future::BoxFuture;
use sa_engine::adapter::{
    Adapter, AdapterError, Catalog, ErrorClass, EventSink, RunOutcome, RunRequest, TurnHost,
};
use sdk::schema::ProtocolVersion;
use sdk::v1::{ConnectionTo, schema::*};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio_util::{
    compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt},
    sync::CancellationToken,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub id: String,
    #[serde(default)]
    pub label: String,
    /// Absolute executable path or a basename resolved against PATH, no shell.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Names of existing environment variables to forward; never secret values.
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default = "default_models")]
    pub models: Vec<String>,
    /// ACP config option advertised by session/new or session/load.
    #[serde(default)]
    pub model_config_id: Option<String>,
    /// Map Super Agent profiles to advertised ACP session modes.
    #[serde(default)]
    pub permission_modes: HashMap<String, String>,
    /// Operator assertion after testing this executable's actual confinement.
    #[serde(default)]
    pub trusted_sandbox: bool,
}
fn default_models() -> Vec<String> {
    vec!["default".into()]
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    #[serde(default)]
    pub servers: Vec<Server>,
}

pub fn config_path(sys: &Sys) -> PathBuf {
    sys.var("SUPERAGENT_ACP_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| sys.platform_dir().join("acp.json"))
}
fn load(sys: &Sys) -> anyhow::Result<Configuration> {
    let path = config_path(sys);
    if !path.exists() {
        return Ok(Configuration::default());
    }
    anyhow::ensure!(
        std::fs::metadata(&path)?.len() <= 256 * 1024,
        "ACP 配置超过 256 KiB"
    );
    let cfg: Configuration = serde_json::from_slice(&std::fs::read(path)?)?;
    let mut ids = HashSet::new();
    for s in &cfg.servers {
        anyhow::ensure!(
            !s.id.is_empty()
                && s.id.len() <= 64
                && s.id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                && ids.insert(s.id.clone()),
            "ACP 服务标识无效或重复"
        );
        anyhow::ensure!(
            !s.command.is_empty()
                && !s.models.is_empty()
                && s.models
                    .iter()
                    .all(|m| sa_store::providers::valid_model_id(m)),
            "ACP 命令或模型无效"
        );
        anyhow::ensure!(
            s.model_config_id.is_some() || s.models == default_models(),
            "多个 ACP 模型需要 model_config_id"
        );
        anyhow::ensure!(
            s.env
                .iter()
                .all(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')),
            "ACP 环境变量名称无效"
        );
    }
    Ok(cfg)
}
fn executable(sys: &Sys, server: &Server) -> Option<PathBuf> {
    let path = PathBuf::from(&server.command);
    if path.is_absolute() {
        path.is_file().then_some(path)
    } else if path.components().count() == 1 {
        sys.which(&server.command)
    } else {
        None
    }
}
fn catalog(sys: &Sys, cfg: &Configuration) -> Value {
    let providers: Vec<Value> = cfg.servers.iter().map(|s| json!({
        "id":s.id, "label":if s.label.is_empty() { &s.id } else { &s.label },
        "available":executable(sys,s).is_some(), "source":"ACP 本机配置",
        "models":s.models.iter().map(|m|json!({"id":m,"label":m,"reasoningEfforts":[]})).collect::<Vec<_>>(),
        "defaultModel":s.models.first(), "detail":"ACP v1 stdio；权限能力需由服务实现"
    })).collect();
    let first = providers.iter().find(|p| p["available"] == true);
    json!({"agent":"acp", "label":"ACP", "available":first.is_some(),
        "default":first.map(|p|json!({"provider":p["id"],"model":p["defaultModel"],"mode":"cli"})),
        "providers":providers, "modes":modes("ACP stdio", first.is_some())})
}
pub struct Acp {
    sys: Sys,
}
impl Acp {
    pub fn new(sys: Sys) -> Self {
        Self { sys }
    }
}
impl Adapter for Acp {
    fn agent(&self) -> &'static str {
        "acp"
    }
    fn catalog(&self) -> BoxFuture<'static, anyhow::Result<Catalog>> {
        let sys = self.sys.clone();
        Box::pin(async move {
            let cfg = load(&sys)?;
            Ok(Catalog {
                value: catalog(&sys, &cfg),
            })
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
        Box::pin(async move { run(sys, request, host, Emit(events), cancel).await })
    }
}
fn failure(class: ErrorClass, code: &str) -> AdapterError {
    let mut e = AdapterError::new(class, "ACP 执行失败，请检查服务能力、认证和配置");
    e.code = Some(code.into());
    e
}
fn sdk_failure(e: sdk::Error, antigravity: bool) -> AdapterError {
    if e.data
        .as_ref()
        .is_some_and(|data| data == "session identity mismatch")
    {
        return failure(ErrorClass::SessionMismatch, "acp:session_mismatch");
    }
    if antigravity {
        let diagnostic = format!("{} {}", e.message, e.data.as_ref().unwrap_or(&Value::Null));
        let classified = crate::antigravity::native_failure(&diagnostic);
        if classified.class != ErrorClass::Other {
            return classified;
        }
    }
    let class = match e.code.value() {
        -32000 => ErrorClass::Auth,
        -32800 => ErrorClass::Cancelled,
        _ => ErrorClass::Protocol,
    };
    failure(class, &format!("acp:rpc:{}", e.code.value()))
}
async fn run(
    sys: Sys,
    req: RunRequest,
    host: Arc<dyn TurnHost>,
    emit: Emit,
    cancel: CancellationToken,
) -> Result<RunOutcome, AdapterError> {
    if !req.images.is_empty() {
        return Err(failure(ErrorClass::InputTooLarge, "acp:images_unsupported"));
    }
    let cfg = load(&sys).map_err(|_| route_gone("ACP 配置无效，请检查 acp.json"))?;
    select(&catalog(&sys, &cfg), &req.route)?;
    let server = cfg
        .servers
        .into_iter()
        .find(|s| s.id == req.route.provider)
        .ok_or_else(|| route_gone("ACP 服务不存在"))?;
    run_server(sys, req, host, emit, cancel, server, false).await
}

/// Reuse ACP lifecycle and permission handling for the built-in agy adapter.
/// Antigravity supplies its already isolated environment and verified server.
pub(crate) async fn run_server(
    sys: Sys,
    req: RunRequest,
    host: Arc<dyn TurnHost>,
    emit: Emit,
    cancel: CancellationToken,
    server: Server,
    antigravity: bool,
) -> Result<RunOutcome, AdapterError> {
    if !matches!(
        req.profile.as_str(),
        "readonly" | "ask" | "partial" | "full" | "auto" | "legacy"
    ) {
        return Err(failure(ErrorClass::Permission, "acp:unknown_profile"));
    }
    if cancel.is_cancelled() {
        return Err(failure(ErrorClass::Cancelled, "acp:cancelled"));
    }
    let unrestricted = matches!(req.profile.as_str(), "full" | "auto");
    if unrestricted && !req.writable {
        return Err(failure(ErrorClass::Permission, "acp:readonly_conflict"));
    }
    let effective_profile = if !req.writable {
        "readonly"
    } else {
        req.profile.as_str()
    };
    let restricted_mode = server.permission_modes.get(effective_profile).cloned();
    if !unrestricted && (!server.trusted_sandbox || restricted_mode.is_none()) {
        return Err(failure(ErrorClass::Permission, "acp:sandbox_unverified"));
    }
    if !unrestricted && req.profile == "readonly" && req.writable {
        return Err(failure(ErrorClass::Permission, "acp:readonly_writer"));
    }
    let exe = executable(&sys, &server).ok_or_else(|| route_gone("ACP 执行程序不存在"))?;
    let mut env = sys.clone();
    if !antigravity {
        env.retain(|name| {
            [
                "PATH",
                "PATHEXT",
                "SYSTEMROOT",
                "WINDIR",
                "TEMP",
                "TMP",
                "HOME",
                "USERPROFILE",
                "LOCALAPPDATA",
                "APPDATA",
            ]
            .iter()
            .any(|n| name.eq_ignore_ascii_case(n))
                || server.env.iter().any(|n| name.eq_ignore_ascii_case(n))
        });
    }
    let mut process = proc::start(&exe, &server.args, &req.cwd, &env)?;
    let stdin = process
        .stdin
        .take()
        .ok_or_else(|| failure(ErrorClass::Protocol, "acp:stdin"))?;
    let stdout = process
        .stdout
        .take()
        .ok_or_else(|| failure(ErrorClass::Protocol, "acp:stdout"))?;
    let active = Arc::new(Mutex::new(None::<String>));
    let text = Arc::new(Mutex::new(String::new()));
    let prompting = Arc::new(AtomicBool::new(false));
    let invalid_session = Arc::new(AtomicBool::new(false));
    let handler_invalid_session = invalid_session.clone();
    let handler_active = active.clone();
    let permission_active = active.clone();
    let handler_text = text.clone();
    let handler_prompting = prompting.clone();
    let handler_emit = emit.clone();
    let permission_emit = emit.clone();
    let permission_cancel = cancel.clone();
    let permission_profile = if antigravity && !unrestricted {
        "readonly"
    } else {
        effective_profile
    }
    .to_string();
    let permission_agent = req.agent.clone();
    let callback_active = active.clone();
    let callback_emit = emit.clone();
    let callback_cancel = cancel.clone();
    let callback_req = req.clone();
    let operation = Client.v1()
        .on_receive_notification(async move |notification: SessionNotification,_cx| {
            let value=serde_json::to_value(&notification).map_err(|_|sdk::Error::invalid_params())?;
            if !handler_prompting.load(Ordering::SeqCst) { return Ok(()); }
            if handler_active.lock().unwrap().as_deref()!=Some(notification.session_id.to_string().as_str()) { handler_invalid_session.store(true,Ordering::SeqCst); return Err(sdk::Error::invalid_params()); }
            handler_emit.journal("native",json!({"method":"session/update","params":value}));
            let update=&value["update"];
            match update["sessionUpdate"].as_str() {
                Some("agent_message_chunk")=>if let Some(chunk)=update["content"]["text"].as_str() {
                    let mut text=handler_text.lock().unwrap();
                    if text.len()+chunk.len()>16*1024*1024 { return Err(sdk::Error::invalid_params()); }
                    text.push_str(chunk); handler_emit.journal("delta",json!(chunk));
                },
                Some("tool_call")=>if let Some(id)=update["toolCallId"].as_str(){handler_emit.tool_started(id);},
                Some("tool_call_update")=>if matches!(update["status"].as_str(),Some("completed"|"failed")) && let Some(id)=update["toolCallId"].as_str(){handler_emit.tool_finished(id);},
                _=>{}
            }
            Ok(())
        },sdk::on_receive_notification!())
        .on_receive_request(async move |permission:RequestPermissionRequest,responder,_cx| {
            let same=permission_active.lock().unwrap().as_deref()==Some(permission.session_id.to_string().as_str());
            let allow=if !same || permission_profile=="readonly" || permission_cancel.is_cancelled(){false}
            else if matches!(permission_profile.as_str(),"full"|"auto"){true}
            else {
                let _=permission_emit.0.send(sa_engine::adapter::AdapterEvent::AwaitingApproval);
                tokio::select!{r=host.approve(json!({"agent":permission_agent,"action":"tool","request":permission}))=>r,()=permission_cancel.cancelled()=>false}
            };
            let selected=permission.options.iter().find(|o|o.kind==if allow{PermissionOptionKind::AllowOnce}else{PermissionOptionKind::RejectOnce});
            let outcome=selected.map(|o|RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(o.option_id.clone()))).unwrap_or(RequestPermissionOutcome::Cancelled);
            permission_emit.journal("adapter.approval",json!({"granted":allow,"session":permission.session_id}));
            responder.respond(RequestPermissionResponse::new(outcome))
        },sdk::on_receive_request!())
        .connect_with(ByteStreams::new(stdin.compat_write(),stdout.compat()),move |connection:ConnectionTo<Agent>| async move {
            let init=connection.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
            if init.protocol_version!=ProtocolVersion::V1 { return Err(sdk::Error::invalid_params()); }
            if !callback_req.images.is_empty() && !init.agent_capabilities.prompt_capabilities.image {
                return Err(sdk::Error::invalid_params().data("image capability required"));
            }
            let (sid,session)=if antigravity {
                native_session(&connection, &callback_req, init.agent_capabilities.load_session).await?
            } else if let Some(sid)=&callback_req.session_id {
                if !init.agent_capabilities.load_session {return Err(sdk::Error::invalid_params());}
                *callback_active.lock().unwrap()=Some(sid.clone());
                let loaded=connection.send_request(LoadSessionRequest::new(sid.clone(),callback_req.cwd.clone())).block_task().await?;
                (sid.clone(),serde_json::to_value(loaded).unwrap())
            }else{
                let created=connection.send_request(NewSessionRequest::new(callback_req.cwd.clone())).block_task().await?;
                let sid=created.session_id.to_string();
                (sid,serde_json::to_value(created).unwrap())
            };
            *callback_active.lock().unwrap()=Some(sid.clone()); callback_emit.session(&sid);
            if let Some(mode)=&restricted_mode {
                let advertised=session["modes"]["availableModes"].as_array().is_some_and(|m|m.iter().any(|m|m["id"]==*mode));
                if !advertised{return Err(sdk::Error::invalid_params());}
                connection.send_request(SetSessionModeRequest::new(sid.clone(),mode.clone())).block_task().await?;
            }
            if antigravity {
                configure_native(&connection, &callback_req, &sid, &session, restricted_mode.as_deref()).await?;
            }
            if let Some(config_id)=server.model_config_id {
                let advertised=session["configOptions"].as_array().is_some_and(|v|v.iter().any(|v|v["id"]==config_id && v["options"].as_array().is_some_and(|o|o.iter().any(|o|o["value"]==callback_req.route.model))));
                if !advertised{return Err(sdk::Error::invalid_params());}
                connection.send_request(SetSessionConfigOptionRequest::new(sid.clone(),config_id,callback_req.route.model.as_str())).block_task().await?;
            }
            callback_emit.journal("adapter.permission",json!({"profile":callback_req.profile,"scope":if unrestricted{"danger-full-access"}else if antigravity{"cli-deny-policy+mode"}else{"operator-verified-acp-mode"}}));
            callback_emit.journal("adapter.route",json!({"agent":callback_req.agent,"provider":callback_req.route.provider,"requested_model":callback_req.route.model,"mode":"cli","protocol":"acp-v1"}));
            prompting.store(true,Ordering::SeqCst);
            let mut content = vec![ContentBlock::Text(TextContent::new(callback_req.prompt.clone()))];
            for image in &callback_req.images {
                content.push(ContentBlock::Image(ImageContent::new(STANDARD.encode(image.data()), image.media_type())));
            }
            let prompt=connection.send_request(PromptRequest::new(sid.clone(),content)).block_task();
            tokio::pin!(prompt);
            let reply=tokio::select! {
                r=&mut prompt=>r?,
                ()=callback_cancel.cancelled()=>{
                    connection.send_notification(CancelNotification::new(sid))?;
                    let _=tokio::time::timeout(Duration::from_millis(300),&mut prompt).await;
                    return Err(sdk::Error::new(sdk::ErrorCode::REQUEST_CANCELLED,"cancelled"));
                }
            };
            callback_emit.journal("native",json!({"method":"session/prompt","result":reply}));
            if reply.stop_reason!=StopReason::EndTurn { return Err(sdk::Error::invalid_params()); }
            Ok(())
        });
    tokio::pin!(operation);
    let result = tokio::select! {
        result=&mut operation=>result.map_err(|error| sdk_failure(error, antigravity)),
        ()=cancel.cancelled()=>{
            let _=tokio::time::timeout(Duration::from_millis(500),&mut operation).await;
            Err(failure(ErrorClass::Cancelled,"acp:cancelled"))
        }
    };
    process.kill_tree().await;
    process.wait().await;
    let sid = active.lock().unwrap().clone().or(req.session_id);
    if invalid_session.load(Ordering::SeqCst) {
        return Err(failure(ErrorClass::SessionMismatch, "acp:session_mismatch"));
    }
    if let Err(mut error) = result {
        error.session_id = sid;
        return Err(error);
    }
    emit.journal("message.boundary", Value::Null);
    let result_text = text.lock().unwrap().clone();
    if result_text.is_empty() {
        return Err(failure(ErrorClass::Protocol, "acp:empty_result"));
    }
    Ok(RunOutcome {
        text: result_text,
        session_id: sid,
    })
}

// agy retains legacy model IDs alongside standard ACP config options. Keep
// those fields using SDK untyped requests rather than dropping them in the
// standard session response schema.
async fn native_session(
    connection: &ConnectionTo<Agent>,
    req: &RunRequest,
    can_load: bool,
) -> Result<(String, Value), sdk::Error> {
    let (method, params) = if let Some(sid) = &req.session_id {
        if !can_load {
            return Err(sdk::Error::invalid_params());
        }
        (
            "session/load",
            json!({"sessionId":sid,"cwd":req.cwd,"mcpServers":[]}),
        )
    } else {
        ("session/new", json!({"cwd":req.cwd,"mcpServers":[]}))
    };
    let session = connection
        .send_request(sdk::UntypedMessage::new(method, params)?)
        .block_task()
        .await?;
    let sid = session["sessionId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(sdk::Error::invalid_params)?
        .to_string();
    if req
        .session_id
        .as_deref()
        .is_some_and(|wanted| wanted != sid)
    {
        return Err(sdk::Error::invalid_params().data("session identity mismatch"));
    }
    if !session["_meta"]["antigravity"]["tier"].is_object() {
        return Err(sdk::Error::invalid_params().data("Google account route could not be verified"));
    }
    Ok((sid, session))
}

fn native_option<'a>(session: &'a Value, id: &str, value: &str) -> Result<&'a Value, sdk::Error> {
    session["configOptions"]
        .as_array()
        .and_then(|options| options.iter().find(|o| o["id"] == id))
        .filter(|option| {
            option["options"]
                .as_array()
                .is_some_and(|values| values.iter().any(|o| o["value"] == value))
        })
        .ok_or_else(sdk::Error::invalid_params)
}

async fn configure_native(
    connection: &ConnectionTo<Agent>,
    req: &RunRequest,
    sid: &str,
    session: &Value,
    mode: Option<&str>,
) -> Result<(), sdk::Error> {
    if req.route.model != "@native" {
        if !session["models"]["availableModels"]
            .as_array()
            .is_some_and(|models| models.iter().any(|m| m["modelId"] == req.route.model))
        {
            return Err(sdk::Error::invalid_params().data("requested model unavailable"));
        }
        connection
            .send_request(sdk::UntypedMessage::new(
                "session/set_model",
                json!({"sessionId":sid,"modelId":req.route.model}),
            )?)
            .block_task()
            .await?;
    }
    // A load on the same ACP connection reads the selected exact model and
    // mode back before any prompt. History updates remain outside prompting.
    let loaded = connection
        .send_request(sdk::UntypedMessage::new(
            "session/load",
            json!({"sessionId":sid,"cwd":req.cwd,"mcpServers":[]}),
        )?)
        .block_task()
        .await?;
    if loaded["sessionId"] != sid
        || (req.route.model != "@native" && loaded["models"]["currentModelId"] != req.route.model)
        || mode.is_some_and(|mode| loaded["modes"]["currentModeId"] != mode)
    {
        return Err(sdk::Error::invalid_params().data("native route readback mismatch"));
    }
    if let Some(effort) = &req.route.reasoning_effort {
        native_option(&loaded, "thought_level", effort)?;
        let configured = connection
            .send_request(SetSessionConfigOptionRequest::new(
                sid.to_string(),
                "thought_level",
                effort.as_str(),
            ))
            .block_task()
            .await?;
        let value = serde_json::to_value(configured)?;
        if native_option(&value, "thought_level", effort)?["currentValue"] != *effort {
            return Err(sdk::Error::invalid_params().data("native effort readback mismatch"));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "antigravity_tests.rs"]
mod antigravity_tests;
