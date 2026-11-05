//! Shared adapter helpers (port of `runtime/adapters/common.cjs`).

use crate::proc::{cancelled, protocol_error};
use crate::sys::Sys;
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OpenFlags};
use sa_engine::adapter::{
    AdapterError, AdapterEvent, ErrorClass, EventSink, Route, RunRequest, TurnHost,
};
use sa_engine::resilience::classify::classified;
use sa_engine::resilience::{CODE_ROUTE_NOT_CONFIGURED, CODE_ROUTE_PROBE_FAILED};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

fn with_message(mut e: AdapterError, message: impl Into<String>) -> AdapterError {
    e.message = message.into();
    e
}

/// The selected provider/model is gone from the configuration (permanent).
pub fn route_gone(message: &str) -> AdapterError {
    with_message(
        classified(ErrorClass::RouteUnavailable, CODE_ROUTE_NOT_CONFIGURED),
        message,
    )
}

/// Discovery failed transiently (retryable).
pub fn probe_failed(message: &str) -> AdapterError {
    with_message(
        classified(ErrorClass::RouteUnavailable, CODE_ROUTE_PROBE_FAILED),
        message,
    )
}

pub fn denied(message: &str) -> AdapterError {
    with_message(
        classified(ErrorClass::Permission, "permission:denied"),
        message,
    )
}

pub fn protocol(message: &str) -> AdapterError {
    protocol_error(message)
}

pub fn session_mismatch(message: &str) -> AdapterError {
    with_message(
        classified(ErrorClass::SessionMismatch, "session:mismatch"),
        message,
    )
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
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let text =
                String::from_utf8(bytes).map_err(|_| anyhow!("本地 JSON 配置无法读取：{name}"))?;
            serde_json::from_str(&text).map_err(|_| anyhow!("本地 JSON 配置无法读取：{name}"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(fallback),
        Err(_) => bail!(
            "本地 JSON 配置无法读取：{}",
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
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

/// `ccWatchPaths()` (AC:52-65): the locator and, when its override is usable,
/// the database and its WAL.
pub fn cc_watch_paths(sys: &Sys) -> Vec<PathBuf> {
    let locator = sys.appdata().join("com.ccswitch.desktop/app_paths.json");
    let mut paths = vec![locator.clone()];
    let over = read_json_or(&locator)["app_config_dir_override"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    if over.as_ref().is_none_or(|p| p.is_absolute()) {
        let db = over
            .unwrap_or_else(|| sys.home.join(".cc-switch"))
            .join("cc-switch.db");
        paths.push(db.with_file_name("cc-switch.db-wal"));
        paths.insert(1, db);
    }
    paths
}

/// `ccRows(...apps)` (AC:66-89): provider rows, read-only.
pub fn cc_rows(sys: &Sys, apps: &[&str]) -> Result<Vec<Map<String, Value>>> {
    let file = cc_database(sys)?;
    if !file.is_file() {
        return Ok(vec![]);
    }
    let db = Connection::open_with_flags(
        &file,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("CC Switch 数据库无法打开")?;
    db.busy_timeout(Duration::from_millis(2000))?;
    let cols: Vec<String> = db
        .prepare("PRAGMA table_info(providers)")?
        .query_map([], |r| r.get::<_, String>("name"))?
        .collect::<rusqlite::Result<_>>()?;
    let optional: Vec<String> = ["meta", "category"]
        .iter()
        .map(|c| {
            if cols.iter().any(|x| x == c) {
                c.to_string()
            } else {
                format!("NULL AS {c}")
            }
        })
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
    let digest = format!(
        "{:x}",
        Sha256::digest(format!("[{}]", ascii.join(", ")).as_bytes())
    );
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

/// Catalog request limits (AC:124-151).
pub const CATALOG_TIMEOUT: Duration = Duration::from_millis(3500);
pub const CATALOG_LIMIT: u64 = 1024 * 1024;

/// One shared client: no environment proxy and no redirects (a proxy or a
/// redirect must never carry the catalog credentials elsewhere), HTTP status
/// checked by the caller, server certificates verified against the operating
/// system trust store.
static HTTP: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .user_agent("SuperAgent")
        .build()
        .into()
});

/// `getJSON(url, headers)` (AC:124-151): https, or http on a loopback host;
/// direct connection, no redirects, 3.5 s, 1 MiB.
pub async fn get_json(
    url: &str,
    headers: &[(String, String)],
    cancel: &CancellationToken,
) -> Result<Value> {
    get_json_with(url, headers, CATALOG_TIMEOUT, CATALOG_LIMIT, cancel).await
}

/// [`get_json`] with an explicit deadline and body limit.
pub async fn get_json_with(
    url: &str,
    headers: &[(String, String)],
    timeout: Duration,
    limit: u64,
    cancel: &CancellationToken,
) -> Result<Value> {
    let u = Url::parse(url).map_err(|_| anyhow!("无效的目录地址"))?;
    let host = u.host_str().unwrap_or_default().to_string();
    let scheme_ok = u.scheme() == "https" || (u.scheme() == "http" && local_host(&host));
    if !scheme_ok || host.is_empty() || !u.username().is_empty() || u.password().is_some() {
        bail!("目录仅支持 HTTPS 或本机 HTTP 端点");
    }
    for (k, v) in headers {
        if k.is_empty() || k.contains(['\r', '\n', ':']) || v.contains(['\r', '\n']) {
            bail!("目录请求头无效");
        }
    }
    let target = u.to_string();
    let headers = headers.to_vec();
    let fetch = tokio::task::spawn_blocking(move || -> Result<(u16, Vec<u8>)> {
        let mut req = HTTP.get(&target).header("Accept", "application/json");
        for (k, v) in &headers {
            req = req.header(k.as_str(), v.as_str());
        }
        // ureq errors may echo the URL but never request headers.
        let resp = req
            .config()
            .timeout_global(Some(timeout))
            .build()
            .call()
            .map_err(|e| match e {
                ureq::Error::Timeout(_) => anyhow!("目录请求超时"),
                ureq::Error::Tls(_) | ureq::Error::Rustls(_) => anyhow!("目录 TLS 握手失败"),
                _ => anyhow!("目录请求失败"),
            })?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Ok((status, vec![]));
        }
        let body = resp
            .into_body()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map_err(|e| match e {
                ureq::Error::BodyExceedsLimit(_) => anyhow!("目录超过 {} KiB", limit / 1024),
                ureq::Error::Timeout(_) => anyhow!("目录请求超时"),
                _ => anyhow!("目录响应无效"),
            })?;
        Ok((status, body))
    });
    let (status, body) = tokio::select! {
        r = fetch => r.map_err(|_| anyhow!("目录请求失败"))??,
        () = cancel.cancelled() => bail!("已取消"),
    };
    if !(200..300).contains(&status) {
        bail!("目录 HTTP {status}");
    }
    let text = String::from_utf8(body).map_err(|_| anyhow!("目录不是 UTF-8"))?;
    serde_json::from_str(&text).map_err(|_| anyhow!("目录不是有效 JSON"))
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
    if !matches!(
        profile,
        "legacy" | "readonly" | "ask" | "partial" | "full" | "auto"
    ) {
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
        && catalog["modes"].as_array().is_some_and(|m| {
            m.iter()
                .any(|m| m["id"] == json!(request.mode) && m["available"] == json!(true))
        });
    if !mode_ok {
        return Err(route_gone("所选执行端不可自动执行"));
    }
    let provider = catalog["providers"].as_array().and_then(|ps| {
        ps.iter()
            .find(|p| p["id"] == json!(request.provider) && p["available"] != json!(false))
    });
    let listed = provider.is_some_and(|p| {
        p["models"].as_array().is_some_and(|ms| {
            ms.iter()
                .any(|m| m["id"] == json!(request.model) && m["available"] != json!(false))
        })
    });
    if !listed {
        return Err(route_gone("已选供应商或模型失效，请刷新配置"));
    }
    if let Some(effort) = &request.reasoning_effort {
        let model = provider
            .and_then(|p| p["models"].as_array())
            .and_then(|ms| ms.iter().find(|m| m["id"] == request.model));
        if model.is_none_or(|m| !crate::catalog::efforts(m).contains(effort)) {
            return Err(route_gone("所选模型未声明支持该思考强度，请刷新模型目录"));
        }
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
