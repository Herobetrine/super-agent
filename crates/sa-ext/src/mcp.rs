//! MCP client connections over the official Rust SDK (`rmcp`), port of
//! `transportFor`/`withClient` (X:63-97).
//!
//! - stdio: the configured executable runs without a shell, without a console
//!   window, in its own job object (Windows) or process group, with the MCP
//!   SDK's safe default environment only; stderr is discarded.
//! - streamable HTTP: the validated endpoint, an optional bearer token read
//!   from the configured `MCP_*` variable, no redirects, no proxy.
//!
//! Every connection serves one operation and is closed afterwards (session
//! `DELETE` for HTTP; stdin close, then kill after 3 s, for stdio). Dropping
//! a connection (cancellation) also tears the process tree down.

use crate::validate::endpoint;
use anyhow::{Context, Result, anyhow, bail};
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation, PaginatedRequestParams, ProtocolVersion,
};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Map, Value, json};
use std::future::Future;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
pub const LIST_TIMEOUT: Duration = Duration::from_secs(10);
pub const CALL_TIMEOUT: Duration = Duration::from_secs(60);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
pub const CANCELLED: &str = "操作已取消";

/// `DEFAULT_INHERITED_ENV_VARS` of the MCP TypeScript SDK: the only variables
/// a stdio server inherits, so tokens and API keys in the studio's
/// environment never reach it.
#[cfg(windows)]
const INHERITED_ENV: &[&str] = &[
    "APPDATA",
    "HOMEDRIVE",
    "HOMEPATH",
    "LOCALAPPDATA",
    "PATH",
    "PROCESSOR_ARCHITECTURE",
    "SYSTEMDRIVE",
    "SYSTEMROOT",
    "TEMP",
    "USERNAME",
    "USERPROFILE",
    "PROGRAMFILES",
];
#[cfg(not(windows))]
const INHERITED_ENV: &[&str] = &["HOME", "LOGNAME", "PATH", "SHELL", "TERM", "USER"];

/// Launch parameters of one `studio_servers` row.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub transport: String,
    pub command: String,
    pub args: Vec<String>,
    pub url: String,
    pub token_env: String,
}

impl ServerConfig {
    /// From a `SELECT * FROM studio_servers` row.
    pub fn from_row(row: &Map<String, Value>) -> Result<Self> {
        let s = |k: &str| row.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        Ok(Self {
            transport: s("transport"),
            command: s("command"),
            args: serde_json::from_str(&s("args")).context("MCP 服务参数无效")?,
            url: s("url"),
            token_env: s("token_env"),
        })
    }
}

pub type Client = RunningService<RoleClient, ClientConfig>;

fn client_info() -> ClientConfig {
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("super-agent-studio", "0.2.0"),
    )
    .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE)
}

/// Race `fut` against `limit` and `cancel`.
pub async fn bounded<T>(
    fut: impl Future<Output = Result<T>>,
    limit: Duration,
    cancel: &CancellationToken,
    timeout: &str,
) -> Result<T> {
    tokio::select! {
        r = tokio::time::timeout(limit, fut) => r.map_err(|_| anyhow!("{timeout}"))?,
        () = cancel.cancelled() => bail!(CANCELLED),
    }
}

fn stdio(config: &ServerConfig) -> Result<TokioChildProcess> {
    let exe = Path::new(&config.command);
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(&config.args).env_clear().kill_on_drop(true);
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
    }
    for key in INHERITED_ENV {
        if let Some(v) = std::env::var_os(key).filter(|v| !v.to_string_lossy().starts_with("()")) {
            cmd.env(key, v);
        }
    }
    let mut wrap = process_wrap::tokio::CommandWrap::from(cmd);
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::CREATE_NO_WINDOW;
        wrap.wrap(process_wrap::tokio::CreationFlags(CREATE_NO_WINDOW))
            .wrap(process_wrap::tokio::JobObject);
    }
    #[cfg(unix)]
    wrap.wrap(process_wrap::tokio::ProcessGroup::leader());
    wrap.wrap(process_wrap::tokio::KillOnDrop);
    let (proc, _) = TokioChildProcess::builder(wrap)
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow!("无法启动 MCP 服务：{}", e.kind()))?;
    Ok(proc)
}

fn http(config: &ServerConfig) -> Result<StreamableHttpClientTransport<reqwest::Client>> {
    let url = endpoint(Some(&json!(config.url)))?;
    let token = if config.token_env.is_empty() {
        None
    } else {
        let token = std::env::var(&config.token_env).ok().filter(|t| !t.is_empty());
        Some(token.ok_or_else(|| anyhow!("未设置此 MCP 服务的令牌环境变量"))?)
    };
    // `redirect: 'error'` keeps every request on the configured origin.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(|a| {
            a.error("MCP 请求尝试离开已配置的来源")
        }))
        .pool_max_idle_per_host(0)
        .no_proxy()
        .build()
        .context("无法创建 HTTP 客户端")?;
    let mut cfg = StreamableHttpClientTransportConfig::with_uri(url.as_str());
    if let Some(token) = token {
        cfg = cfg.auth_header(token);
    }
    Ok(StreamableHttpClientTransport::with_client(client, cfg))
}

/// `client.connect(transport, {timeout: 12000, signal})`.
pub async fn connect(config: &ServerConfig, cancel: &CancellationToken) -> Result<Client> {
    if cancel.is_cancelled() {
        bail!(CANCELLED);
    }
    let fut = async {
        let client = if config.transport == "http" {
            client_info().serve(http(config)?).await
        } else {
            client_info().serve(stdio(config)?).await
        };
        client.map_err(|e| anyhow!("MCP 服务连接失败：{e}"))
    };
    bounded(fut, CONNECT_TIMEOUT, cancel, "MCP 服务连接超时").await
}

/// Close a finished connection (X:86-95).
pub async fn close(mut client: Client) {
    let _ = client.close_with_timeout(CLOSE_TIMEOUT).await;
}

/// One `tools/list` page: `(tools, nextCursor)` as wire JSON.
pub async fn list_page(client: &Client, cursor: Option<String>) -> Result<(Vec<Value>, Option<String>)> {
    let params = cursor.map(|c| PaginatedRequestParams::default().with_cursor(Some(c)));
    let fut = async { client.list_tools(params).await.map_err(|e| anyhow!("{e}")) };
    let page = bounded(fut, LIST_TIMEOUT, &CancellationToken::new(), "读取 MCP 工具目录超时").await?;
    let page = serde_json::to_value(page)?;
    let tools = page.get("tools").and_then(Value::as_array).cloned().unwrap_or_default();
    let next = page.get("nextCursor").and_then(Value::as_str).map(str::to_string);
    Ok((tools, next))
}

/// `client.callTool({name, arguments}, undefined, {signal, timeout: 60000})`;
/// the result is the wire `CallToolResult` JSON.
pub async fn call_tool(client: &Client, tool: &str, args: &Value, cancel: &CancellationToken) -> Result<Value> {
    let params: CallToolRequestParams = serde_json::from_value(json!({"name": tool, "arguments": args}))?;
    let fut = async { client.call_tool(params).await.map_err(|e| anyhow!("{e}")) };
    let result = bounded(fut, CALL_TIMEOUT, cancel, "MCP 调用超时").await?;
    Ok(serde_json::to_value(result)?)
}
