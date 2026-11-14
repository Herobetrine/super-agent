//! Claude Code adapter (port of `runtime/adapters/claude.cjs`, spec §7.3).

use crate::common::{
    Emit, cc_rows, endpoint, fingerprint, local_host, modes, probe_failed, read_json, read_json_or,
    route_gone, scope, select, session_mismatch,
};
use crate::custom::{self, CustomProvider, Registry};
use crate::proc::{Proc, capture, start, stream};
use crate::sys::Sys;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared, join_all};
use regex::Regex;
use sa_engine::adapter::{
    Adapter, AdapterError, Catalog, ErrorClass, EventSink, RunOutcome, RunRequest, TurnHost,
};
use sa_engine::resilience::classify::{
    classified, classify_text, claude_api_retry, from_http_status,
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

static DEFAULT_MODEL_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ANTHROPIC_DEFAULT_\w+_MODEL$").expect("valid regex"));

pub struct Claude {
    sys: Sys,
    registry: Registry,
    efficiency: sa_store::efficiency::Shared,
}

impl Claude {
    pub fn new(sys: Sys) -> Self {
        Self::with_registry(sys, custom::registry(vec![]))
    }

    pub fn with_registry(sys: Sys, registry: Registry) -> Self {
        Self::with_efficiency(
            sys,
            registry,
            sa_store::efficiency::shared(Default::default()),
        )
    }

    pub fn with_efficiency(
        sys: Sys,
        registry: Registry,
        efficiency: sa_store::efficiency::Shared,
    ) -> Self {
        Self {
            sys,
            registry,
            efficiency,
        }
    }
}

pub(crate) fn native_file(sys: &Sys) -> PathBuf {
    sys.var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| sys.home.join(".claude"))
        .join("settings.json")
}

/// `executable()` (CL:24-31).
pub fn executable(sys: &Sys) -> Option<PathBuf> {
    if let Some(cli) = sys.which(if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    }) {
        return Some(cli);
    }
    let shim = sys.which("claude.cmd")?;
    let npm = shim
        .parent()?
        .join("node_modules/@anthropic-ai/claude-code/bin/claude.exe");
    npm.is_file().then_some(npm)
}

fn api_base(base: &str) -> String {
    custom::anthropic_root(base)
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
    if native["ANTHROPIC_BASE_URL"]
        .as_str()
        .is_none_or(str::is_empty)
    {
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
    model_info: Vec<Value>,
    catalog_error: Option<String>,
    preferred: String,
    reason: String,
    cache_settings: Value,
}

impl Record {
    fn id(&self) -> String {
        fingerprint(
            "ccs-",
            &[
                json!(self.app),
                json!(self.row),
                json!(self.base),
                json!(self.auth),
            ],
        )
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
        self.app == other.app
            && self.row == other.row
            && self.base == other.base
            && self.auth == other.auth
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
    let Ok(out) = capture(
        exe,
        &status_args,
        &sys.temp,
        &env,
        "",
        Duration::from_secs(8),
        cancel,
    )
    .await
    else {
        return rec;
    };
    let Ok(status) = serde_json::from_str::<Value>(&out) else {
        return rec;
    };
    let logged_in = status["loggedIn"] == true && status["apiProvider"] == "firstParty";
    if status["loggedIn"] == false {
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
    if let Ok(buf) = capture(
        exe,
        &args,
        &sys.temp,
        &env,
        &msg,
        Duration::from_secs(8),
        cancel,
    )
    .await
    {
        for line in buf.trim().lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let r = &v["response"];
            if r["request_id"] == json!("catalog") && r["subtype"] == json!("success") {
                rec.models = r["response"]["models"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| {
                        m["value"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                    })
                    .collect();
                rec.model_info = r["response"]["models"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(crate::catalog::model)
                    .collect();
            }
        }
    }
    if logged_in && !rec.models.is_empty() {
        rec.reason.clear();
    }
    rec
}

type ModelProbe = Shared<BoxFuture<'static, Result<Vec<Value>, String>>>;
type ModelCache = Arc<AsyncMutex<HashMap<(String, String, String), ModelProbe>>>;

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
    let base = env["ANTHROPIC_BASE_URL"]
        .as_str()
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_string();
    let auth = if env["ANTHROPIC_AUTH_TOKEN"]
        .as_str()
        .is_some_and(|t| !t.is_empty())
    {
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
        cache_settings: crate::cache_policy::selected_settings(cfg),
        ..Default::default()
    };
    let attempt: anyhow::Result<()> = async {
        endpoint(&base)?;
        let token = rec
            .token
            .clone()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| anyhow::anyhow!("缺少配对凭据"))?;
        if meta["apiFormat"]
            .as_str()
            .is_some_and(|f| !f.is_empty() && f != "anthropic")
        {
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
        for r in meta["claudeDesktopModelRoutes"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(_, r)| r)
        {
            if let Some(m) = r["model"].as_str() {
                entries.push(m.to_string());
            }
        }
        let mut seen = HashSet::new();
        rec.models = entries
            .into_iter()
            .filter(|v| !v.is_empty() && v.encode_utf16().count() < 200 && seen.insert(v.clone()))
            .collect();
        {
            let url = format!("{}/v1/models?limit=1000", api_base(&base));
            let key = (url.clone(), auth.to_string(), token.clone());
            let probe = {
                let mut probes = cache.lock().await;
                probes
                    .entry(key)
                    .or_insert_with(|| {
                        let cancel = cancel.clone();
                        async move {
                            let mut headers = if auth == "ANTHROPIC_API_KEY" {
                                vec![("x-api-key".to_string(), token)]
                            } else {
                                vec![("Authorization".to_string(), format!("Bearer {token}"))]
                            };
                            headers.push(("anthropic-version".into(), "2023-06-01".into()));
                            crate::catalog::fetch(&url, &headers, &cancel)
                                .await
                                .map_err(|e| e.to_string())
                        }
                        .boxed()
                        .shared()
                    })
                    .clone()
            };
            match probe.await {
                Ok(discovered) => {
                    rec.model_info = discovered.clone();
                    for m in discovered {
                        if let Some(id) = m["id"].as_str()
                            && !rec.models.iter().any(|m| m == id)
                        {
                            rec.models.push(id.to_string());
                        }
                    }
                }
                Err(error) => rec.catalog_error = Some(error),
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
async fn discover(
    sys: &Sys,
    exe: Option<PathBuf>,
    cancel: &CancellationToken,
) -> anyhow::Result<Discovery> {
    let dir = sys.home.join(".cc-switch");
    let device = read_json_or(&dir.join("settings.json"));
    let state = read_json_or(&dir.join("live-state.json"))["apps"]["claude-desktop"].clone();
    let bound = read_json_or(&sys.platform_dir().join("claude-route.json"));
    let official_probe = first_party(sys, exe.as_ref(), cancel).shared();
    let cache: ModelCache = Arc::default();
    let rows = cc_rows(sys, &["claude", "claude-desktop"])?;
    let records = join_all(rows.iter().map(|row| {
        let (cache, official) = (&cache, official_probe.clone());
        async move {
            let app = row["app_type"].as_str().unwrap_or_default();
            let id = match &row["id"] {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let name = row["name"].as_str().unwrap_or_default();
            let cfg: Option<Value> = row["settings_config"]
                .as_str()
                .and_then(|s| serde_json::from_str(s).ok());
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
            let mut record = make_record(app, &id, name, &cfg, &meta, cache, cancel).await;
            if row["category"] == json!("official") && record.base.is_empty() {
                let official = official.await;
                record.auth = official.auth;
                record.models = official.models;
                record.model_info = official.model_info;
                record.reason = official.reason;
            }
            record
        }
    }));
    let cfg = if sys.var("ANTHROPIC_BASE_URL").is_some() {
        let env: serde_json::Map<String, Value> = sys
            .vars()
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        json!({"env": env})
    } else {
        read_json_or(&native_file(sys))
    };
    let native_path = native_file(sys).to_string_lossy().into_owned();
    let meta = json!({});
    let native = make_record(
        "native",
        &native_path,
        "当前 Claude Code 配置",
        &cfg,
        &meta,
        &cache,
        cancel,
    );
    let (official, mut records, mut native) =
        tokio::join!(official_probe.clone(), records, native,);
    drop(official_probe);
    if !native.base.is_empty() {
        let matched = records.iter_mut().find(|r| {
            r.app == "claude"
                && r.reason.is_empty()
                && r.base == native.base
                && r.auth == native.auth
                && r.token == native.token
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
    let direct = state["mode"]
        .as_str()
        .is_none_or(|m| m.is_empty() || m == "direct");
    let active_idx = if !direct {
        None
    } else if let Some(cur) = device["currentProviderClaudeDesktop"]
        .as_str()
        .filter(|c| !c.is_empty())
    {
        records
            .iter()
            .position(|r| r.app == "claude-desktop" && r.row == cur)
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
            && bound["base_url"]
                .as_str()
                .unwrap_or_default()
                .trim_end_matches('/')
                == a.base
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
    if native.reason.is_empty()
        && !native.preferred.is_empty()
        && native.models.contains(&native.preferred)
    {
        legacy.insert("user".into(), native.route());
    }
    for record in &mut records {
        let mut rows: Vec<Value> = record
            .models
            .iter()
            .map(|id| {
                record
                    .model_info
                    .iter()
                    .find(|m| m["id"] == *id)
                    .cloned()
                    .unwrap_or_else(|| json!({"id":id,"label":id}))
            })
            .collect();
        crate::catalog::supplement(&mut rows, &official.model_info);
        crate::catalog::restrict_efforts(&mut rows, &["low", "medium", "high", "xhigh", "max"]);
        record.model_info = rows;
    }
    let providers: Vec<Value> = records
        .iter()
        .map(|r| {
            json!({
                "id": r.id(),
                "label": format!("{} · {}", r.name, r.app),
                "available": r.reason.is_empty(),
                "models": r.model_info,
                "defaultModel": r.preferred,
                "catalogError": r.catalog_error.is_some(),
                "catalog_complete": r.catalog_error.is_none() && r.reason.is_empty(),
                "detail": if let Some(error) = &r.catalog_error { format!("在线目录获取失败：{error}；以下仅为已保存型号") } else if r.reason.is_empty() { "同源认证的在线模型目录".to_string() } else { r.reason.clone() },
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
        let customs = custom::snapshot(&self.registry, "claude");
        Box::pin(async move {
            let mut value = discover(&sys, executable(&sys), &CancellationToken::new())
                .await?
                .catalog;
            custom::refresh(&mut value, &customs, &sys, &CancellationToken::new()).await;
            Ok(Catalog { value })
        })
    }

    fn run(
        &self,
        request: RunRequest,
        host: Arc<dyn TurnHost>,
        events: EventSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<RunOutcome, AdapterError>> {
        let mut sys = self.sys.clone();
        let ttl = self
            .efficiency
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .claude_cache_ttl;
        crate::cache_policy::select_fallback(&mut sys, ttl);
        let customs = custom::snapshot(&self.registry, "claude");
        Box::pin(async move { run(sys, customs, request, host, Emit(events), cancel).await })
    }
}

fn strip_1m(m: &str) -> &str {
    m.strip_suffix("[1M]").unwrap_or(m)
}

const MODEL_CAPABILITIES: &str = "CLAUDE_CODE_MODEL_CAPABILITIES";

fn local_gateway_settings(
    env: &Sys,
    cwd: &Path,
    high: bool,
    model: &str,
) -> Result<Value, AdapterError> {
    let mut capabilities = env.get(MODEL_CAPABILITIES).unwrap_or_default().to_owned();
    // Restricted calls already pass --setting-sources "" and must keep that boundary.
    if high {
        let mut paths = vec![native_file(env)];
        for directory in cwd.ancestors().collect::<Vec<_>>().into_iter().rev() {
            paths.push(directory.join(".claude/settings.json"));
            paths.push(directory.join(".claude/settings.local.json"));
        }
        for path in paths {
            let source = read_json(&path, Value::Null).map_err(|_| {
                route_gone("Claude 本地网关兼容设置无法读取现有 settings，已停止本次调用")
            })?;
            if let Some(value) = source["env"].get(MODEL_CAPABILITIES) {
                capabilities = value
                    .as_str()
                    .ok_or_else(|| route_gone("Claude 模型能力设置必须是字符串，已停止本次调用"))?
                    .to_owned();
            }
        }
    }
    // CLI 2.1.317 merges matching entries per capability in order, including
    // duplicate exact selectors and wildcards. Append only the selected override
    // so all other effective capability tokens and unrelated entries survive.
    let entry = format!("{model}=-mid_conv_system");
    let capabilities = capabilities.trim_end_matches(';');
    let merged = if capabilities.is_empty() {
        entry
    } else {
        format!("{capabilities};{entry}")
    };
    Ok(json!({"env":{MODEL_CAPABILITIES:merged}}))
}

/// `run(o)` (CL:267-356).
async fn run(
    sys: Sys,
    customs: Vec<CustomProvider>,
    req: RunRequest,
    host: Arc<dyn TurnHost>,
    emit: Emit,
    cancel: CancellationToken,
) -> Result<RunOutcome, AdapterError> {
    crate::images::check(&req)?;
    let boundary = scope("claude", &req, &host, &cancel, false).await?;
    let high = boundary == "danger-full-access";
    let mut d = discover(&sys, executable(&sys), &cancel)
        .await
        .map_err(|e| probe_failed(&e.to_string()))?;
    custom::refresh(&mut d.catalog, &customs, &sys, &cancel).await;
    let s = select(&d.catalog, &req.route)?;
    let chosen = custom::lookup(&d.catalog, &customs, &s.provider).cloned();
    let r = match &chosen {
        Some(_) => Record::default(),
        None => d
            .records
            .iter()
            .find(|r| r.id() == s.provider)
            .cloned()
            .ok_or_else(|| route_gone("已选供应商或模型失效，请刷新配置"))?,
    };
    let exe = d
        .exe
        .clone()
        .ok_or_else(|| route_gone("本机未安装所选原生执行端"))?;
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
    let sdk = chosen.is_none() && r.auth == "sdk-login";
    let mut env = match &chosen {
        // Endpoint, bearer token and model aliases come from the operator's
        // provider; the key travels only in the child environment.
        Some(p) => custom::claude_env(p, &s.model, &sys)
            .map_err(|_| route_gone("自定义供应商的密钥环境变量未设置"))?,
        None if sdk => official_env(&sys),
        None => sys.clone(),
    };
    if !sdk && chosen.is_none() {
        env.retain(|k| {
            let u = k.to_ascii_uppercase();
            !(u.starts_with("ANTHROPIC_")
                || u.starts_with("CLAUDE_CODE_OAUTH")
                || matches!(
                    u.as_str(),
                    "CLAUDE_CODE_USE_BEDROCK"
                        | "CLAUDE_CODE_USE_VERTEX"
                        | "CLAUDE_CODE_USE_FOUNDRY"
                        | "SUPERAGENT_CLAUDE_AUTH_TOKEN"
                ))
        });
        let base = api_base(&r.base);
        env.set("ANTHROPIC_BASE_URL", base.clone());
        env.set(&r.auth, r.token.clone().unwrap_or_default());
        let bound_base = api_base(d.bound["base_url"].as_str().unwrap_or_default());
        let local = endpoint(&r.base).is_ok_and(|u| local_host(u.host_str().unwrap_or_default()));
        if bound_base == base && local {
            let settings = local_gateway_settings(&env, &req.cwd, high, &s.model)?;
            let capabilities = settings["env"][MODEL_CAPABILITIES]
                .as_str()
                .unwrap_or_default();
            env.set(MODEL_CAPABILITIES, capabilities);
            // Claude applies settings.env after inherited shell variables. A narrow
            // per-session override keeps this local gateway compatibility setting effective.
            args.extend(["--settings".into(), settings.to_string()]);
        }
    }
    if high {
        env.set("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", "1");
    }
    crate::cache_policy::configure(
        &exe,
        &req.cwd,
        &native_file(&sys),
        &r.cache_settings,
        &mut env,
        &cancel,
    )
    .await?;
    args.push("--model".into());
    args.push(s.model.clone());
    if let Some(effort) = &s.reasoning_effort {
        args.extend(["--effort".into(), effort.clone()]);
    }
    if let Some(sid) = &req.session_id {
        args.push("--resume".into());
        args.push(sid.clone());
    }
    emit.journal(
        "adapter.permission",
        json!({"profile": req.profile, "scope": boundary}),
    );
    let input = format!(
        "{}\n",
        json!({"type": "user", "message": {"role": "user", "content": crate::images::claude(&req)}})
    );
    let mut proc = start(&exe, &args, &req.cwd, &env)?;
    collect_turn(
        &mut proc,
        &input,
        req.session_id.as_deref(),
        (!sdk).then_some(s.model.as_str()),
        emit,
        &cancel,
    )
    .await
}

/// Consume the native process using the same terminal contract for real and fake CLIs.
async fn collect_turn(
    proc: &mut Proc,
    input: &str,
    expected_session: Option<&str>,
    expected_model: Option<&str>,
    emit: Emit,
    cancel: &CancellationToken,
) -> Result<RunOutcome, AdapterError> {
    let mut terminal: Option<Value> = None;
    let mut last_retry: Option<Value> = None;
    let mut assistant_error: Option<AdapterError> = None;
    let mut session: Option<String> = expected_session.map(str::to_owned);
    let mut open_tools: HashSet<String> = HashSet::new();
    let streamed = stream(proc, input, cancel, |e| {
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
            let model = e["model"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| e["model"].to_string());
            if expected_model.is_some_and(|expected| strip_1m(&model) != strip_1m(expected)) {
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
        if e["type"] == json!("assistant") {
            assistant_error = e["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|block| block["text"].as_str())
                .map(classify_claude_text)
                .find(|error| error.class != ErrorClass::Other);
        }
        if e["type"] == json!("result") {
            terminal = Some(e);
        }
        Ok(())
    })
    .await;
    let t = terminal.unwrap_or(Value::Null);
    if let Err(mut err) = streamed {
        // Only a generic process crash may be refined by a completed native error.
        // Cancellation, transport, protocol and callback failures keep their precedence.
        if err.class == ErrorClass::ProcessCrashed && terminal_reports_error(&t) {
            let native_error = terminal_error(&t, assistant_error, last_retry.as_ref());
            if native_error.class != ErrorClass::Other {
                err = native_error;
            }
        }
        err.session_id = err
            .session_id
            .or_else(|| t["session_id"].as_str().map(str::to_owned))
            .or(session);
        return Err(err);
    }
    let resumed_other =
        expected_session.is_some_and(|sid| t["session_id"].as_str().is_some_and(|got| got != sid));
    if resumed_other {
        return Err(session_mismatch("Claude 未返回匹配会话的成功终态"));
    }
    let ok = t["is_error"] != json!(true)
        && t["subtype"] == json!("success")
        && t["result"].is_string()
        && t["session_id"].as_str().is_some_and(|s| !s.is_empty());
    if !ok {
        let mut err = terminal_error(&t, assistant_error, last_retry.as_ref());
        err.message = "Claude 未返回匹配会话的成功终态".into();
        err.session_id = t["session_id"].as_str().map(str::to_string).or(session);
        return Err(err);
    }
    Ok(RunOutcome {
        text: t["result"].as_str().unwrap_or_default().to_string(),
        session_id: t["session_id"].as_str().map(str::to_string),
    })
}

fn terminal_reports_error(terminal: &Value) -> bool {
    terminal["is_error"] == true
        || terminal["subtype"]
            .as_str()
            .is_some_and(|kind| kind.starts_with("error"))
}

fn classify_claude_text(text: &str) -> AdapterError {
    // Claude's native API-error assistant frame carries an explicit HTTP status.
    // Inspect only this envelope and keep the provider body out of operator errors.
    if let Some(status) = text
        .trim_start()
        .strip_prefix("API Error: ")
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|word| word.parse::<u16>().ok())
        .filter(|status| (400..=599).contains(status))
    {
        return from_http_status(status, None);
    }
    classify_text(text)
}

fn terminal_error(
    terminal: &Value,
    assistant_error: Option<AdapterError>,
    last_retry: Option<&Value>,
) -> AdapterError {
    if let Some(text) = terminal["result"].as_str() {
        let error = classify_claude_text(text);
        if error.class != ErrorClass::Other {
            return error;
        }
    }
    // Assistant prose is evidence only when a terminal frame declares an error.
    if terminal_reports_error(terminal)
        && let Some(error) = assistant_error
    {
        return error;
    }
    if let Some(retry) = last_retry {
        return claude_api_retry(retry);
    }
    if terminal.is_null() {
        classified(ErrorClass::ProcessCrashed, "claude:no_terminal")
    } else {
        classified(ErrorClass::Other, "claude:error_result")
    }
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

#[cfg(test)]
#[path = "claude_discovery.rs"]
mod discovery_tests;

#[cfg(test)]
mod terminal_tests {
    use super::*;
    use sa_engine::resilience::classify::is_retryable;
    use sa_engine::resilience::policy::RetryLimits;

    async fn fake_cli(
        records: Vec<Value>,
        exit: i32,
        stderr: &str,
    ) -> Result<RunOutcome, AdapterError> {
        let sys = Sys::from_process();
        let node = sys
            .which(if cfg!(windows) { "node.exe" } else { "node" })
            .expect("native-stream fixture requires Node.js");
        let data = tempfile::tempdir().unwrap();
        let code = format!(
            r#"
            require('node:fs').readFileSync(0);
            for (const record of {}) process.stdout.write(JSON.stringify(record) + "\n");
            process.stderr.write({});
            setTimeout(() => {{ process.exitCode = {}; }}, 30);
        "#,
            serde_json::to_string(&records).unwrap(),
            serde_json::to_string(stderr).unwrap(),
            exit
        );
        let mut proc = start(&node, &["-e".into(), code], data.path(), &sys).unwrap();
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        collect_turn(
            &mut proc,
            "{}\n",
            None,
            None,
            Emit(events),
            &CancellationToken::new(),
        )
        .await
    }

    fn assistant_error() -> Value {
        json!({"type":"assistant","message":{"content":[{"type":"text","text":"API Error: 400 Per-turn system messages require upstream per-turn-control support private-provider-body"}]}})
    }

    fn result(is_error: bool) -> Value {
        json!({"type":"result","subtype":"success","is_error":is_error,"session_id":"fixture-session","result":"","usage":{"input_tokens":0,"output_tokens":0}})
    }

    #[tokio::test]
    async fn native_400_with_nonzero_exit_is_permanent_and_redacted() {
        let error = fake_cli(vec![assistant_error(), result(true)], 1, "")
            .await
            .unwrap_err();
        assert_eq!(error.class, ErrorClass::Protocol);
        assert_eq!(error.http_status, Some(400));
        assert_eq!(error.code.as_deref(), Some("http:400"));
        assert_eq!(error.session_id.as_deref(), Some("fixture-session"));
        assert!(!is_retryable(error.class));
        assert_eq!(RetryLimits::default().for_error(&error), 0);
        assert!(!error.message.contains("private-provider-body"));
    }

    #[tokio::test]
    async fn error_prose_only_refines_a_terminal_declared_failure() {
        let unknown = json!({"type":"result","subtype":"success","is_error":true,"session_id":"fixture-session","result":"unknown native failure"});
        let error = fake_cli(vec![unknown], 1, "").await.unwrap_err();
        assert_eq!(error.class, ErrorClass::ProcessCrashed);
        let error = fake_cli(vec![assistant_error()], 1, "").await.unwrap_err();
        assert_eq!(error.class, ErrorClass::ProcessCrashed);
        let error = fake_cli(vec![assistant_error(), result(false)], 1, "")
            .await
            .unwrap_err();
        assert_eq!(error.class, ErrorClass::ProcessCrashed);
        let outcome = fake_cli(vec![assistant_error(), result(false)], 0, "")
            .await
            .unwrap();
        assert_eq!(outcome.text, "");
        let error = fake_cli(vec![assistant_error(), result(true)], 0, "")
            .await
            .unwrap_err();
        assert_eq!(error.class, ErrorClass::Protocol);
    }

    #[tokio::test]
    async fn transport_failure_keeps_precedence_over_native_error_text() {
        let error = fake_cli(
            vec![assistant_error(), result(true)],
            1,
            "fixture-private ECONNRESET\n",
        )
        .await
        .unwrap_err();
        assert_eq!(error.class, ErrorClass::Network);
        assert!(!error.message.contains("fixture-private"));
    }
    #[test]
    fn gateway_capabilities_follow_file_precedence_without_copying_other_env() {
        let data = tempfile::tempdir().unwrap();
        let home = data.path().join("home");
        let cwd = data.path().join("project/nested");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(cwd.join(".claude")).unwrap();
        let env = Sys::new(
            vec![(MODEL_CAPABILITIES.into(), "shell=effort".into())],
            home.clone(),
            data.path().into(),
        );
        std::fs::write(home.join(".claude/settings.json"), json!({"env":{MODEL_CAPABILITIES:"user=effort","ANTHROPIC_AUTH_TOKEN":"fixture-private"}}).to_string()).unwrap();
        let project = cwd.parent().unwrap();
        std::fs::create_dir_all(project.join(".claude")).unwrap();
        std::fs::write(project.join(".claude/settings.json"), json!({"env":{MODEL_CAPABILITIES:"unrelated=effort;claude-fable-*=effort,mid_conv_system;claude-fable-5-2=-effort;claude-fable-*=effort"}}).to_string()).unwrap();
        let settings = local_gateway_settings(&env, &cwd, true, "claude-fable-5-2").unwrap();
        assert_eq!(
            settings["env"][MODEL_CAPABILITIES],
            "unrelated=effort;claude-fable-*=effort,mid_conv_system;claude-fable-5-2=-effort;claude-fable-*=effort;claude-fable-5-2=-mid_conv_system"
        );
        assert_eq!(settings["env"].as_object().unwrap().len(), 1);
        assert!(!settings.to_string().contains("fixture-private"));
        std::fs::write(
            cwd.join(".claude/settings.local.json"),
            json!({"env":{MODEL_CAPABILITIES:"local=effort;"}}).to_string(),
        )
        .unwrap();
        assert_eq!(
            local_gateway_settings(&env, &cwd, true, "claude-fable-5-2").unwrap()["env"]
                [MODEL_CAPABILITIES],
            "local=effort;claude-fable-5-2=-mid_conv_system"
        );
    }

    #[test]
    fn gateway_capabilities_fail_closed_on_bad_settings_but_restricted_ignores_files() {
        let data = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(data.path().join(".claude")).unwrap();
        std::fs::write(
            data.path().join(".claude/settings.json"),
            b"invalid-json-private",
        )
        .unwrap();
        let env = Sys::new(
            vec![(
                MODEL_CAPABILITIES.into(),
                "shell=effort;claude-fable-5-2=effort".into(),
            )],
            data.path().into(),
            data.path().into(),
        );
        let error =
            local_gateway_settings(&env, data.path(), true, "claude-fable-5-2").unwrap_err();
        assert!(!error.message.contains("invalid-json-private"));
        assert_eq!(
            local_gateway_settings(&env, data.path(), false, "claude-fable-5-2").unwrap()["env"]
                [MODEL_CAPABILITIES],
            "shell=effort;claude-fable-5-2=effort;claude-fable-5-2=-mid_conv_system"
        );
        std::fs::write(
            data.path().join(".claude/settings.json"),
            json!({"env":{MODEL_CAPABILITIES:42}}).to_string(),
        )
        .unwrap();
        assert!(local_gateway_settings(&env, data.path(), true, "claude-fable-5-2").is_err());
    }
}
