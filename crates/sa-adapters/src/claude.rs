//! Claude Code adapter (port of `runtime/adapters/claude.cjs`, spec §7.3).

use crate::common::{
    Emit, cc_rows, endpoint, fingerprint, get_json, local_host, modes, probe_failed, read_json_or,
    route_gone, scope, select, session_mismatch,
};
use crate::proc::{capture, start, stream};
use crate::sys::Sys;
use futures::future::{BoxFuture, join_all};
use regex::Regex;
use sa_engine::adapter::{Adapter, AdapterError, Catalog, ErrorClass, EventSink, RunOutcome, RunRequest, TurnHost};
use sa_engine::resilience::classify::{classified, classify_text, claude_api_retry};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

static DEFAULT_MODEL_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ANTHROPIC_DEFAULT_\w+_MODEL$").expect("valid regex"));

pub struct Claude {
    sys: Sys,
}

impl Claude {
    pub fn new(sys: Sys) -> Self {
        Self { sys }
    }
}

fn native_file(sys: &Sys) -> PathBuf {
    sys.var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| sys.home.join(".claude"))
        .join("settings.json")
}

/// `executable()` (CL:24-31).
pub fn executable(sys: &Sys) -> Option<PathBuf> {
    if let Some(cli) = sys.which(if cfg!(windows) { "claude.exe" } else { "claude" }) {
        return Some(cli);
    }
    let shim = sys.which("claude.cmd")?;
    let npm = shim.parent()?.join("node_modules/@anthropic-ai/claude-code/bin/claude.exe");
    npm.is_file().then_some(npm)
}

fn api_base(base: &str) -> String {
    let b = base.trim_end_matches('/');
    b.strip_suffix("/v1").unwrap_or(b).to_string()
}

/// `officialEnv()` (CL:33-51): first-party login without gateway overrides.
fn official_env(sys: &Sys) -> Sys {
    let mut env = sys.clone();
    if env.var("ANTHROPIC_BASE_URL").is_some() {
        env.remove("ANTHROPIC_API_KEY");
        env.remove("ANTHROPIC_AUTH_TOKEN");
    }
    for k in [
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_CUSTOM_HEADERS",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ] {
        env.remove(k);
    }
    let native = read_json_or(&native_file(sys));
    let native = &native["env"];
    if native["ANTHROPIC_BASE_URL"].as_str().is_none_or(str::is_empty) {
        for k in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"] {
            if let Some(v) = native[k].as_str().filter(|v| !v.is_empty()) {
                env.set(k, v);
            }
        }
    }
    env
}

/// One provider record (CL:102-160). `token` never leaves the adapter.
#[derive(Clone, Debug, Default)]
struct Record {
    app: String,
    row: String,
    name: String,
    base: String,
    auth: String,
    token: Option<String>,
    models: Vec<String>,
    preferred: String,
    reason: String,
}

impl Record {
    fn id(&self) -> String {
        fingerprint("ccs-", &[json!(self.app), json!(self.row), json!(self.base), json!(self.auth)])
    }
    fn route(&self) -> Value {
        let model = if self.models.contains(&self.preferred) {
            self.preferred.clone()
        } else {
            self.models.first().cloned().unwrap_or_default()
        };
        json!({"provider": self.id(), "model": model, "mode": "cli"})
    }
    fn same(&self, other: &Record) -> bool {
        self.app == other.app && self.row == other.row && self.base == other.base && self.auth == other.auth
    }
}

/// `firstParty(exe)` (CL:52-101).
async fn first_party(sys: &Sys, exe: Option<&PathBuf>, cancel: &CancellationToken) -> Record {
    let mut rec = Record {
        app: "native".into(),
        row: "first-party".into(),
        name: "官方账号".into(),
        auth: "sdk-login".into(),
        reason: "未验证官方登录或模型目录".into(),
        ..Default::default()
    };
    let Some(exe) = exe else { return rec };
    let env = official_env(sys);
    let flags = ["--safe-mode", "--setting-sources", ""].map(String::from);
    let mut status_args = flags.to_vec();
    status_args.extend(["auth", "status"].map(String::from));
    let Ok(out) = capture(exe, &status_args, &sys.temp, &env, "", Duration::from_secs(8), cancel).await else {
        return rec;
    };
    let Ok(status) = serde_json::from_str::<Value>(&out) else { return rec };
    if status["loggedIn"] != json!(true) || status["apiProvider"] != json!("firstParty") {
        return rec;
    }
    let msg = format!(
        "{}\n",
        json!({"type": "control_request", "request_id": "catalog", "request": {"subtype": "initialize"}})
    );
    let mut args = flags.to_vec();
    args.extend(
        [
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--strict-mcp-config",
            "--tools",
            "",
            "--no-session-persistence",
        ]
        .map(String::from),
    );
    if let Ok(buf) = capture(exe, &args, &sys.temp, &env, &msg, Duration::from_secs(8), cancel).await {
        for line in buf.trim().lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            let r = &v["response"];
            if r["request_id"] == json!("catalog") && r["subtype"] == json!("success") {
                rec.models = r["response"]["models"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| m["value"].as_str().filter(|s| !s.is_empty()).map(str::to_string))
                    .collect();
            }
        }
    }
    if !rec.models.is_empty() {
        rec.reason.clear();
    }
    rec
}

type ModelCache = Arc<AsyncMutex<HashMap<String, Option<Vec<String>>>>>;

/// `makeRecord(app, id, name, cfg, meta, cache)` (CL:102-160).
#[allow(clippy::too_many_arguments)]
async fn make_record(
    app: &str,
    id: &str,
    name: &str,
    cfg: &Value,
    meta: &Value,
    cache: &ModelCache,
    cancel: &CancellationToken,
) -> Record {
    let env = &cfg["env"];
    let base = env["ANTHROPIC_BASE_URL"].as_str().unwrap_or_default().trim_end_matches('/').to_string();
    let auth = if env["ANTHROPIC_AUTH_TOKEN"].as_str().is_some_and(|t| !t.is_empty()) {
        "ANTHROPIC_AUTH_TOKEN"
    } else {
        "ANTHROPIC_API_KEY"
    };
    let preferred = cfg["model"]
        .as_str()
        .filter(|m| !m.is_empty())
        .or_else(|| env["ANTHROPIC_MODEL"].as_str())
        .unwrap_or_default()
        .to_string();
    let mut rec = Record {
        app: app.into(),
        row: id.into(),
        name: name.into(),
        base: base.clone(),
        auth: auth.into(),
        token: env[auth].as_str().map(str::to_string),
        preferred,
        ..Default::default()
    };
    let attempt: anyhow::Result<()> = async {
        let url = endpoint(&base)?;
        let token = rec.token.clone().filter(|t| !t.is_empty()).ok_or_else(|| anyhow::anyhow!("缺少配对凭据"))?;
        if meta["apiFormat"].as_str().is_some_and(|f| !f.is_empty() && f != "anthropic") {
            anyhow::bail!("不兼容的供应商协议");
        }
        let mut entries: Vec<String> = vec![rec.preferred.clone()];
        for (k, v) in env.as_object().into_iter().flatten() {
            if (k == "ANTHROPIC_MODEL" || DEFAULT_MODEL_KEY.is_match(k))
                && let Some(s) = v.as_str()
            {
                entries.push(s.to_string());
            }
        }
        for r in meta["claudeDesktopModelRoutes"].as_object().into_iter().flatten().map(|(_, r)| r) {
            if let Some(m) = r["model"].as_str() {
                entries.push(m.to_string());
            }
        }
        let mut seen = HashSet::new();
        rec.models = entries
            .into_iter()
            .filter(|v| !v.is_empty() && v.encode_utf16().count() < 200 && seen.insert(v.clone()))
            .collect();
        if local_host(url.host_str().unwrap_or_default()) {
            let key = serde_json::to_string(&json!([base, auth, token]))?;
            let mut guard = cache.lock().await;
            if !guard.contains_key(&key) {
                let headers = if auth == "ANTHROPIC_API_KEY" {
                    vec![("x-api-key".to_string(), token.clone())]
                } else {
                    vec![("Authorization".to_string(), format!("Bearer {token}"))]
                };
                let found = get_json(&format!("{}/v1/models", api_base(&base)), &headers, cancel)
                    .await
                    .ok()
                    .and_then(|b| {
                        b["data"].as_array().map(|d| {
                            d.iter()
                                .filter_map(|m| m["id"].as_str())
                                .filter(|v| !v.is_empty() && v.encode_utf16().count() < 200)
                                .map(str::to_string)
                                .collect::<Vec<_>>()
                        })
                    });
                guard.insert(key.clone(), found);
            }
            if let Some(Some(discovered)) = guard.get(&key) {
                let set: BTreeSet<String> = discovered.iter().cloned().collect();
                rec.models = set.into_iter().collect();
            }
        }
        if rec.models.is_empty() {
            anyhow::bail!("没有可用模型");
        }
        Ok(())
    }
    .await;
    if attempt.is_err() {
        rec.reason = "端点、凭据、协议或模型不可用".into();
    }
    rec
}

struct Discovery {
    exe: Option<PathBuf>,
    records: Vec<Record>,
    bound: Value,
    catalog: Value,
}

/// `discover()` (CL:167-266).
async fn discover(sys: &Sys, cancel: &CancellationToken) -> anyhow::Result<Discovery> {
    let exe = executable(sys);
    let dir = sys.home.join(".cc-switch");
    let device = read_json_or(&dir.join("settings.json"));
    let state = read_json_or(&dir.join("live-state.json"))["apps"]["claude-desktop"].clone();
    let bound = read_json_or(&sys.platform_dir().join("claude-route.json"));
    let official = first_party(sys, exe.as_ref(), cancel).await;
    let cache: ModelCache = Arc::default();
    let rows = cc_rows(sys, &["claude", "claude-desktop"])?;
    let mut records: Vec<Record> = join_all(rows.iter().map(|row| {
        let (cache, official) = (&cache, &official);
        async move {
            let app = row["app_type"].as_str().unwrap_or_default();
            let id = match &row["id"] {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let name = row["name"].as_str().unwrap_or_default();
            let cfg: Option<Value> = row["settings_config"].as_str().and_then(|s| serde_json::from_str(s).ok());
            let meta: Option<Value> = match row["meta"].as_str() {
                Some(m) => serde_json::from_str(m).ok(),
                None => Some(json!({})),
            };
            let (Some(cfg), Some(meta)) = (cfg, meta) else {
                return Record {
                    app: app.into(),
                    row: id,
                    name: name.into(),
                    reason: "无效的来源记录".into(),
                    ..Default::default()
                };
            };
            let mut r = make_record(app, &id, name, &cfg, &meta, cache, cancel).await;
            if row["category"] == json!("official") && r.base.is_empty() {
                r.auth = official.auth.clone();
                r.models = official.models.clone();
                r.reason = official.reason.clone();
            }
            r
        }
    }))
    .await;
    let cfg = if sys.var("ANTHROPIC_BASE_URL").is_some() {
        let env: serde_json::Map<String, Value> =
            sys.vars().iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        json!({"env": env})
    } else {
        read_json_or(&native_file(sys))
    };
    let native_path = native_file(sys).to_string_lossy().into_owned();
    let mut native = make_record("native", &native_path, "当前 Claude Code 配置", &cfg, &json!({}), &cache, cancel).await;
    if !native.base.is_empty() {
        let matched = records.iter_mut().find(|r| {
            r.app == "claude" && r.reason.is_empty() && r.base == native.base && r.auth == native.auth && r.token == native.token
        });
        match matched {
            Some(m) => {
                if m.models.contains(&native.preferred) {
                    m.preferred = native.preferred.clone();
                }
                native = m.clone();
            }
            None => records.push(native.clone()),
        }
    } else {
        native = records
            .iter()
            .find(|r| r.app == "claude" && r.auth == "sdk-login")
            .cloned()
            .unwrap_or_else(|| official.clone());
        if !records.iter().any(|r| r.same(&native)) {
            records.push(native.clone());
        }
    }
    let direct = state["mode"].as_str().is_none_or(|m| m.is_empty() || m == "direct");
    let active_idx = if !direct {
        None
    } else if let Some(cur) = device["currentProviderClaudeDesktop"].as_str().filter(|c| !c.is_empty()) {
        records.iter().position(|r| r.app == "claude-desktop" && r.row == cur)
    } else {
        records.iter().position(|r| r.same(&native))
    };
    let mut legacy = serde_json::Map::new();
    if let Some(i) = active_idx {
        let a = &mut records[i];
        let bound_model = bound["model"].as_str().unwrap_or_default();
        if a.reason.is_empty()
            && bound["app_type"].as_str() == Some(&a.app)
            && bound["provider_id"].as_str() == Some(&a.row)
            && bound["base_url"].as_str().unwrap_or_default().trim_end_matches('/') == a.base
            && a.models.iter().any(|m| m == bound_model)
        {
            if a.preferred.is_empty() {
                a.preferred = bound_model.to_string();
            }
            let mut r = a.route();
            r["model"] = json!(bound_model);
            legacy.insert("cc-switch".into(), r);
        }
        if a.same(&native) {
            native.preferred = a.preferred.clone();
        }
    }
    if native.reason.is_empty() && !native.preferred.is_empty() && native.models.contains(&native.preferred) {
        legacy.insert("user".into(), native.route());
    }
    let providers: Vec<Value> = records
        .iter()
        .map(|r| {
            json!({
                "id": r.id(),
                "label": format!("{} · {}", r.name, r.app),
                "available": r.reason.is_empty(),
                "models": r.models.iter().map(|id| json!({"id": id, "label": id})).collect::<Vec<_>>(),
                "defaultModel": r.preferred,
                "detail": if r.reason.is_empty() { "同一记录的端点与认证".to_string() } else { r.reason.clone() },
            })
        })
        .collect();
    let default = match active_idx.map(|i| &records[i]) {
        Some(a) if a.reason.is_empty() => a.route(),
        _ => json!({"provider": "", "model": "", "mode": "cli"}),
    };
    let catalog = json!({
        "providers": providers,
        "default": default,
        "legacy_defaults": legacy,
        "modes": modes("Claude Code CLI", exe.is_some()),
        "detail": "CC Switch 已选直连来源及 Claude 原生配置",
        "downloads": [
            {"label": "Claude Code 安装", "url": "https://code.claude.com/docs/en/quickstart"},
            {"label": "Claude 桌面版", "url": "https://claude.com/download"}
        ]
    });
    Ok(Discovery {
        exe,
        records,
        bound,
        catalog,
    })
}

impl Adapter for Claude {
    fn agent(&self) -> &'static str {
        "claude"
    }

    fn catalog(&self) -> BoxFuture<'static, anyhow::Result<Catalog>> {
        let sys = self.sys.clone();
        Box::pin(async move {
            Ok(Catalog {
                value: discover(&sys, &CancellationToken::new()).await?.catalog,
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

fn strip_1m(m: &str) -> &str {
    m.strip_suffix("[1M]").unwrap_or(m)
}

/// `run(o)` (CL:267-356).
async fn run(
    sys: Sys,
    req: RunRequest,
    host: Arc<dyn TurnHost>,
    emit: Emit,
    cancel: CancellationToken,
) -> Result<RunOutcome, AdapterError> {
    let boundary = scope("claude", &req, &host, &cancel, false).await?;
    let high = boundary == "danger-full-access";
    let d = discover(&sys, &cancel).await.map_err(|e| probe_failed(&e.to_string()))?;
    let s = select(&d.catalog, &req.route)?;
    let r = d
        .records
        .iter()
        .find(|r| r.id() == s.provider)
        .cloned()
        .ok_or_else(|| route_gone("已选供应商或模型失效，请刷新配置"))?;
    let exe = d.exe.clone().ok_or_else(|| route_gone("本机未安装所选原生执行端"))?;
    let mut args: Vec<String> = [
        "-p",
        "--output-format",
        "stream-json",
        "--input-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
    ]
    .map(String::from)
    .to_vec();
    if high {
        args.extend(["--permission-mode", "bypassPermissions"].map(String::from));
    } else {
        args.extend(
            [
                "--safe-mode",
                "--strict-mcp-config",
                "--disable-slash-commands",
                "--no-chrome",
                "--tools",
                "Read,Glob,Grep",
                "--permission-mode",
                "dontAsk",
                "--setting-sources",
                "",
            ]
            .map(String::from),
        );
    }
    let sdk = r.auth == "sdk-login";
    let mut env = if sdk { official_env(&sys) } else { sys.clone() };
    if !sdk {
        env.retain(|k| {
            let u = k.to_ascii_uppercase();
            !(u.starts_with("ANTHROPIC_")
                || u.starts_with("CLAUDE_CODE_OAUTH")
                || matches!(
                    u.as_str(),
                    "CLAUDE_CODE_USE_BEDROCK" | "CLAUDE_CODE_USE_VERTEX" | "CLAUDE_CODE_USE_FOUNDRY" | "SUPERAGENT_CLAUDE_AUTH_TOKEN"
                ))
        });
        let base = api_base(&r.base);
        env.set("ANTHROPIC_BASE_URL", base.clone());
        env.set(&r.auth, r.token.clone().unwrap_or_default());
        let bound_base = api_base(d.bound["base_url"].as_str().unwrap_or_default());
        let local = endpoint(&r.base).is_ok_and(|u| local_host(u.host_str().unwrap_or_default()));
        if bound_base == base && local {
            let caps = env
                .get("CLAUDE_CODE_MODEL_CAPABILITIES")
                .map(|c| c.trim_end_matches(';').to_string())
                .filter(|c| !c.is_empty());
            let entry = format!("{}=-mid_conv_system", s.model);
            env.set(
                "CLAUDE_CODE_MODEL_CAPABILITIES",
                match caps {
                    Some(c) => format!("{c};{entry}"),
                    None => entry,
                },
            );
        }
    }
    if high {
        env.set("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", "1");
    }
    args.push("--model".into());
    args.push(s.model.clone());
    if let Some(sid) = &req.session_id {
        args.push("--resume".into());
        args.push(sid.clone());
    }
    emit.journal("adapter.permission", json!({"profile": req.profile, "scope": boundary}));
    let input = format!(
        "{}\n",
        json!({"type": "user", "message": {"role": "user", "content": req.prompt}})
    );
    let mut proc = start(&exe, &args, &req.cwd, &env)?;
    let mut terminal: Option<Value> = None;
    let mut last_retry: Option<Value> = None;
    let mut session: Option<String> = req.session_id.clone();
    let mut open_tools: HashSet<String> = HashSet::new();
    let streamed = stream(&mut proc, &input, &cancel, |e| {
        emit.journal("native", e.clone());
        if e["type"] == json!("stream_event") {
            if e["event"]["delta"]["type"] == json!("text_delta") {
                emit.journal("delta", e["event"]["delta"]["text"].clone());
            }
            if e["event"]["type"] == json!("content_block_stop") {
                emit.journal("message.boundary", Value::Null);
            }
        }
        track_tools(&e, &emit, &mut open_tools);
        if e["type"] == json!("system") && e["subtype"] == json!("init") {
            if let Some(id) = e["session_id"].as_str() {
                emit.session(id);
                session = Some(id.to_string());
            }
            let model = e["model"].as_str().map(str::to_string).unwrap_or_else(|| e["model"].to_string());
            if !sdk && strip_1m(&model) != strip_1m(&s.model) {
                return Err(route_gone("Claude 原生模型与所选路由不符"));
            }
        }
        if e["type"] == json!("system") && e["subtype"] == json!("api_retry") {
            if e["error"] == json!("authentication_failed") {
                let mut err = claude_api_retry(&e);
                err.message = "Claude 供应商拒绝认证".into();
                return Err(err);
            }
            let after = claude_api_retry(&e).retry_after;
            emit.native_retry(after);
            last_retry = Some(e.clone());
        }
        if e["type"] == json!("result") {
            terminal = Some(e);
        }
        Ok(())
    })
    .await;
    if let Err(mut err) = streamed {
        err.session_id = err.session_id.or(session);
        return Err(err);
    }
    let t = terminal.unwrap_or(Value::Null);
    let resumed_other = req
        .session_id
        .as_deref()
        .is_some_and(|sid| t["session_id"].as_str().is_some_and(|got| got != sid));
    if resumed_other {
        return Err(session_mismatch("Claude 未返回匹配会话的成功终态"));
    }
    let ok = t["is_error"] != json!(true)
        && t["subtype"] == json!("success")
        && t["result"].is_string()
        && t["session_id"].as_str().is_some_and(|s| !s.is_empty());
    if !ok {
        // The result text is only inspected for a class, never copied.
        let mut err = if let Some(retry) = &last_retry {
            claude_api_retry(retry)
        } else if let Some(text) = t["result"].as_str() {
            classify_text(text)
        } else if t.is_null() {
            classified(ErrorClass::ProcessCrashed, "claude:no_terminal")
        } else {
            classified(ErrorClass::Other, "claude:error_result")
        };
        err.message = "Claude 未返回匹配会话的成功终态".into();
        err.session_id = t["session_id"].as_str().map(str::to_string).or(session);
        return Err(err);
    }
    Ok(RunOutcome {
        text: t["result"].as_str().unwrap_or_default().to_string(),
        session_id: t["session_id"].as_str().map(str::to_string),
    })
}

/// `tool_use` blocks open a tool; matching `tool_result` blocks close it.
fn track_tools(e: &Value, emit: &Emit, open: &mut HashSet<String>) {
    let blocks = e["message"]["content"].as_array();
    for b in blocks.into_iter().flatten() {
        match b["type"].as_str() {
            Some("tool_use") if e["type"] == json!("assistant") => {
                if let Some(id) = b["id"].as_str()
                    && open.insert(id.to_string())
                {
                    emit.tool_started(id);
                }
            }
            Some("tool_result") if e["type"] == json!("user") => {
                if let Some(id) = b["tool_use_id"].as_str()
                    && open.remove(id)
                {
                    emit.tool_finished(id);
                }
            }
            _ => {}
        }
    }
}
