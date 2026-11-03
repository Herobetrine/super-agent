//! Shared adapter helpers (port of `runtime/adapters/common.cjs`).

use crate::proc::{cancelled, protocol_error};
use crate::sys::Sys;
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OpenFlags};
use sa_engine::adapter::{AdapterError, AdapterEvent, ErrorClass, EventSink, Route, RunRequest, TurnHost};
use sa_engine::resilience::classify::classified;
use sa_engine::resilience::{CODE_ROUTE_NOT_CONFIGURED, CODE_ROUTE_PROBE_FAILED};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use url::Url;

fn with_message(mut e: AdapterError, message: impl Into<String>) -> AdapterError {
    e.message = message.into();
    e
}

/// The selected provider/model is gone from the configuration (permanent).
pub fn route_gone(message: &str) -> AdapterError {
    with_message(classified(ErrorClass::RouteUnavailable, CODE_ROUTE_NOT_CONFIGURED), message)
}

/// Discovery failed transiently (retryable).
pub fn probe_failed(message: &str) -> AdapterError {
    with_message(classified(ErrorClass::RouteUnavailable, CODE_ROUTE_PROBE_FAILED), message)
}

pub fn denied(message: &str) -> AdapterError {
    with_message(classified(ErrorClass::Permission, "permission:denied"), message)
}

pub fn protocol(message: &str) -> AdapterError {
    protocol_error(message)
}

pub fn session_mismatch(message: &str) -> AdapterError {
    with_message(classified(ErrorClass::SessionMismatch, "session:mismatch"), message)
}

/// Adapter-side event sink.
#[derive(Clone)]
pub struct Emit(pub EventSink);

impl Emit {
    pub fn journal(&self, kind: &str, data: Value) {
        let _ = self.0.send(AdapterEvent::Journal {
            kind: kind.into(),
            data,
        });
    }
    pub fn session(&self, id: &str) {
        let _ = self.0.send(AdapterEvent::Session(id.into()));
    }
    pub fn tool_started(&self, id: &str) {
        let _ = self.0.send(AdapterEvent::ToolStarted(id.into()));
    }
    pub fn tool_finished(&self, id: &str) {
        let _ = self.0.send(AdapterEvent::ToolFinished(id.into()));
    }
    pub fn native_retry(&self, after: Option<Duration>) {
        let _ = self.0.send(AdapterEvent::NativeRetry { after });
    }
    /// The engine discarded the turn.
    pub fn closed(&self) -> bool {
        self.0.is_closed()
    }
}

/// `readJSON(file, fallback)` (AC:12-20): strict UTF-8 JSON, fallback when
/// missing.
pub fn read_json(path: &Path, fallback: Value) -> Result<Value> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let text = String::from_utf8(bytes).map_err(|_| anyhow!("本地 JSON 配置无法读取：{name}"))?;
            serde_json::from_str(&text).map_err(|_| anyhow!("本地 JSON 配置无法读取：{name}"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(fallback),
        Err(_) => bail!(
            "本地 JSON 配置无法读取：{}",
            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        ),
    }
}

/// `readJSON` that treats unreadable files like missing ones.
pub fn read_json_or(path: &Path) -> Value {
    read_json(path, json!({})).unwrap_or_else(|_| json!({}))
}

pub fn is_file(p: &Path) -> bool {
    p.is_file()
}

/// CC Switch database path (AC:41-51).
fn cc_database(sys: &Sys) -> Result<PathBuf> {
    let locator = sys.appdata().join("com.ccswitch.desktop/app_paths.json");
    let override_dir = read_json(&locator, json!({}))?
        .get("app_config_dir_override")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let folder = override_dir.unwrap_or_else(|| sys.home.join(".cc-switch"));
    if !folder.is_absolute() {
        bail!("CC Switch 数据目录不是绝对路径");
    }
    Ok(folder.join("cc-switch.db"))
}

/// `ccRows(...apps)` (AC:66-89): provider rows, read-only.
pub fn cc_rows(sys: &Sys, apps: &[&str]) -> Result<Vec<Map<String, Value>>> {
    let file = cc_database(sys)?;
    if !file.is_file() {
        return Ok(vec![]);
    }
    let db = Connection::open_with_flags(&file, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .context("CC Switch 数据库无法打开")?;
    db.busy_timeout(Duration::from_millis(2000))?;
    let cols: Vec<String> = db
        .prepare("PRAGMA table_info(providers)")?
        .query_map([], |r| r.get::<_, String>("name"))?
        .collect::<rusqlite::Result<_>>()?;
    let optional: Vec<String> = ["meta", "category"]
        .iter()
        .map(|c| if cols.iter().any(|x| x == c) { c.to_string() } else { format!("NULL AS {c}") })
        .collect();
    let marks = vec!["?"; apps.len()].join(",");
    let sql = format!(
        "SELECT app_type,id,name,settings_config,is_current,{} FROM providers WHERE app_type IN ({marks}) ORDER BY app_type,name",
        optional.join(",")
    );
    let mut stmt = db.prepare(&sql)?;
    let names: Vec<String> = stmt.column_names().map(|s| s.to_string()).collect();
    let rows = stmt.query_map(rusqlite::params_from_iter(apps.iter()), |r| {
        let mut m = Map::new();
        for (i, n) in names.iter().enumerate() {
            let v = match r.get_ref(i)? {
                rusqlite::types::ValueRef::Null => Value::Null,
                rusqlite::types::ValueRef::Integer(x) => json!(x),
                rusqlite::types::ValueRef::Real(x) => json!(x),
                rusqlite::types::ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                rusqlite::types::ValueRef::Blob(_) => Value::Null,
            };
            m.insert(n.clone(), v);
        }
        Ok(m)
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// `fingerprint(prefix, parts)` (AC:90-105): Python-compatible ASCII JSON.
pub fn fingerprint(prefix: &str, parts: &[Value]) -> String {
    let ascii: Vec<String> = parts
        .iter()
        .map(|v| {
            let s = serde_json::to_string(v).unwrap_or_default();
            let mut out = String::with_capacity(s.len());
            for unit in s.encode_utf16() {
                if unit >= 0x7f {
                    out.push_str(&format!("\\u{unit:04x}"));
                } else {
                    out.push(unit as u8 as char);
                }
            }
            out
        })
        .collect();
    let digest = format!("{:x}", Sha256::digest(format!("[{}]", ascii.join(", ")).as_bytes()));
    format!("{prefix}{}", &digest[..24])
}

pub fn local_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "[::1]" | "::1" | "localhost")
}

/// `endpoint(base)` (AC:107-123): https, or http on a loopback host; no
/// credentials, query or fragment.
pub fn endpoint(base: &str) -> Result<Url> {
    let u = Url::parse(base).map_err(|_| anyhow!("无效的供应商端点"))?;
    let host = u.host_str().unwrap_or_default().to_string();
    let ok = u.username().is_empty()
        && u.password().is_none()
        && u.query().is_none_or(str::is_empty)
        && u.fragment().is_none_or(str::is_empty)
        && (u.scheme() == "https" || (u.scheme() == "http" && local_host(&host)));
    if !ok {
        bail!("不支持的供应商端点");
    }
    Ok(u)
}

/// `getJSON(url, headers)` (AC:124-151) for loopback catalogs: a direct
/// HTTP/1.1 GET with no proxy and no redirects, 3.5 s, 1 MiB.
pub async fn get_json(url: &str, headers: &[(String, String)], cancel: &CancellationToken) -> Result<Value> {
    let u = Url::parse(url).map_err(|_| anyhow!("无效的目录地址"))?;
    let host = u.host_str().unwrap_or_default().to_string();
    if u.scheme() != "http" || !local_host(&host) {
        // Remote HTTPS catalogs are not fetched by the Rust port yet.
        bail!("目录仅支持本机 HTTP 端点");
    }
    for (k, v) in headers {
        if k.contains(['\r', '\n', ':']) || v.contains(['\r', '\n']) {
            bail!("目录请求头无效");
        }
    }
    let port = u.port().unwrap_or(80);
    let path = match u.query() {
        Some(q) => format!("{}?{q}", u.path()),
        None => u.path().to_string(),
    };
    let authority = if u.port().is_some() { format!("{host}:{port}") } else { host.clone() };
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nAccept: application/json\r\nConnection: close\r\n");
    for (k, v) in headers {
        request.push_str(&format!("{k}: {v}\r\n"));
    }
    request.push_str("\r\n");
    let connect_host = host.trim_start_matches('[').trim_end_matches(']').to_string();
    let fetch = async move {
        let mut stream = tokio::net::TcpStream::connect((connect_host.as_str(), port)).await?;
        stream.write_all(request.as_bytes()).await?;
        let mut raw = Vec::new();
        let mut chunk = vec![0u8; 16 * 1024];
        loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..n]);
            if raw.len() > 1024 * 1024 + 64 * 1024 {
                bail!("目录超过 1 MiB");
            }
        }
        anyhow::Ok(raw)
    };
    let raw = tokio::select! {
        r = tokio::time::timeout(Duration::from_millis(3500), fetch) => r.map_err(|_| anyhow!("目录请求超时"))??,
        () = cancel.cancelled() => bail!("已取消"),
    };
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("目录响应无效"))?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| anyhow!("目录响应无效"))?;
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("目录响应无效"))?;
    if !(200..300).contains(&status) {
        bail!("目录 HTTP {status}");
    }
    let chunked = lines.any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let body = &raw[split + 4..];
    let body = if chunked { dechunk(body)? } else { body.to_vec() };
    if body.len() > 1024 * 1024 {
        bail!("目录超过 1 MiB");
    }
    let text = String::from_utf8(body).map_err(|_| anyhow!("目录不是 UTF-8"))?;
    Ok(serde_json::from_str(&text)?)
}

fn dechunk(mut body: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let end = body.windows(2).position(|w| w == b"\r\n").ok_or_else(|| anyhow!("分块响应无效"))?;
        let size_text = std::str::from_utf8(&body[..end])?;
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)?;
        body = &body[end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if body.len() < size + 2 {
            bail!("分块响应无效");
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

pub fn high(profile: &str) -> bool {
    matches!(profile, "full" | "auto")
}

/// `scope(agent, o, writer)` (AC:320-346): the sandbox for this turn.
pub async fn scope(
    agent: &str,
    req: &RunRequest,
    host: &Arc<dyn TurnHost>,
    cancel: &CancellationToken,
    writer: bool,
) -> Result<&'static str, AdapterError> {
    let profile = req.profile.as_str();
    if !matches!(profile, "legacy" | "readonly" | "ask" | "partial" | "full" | "auto") {
        return Err(denied("未知权限档位"));
    }
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    if high(profile) {
        if !req.writable {
            return Err(denied("完全访问与只读授权冲突"));
        }
        return Ok("danger-full-access");
    }
    if !req.writable {
        return Ok("read-only");
    }
    if profile == "readonly" || !writer {
        return Err(denied(&format!("{agent} 未获受控写入权限")));
    }
    if profile == "ask" {
        let action = json!({
            "agent": agent,
            "action": "workspace_write",
            "cwd": req.cwd,
            "scope": "current_turn",
            "summary": "允许本轮在项目工作区写入并执行命令？",
        });
        if !host.approve(action).await {
            return Err(denied("本轮工作区写入未获批准"));
        }
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
    }
    Ok("workspace-write")
}

/// `select(catalog, request)` (AC:347-361).
pub fn select(catalog: &Value, request: &Route) -> Result<Route, AdapterError> {
    let mode_ok = request.mode == "cli"
        && catalog["modes"]
            .as_array()
            .is_some_and(|m| m.iter().any(|m| m["id"] == json!(request.mode) && m["available"] == json!(true)));
    if !mode_ok {
        return Err(route_gone("所选执行端不可自动执行"));
    }
    let provider = catalog["providers"].as_array().and_then(|ps| {
        ps.iter()
            .find(|p| p["id"] == json!(request.provider) && p["available"] != json!(false))
    });
    let listed = provider.is_some_and(|p| {
        p["models"]
            .as_array()
            .is_some_and(|ms| ms.iter().any(|m| m["id"] == json!(request.model) && m["available"] != json!(false)))
    });
    if !listed {
        return Err(route_gone("已选供应商或模型失效，请刷新配置"));
    }
    Ok(request.clone())
}

/// `modes(label, available)` (AC:362-367).
pub fn modes(label: &str, available: bool) -> Value {
    json!([
        {"id": "cli", "label": label, "available": available, "detail": "原生 CLI 协议"},
        {"id": "desktop", "label": "桌面应用", "available": false, "detail": "桌面应用未打通自动执行和回传"}
    ])
}
