//! Claude Code adapter (port of `runtime/adapters/claude.cjs`, spec §7.3).

use crate::common::{
    Emit, cc_rows, endpoint, fingerprint, local_host, modes, probe_failed, read_json, read_json_or,
    route_gone, scope, select, session_mismatch,
};
use crate::custom::{self, CustomProvider, Registry};
use crate::probe_cache::{
    CACHED_FAILURE, DiscoveryTiming, ModelProbeResult, ProbeCache, ProbeKey, ProbeUse, Probes, Stamp,
    TurnTiming, config_digest, custom_probe_slot, file_stamp, probe_slot, refresh_custom,
    safe_models, supplement_efforts,
};
use crate::proc::{Proc, cancelled, capture, start, stream};
use crate::sys::Sys;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared, join_all};
use regex::Regex;
use sa_engine::adapter::{
    Adapter, AdapterError, Catalog, ErrorClass, EventSink, Route, RunOutcome, RunRequest, TurnHost,
};
use sa_engine::resilience::classify::{
    MAX_QUOTA_RESET, classified, classify_text, claude_api_retry, from_http_status, quota,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

static DEFAULT_MODEL_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ANTHROPIC_DEFAULT_\w+_MODEL$").expect("valid regex"));

pub struct Claude {
    sys: Sys,
    registry: Registry,
    efficiency: sa_store::efficiency::Shared,
    probes: ProbeCache,
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
        let probes = ProbeCache::new(sys.data_dir.as_deref());
        Self { sys, registry, efficiency, probes }
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

fn probe_key(sys: &Sys, exe: Option<&Path>, customs: &[CustomProvider]) -> ProbeKey {
    let native = native_file(sys);
    let cc = sys.home.join(".cc-switch");
    let config = [
        native.clone(),
        cc.join("settings.json"),
        cc.join("live-state.json"),
        sys.platform_dir().join("claude-route.json"),
    ].map(|path| config_digest(&path));
    let mut env = sys.clone();
    // This preference is consumed by the turn launcher after model discovery.
    env.remove("SUPERAGENT_CLAUDE_CACHE_TTL");
    ProbeKey::new("claude", &env, exe, json!([
        crate::revision::fingerprint(&crate::revision::watch_paths(sys, "claude")),
        config, customs,
        file_stamp(&native.with_file_name(".credentials.json")),
    ]))
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
    catalog_fallback: bool,
    preferred: String,
    reason: String,
    cache_settings: Value,
    probe_slot: Option<String>,
    routed_slots: HashMap<String, String>,
    inferred_efforts: HashSet<String>,
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

const OFFICIAL_SLOT: &str = "native:official";

fn official_record() -> Record {
    Record {
        app: "native".into(),
        row: "first-party".into(),
        name: "官方账号".into(),
        auth: "sdk-login".into(),
        probe_slot: Some(OFFICIAL_SLOT.into()),
        reason: "未验证官方登录或模型目录".into(),
        ..Default::default()
    }
}

/// `firstParty(exe)` (CL:52-101).
async fn first_party(sys: &Sys, exe: Option<&PathBuf>, cancel: &CancellationToken) -> Record {
    let mut rec = official_record();
    let Some(exe) = exe else { return rec };
    let env = official_env(sys);
    let flags = ["--safe-mode", "--setting-sources", ""].map(String::from);
    let mut status_args = flags.to_vec();
    status_args.extend(["auth", "status"].map(String::from));
    rec.catalog_error = Some("官方登录状态探测失败".into());
    let Ok((out, status_code)) = crate::proc::capture_status::<true>(
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
        rec.catalog_error = None;
        return rec;
    }
    if status_code != 0 { return rec; }
    rec.catalog_error = Some("官方模型目录初始化失败".into());
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
            if r["request_id"] == json!("catalog") && r["subtype"] == json!("success")
                && r["response"]["models"].is_array() {
                rec.catalog_error = None;
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

#[derive(Serialize, Deserialize)]
struct OfficialModels {
    available: bool,
    models: Vec<String>,
    model_info: Vec<Value>,
}

async fn discover_official(
    sys: &Sys, exe: Option<&PathBuf>, cancel: &CancellationToken, probes: Option<&Probes<'_>>,
    optional: bool,
) -> Record {
    let Some(probes) = probes.filter(|_| exe.is_some()) else {
        return first_party(sys, exe, cancel).await;
    };
    if let Some(models) = probes.read::<OfficialModels>(OFFICIAL_SLOT) {
        let mut record = official_record();
        if models.available {
            record.reason.clear();
        }
        record.models = models.models;
        record.model_info = models.model_info;
        return record;
    }
    if probes.failure_is_fresh(OFFICIAL_SLOT, optional) {
        let mut record = official_record();
        record.catalog_error = Some(CACHED_FAILURE.into());
        return record;
    }
    let started = probes.start();
    let record = first_party(sys, exe, cancel).await;
    if !cancel.is_cancelled() {
        let models = record.catalog_error.is_none().then(|| OfficialModels {
            available: record.reason.is_empty(),
            models: record.models.clone(),
            model_info: safe_models(&json!(record.model_info)),
        });
        probes.record(OFFICIAL_SLOT, started, models.as_ref());
    }
    record
}

type ModelProbe = Shared<BoxFuture<'static, (Stamp, Result<ModelProbeResult, String>)>>;
type ModelCache = AsyncMutex<HashMap<(String, String, String), ModelProbe>>;

struct DiscoveryProbes<'a> {
    http: ModelCache,
    reusable: Option<&'a Probes<'a>>,
    selected: Option<&'a Route>,
    routed_member: Option<&'a str>,
}

impl DiscoveryProbes<'_> {
    async fn models(
        &self, url: String, auth: &str, token: String, slot: &str, optional: bool,
        cancel: &CancellationToken,
    ) -> Result<ModelProbeResult, String> {
        if let Some(models) = self.reusable.and_then(|cache| cache.read(slot)) {
            return Ok((models, None));
        }
        if self.reusable.is_some_and(|cache| cache.failure_is_fresh(slot, optional)) {
            return Err(CACHED_FAILURE.into());
        }
        let auth = auth.to_string();
        let key = (url.clone(), auth.clone(), token.clone());
        let probe = {
            let mut pending = self.http.lock().await;
            pending.entry(key).or_insert_with(|| {
                let cancel = cancel.clone();
                let started = self.reusable.map_or_else(Stamp::now, Probes::start);
                async move {
                    let mut headers = if auth == "ANTHROPIC_API_KEY" {
                        vec![("x-api-key".to_string(), token)]
                    } else {
                        vec![("Authorization".to_string(), format!("Bearer {token}"))]
                    };
                    headers.push(("anthropic-version".into(), "2023-06-01".into()));
                    let result = crate::catalog::fetch_with_status(&url, &headers, &cancel).await
                        .map_err(|error| error.to_string());
                    (started, result)
                }.boxed().shared()
            }).clone()
        };
        let (started, result) = probe.await;
        if let Some(probes) = self.reusable && !cancel.is_cancelled() {
            let models = result.as_ref().ok()
                .filter(|(_, error)| error.is_none())
                .map(|(models, _)| safe_models(&json!(models)));
            probes.record(slot, started, models.as_ref());
        }
        result
    }
}

fn configured_record(app: &str, id: &str, name: &str, cfg: &Value) -> Record {
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
    Record {
        app: app.into(),
        row: id.into(),
        name: name.into(),
        base,
        auth: auth.into(),
        token: env[auth].as_str().map(str::to_string),
        preferred,
        cache_settings: crate::cache_policy::selected_settings(cfg),
        ..Default::default()
    }
}

/// `makeRecord(app, id, name, cfg, meta, cache)` (CL:102-160).
async fn make_record(
    mut rec: Record,
    cfg: &Value,
    meta: &Value,
    cache: Option<&DiscoveryProbes<'_>>,
    cancel: &CancellationToken,
) -> Record {
    let env = &cfg["env"];
    let attempt: anyhow::Result<()> = async {
        endpoint(&rec.base)?;
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
        if let Some(cache) = cache {
            let url = format!("{}/v1/models?limit=1000", api_base(&rec.base));
            let provider = rec.id();
            let slot = probe_slot("provider", &json!(provider));
            let optional = cache.selected.is_some_and(|route| {
                route.provider != provider
                    && !(rec.app == "claude" && cache.routed_member == Some(rec.row.as_str()))
            });
            let result = cache.models(url, &rec.auth, token, &slot, optional, cancel).await;
            rec.probe_slot = Some(slot);
            match result {
                Ok((discovered, fallback)) => {
                    rec.catalog_fallback = fallback.is_some();
                    rec.catalog_error = fallback;
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
    catalog: Value,
}

impl Discovery {
    fn primary_slot(&self, provider: &str) -> Option<&str> {
        self.records
            .iter()
            .find(|record| record.id() == provider)?
            .probe_slot
            .as_deref()
    }

    fn route_slots(&self, route: &Route) -> Vec<&str> {
        let Some(record) = self.records.iter().find(|record| record.id() == route.provider)
        else { return Vec::new() };
        let mut slots: Vec<_> = record.probe_slot.as_deref().into_iter().collect();
        if let Some(slot) = record.routed_slots.get(&route.model) {
            slots.push(slot);
        }
        if route.reasoning_effort.is_some() && record.inferred_efforts.contains(&route.model)
            && !slots.contains(&OFFICIAL_SLOT)
        {
            slots.push(OFFICIAL_SLOT);
        }
        slots
    }
}

/// CC Switch model id that its aggregation ("Stack") mode routes to one member.
const STACK_PREFIX: &str = "ccs-claude-";

/// CC Switch routing or aggregation mode for Claude Code. Requests go to its
/// local listener with a placeholder key; unprefixed models reach the default
/// member and `ccs-claude-<key>--<model>` reaches the member that owns `key`,
/// even when that model is not in the listener's published catalog.
/// [CC Switch `mode/stack.rs`](https://github.com/farion1231/cc-switch/blob/main/src-tauri/src/mode/stack.rs)
struct Gateway {
    base: String,
    route: String,
    /// `(key, provider row)` in membership order; empty in routing-only mode.
    members: Vec<(String, String)>,
}

fn gateway(sys: &Sys, state: &Value) -> Option<Gateway> {
    if state["mode"].as_str() != Some("proxy") {
        return None;
    }
    let base = crate::common::cc_proxy(sys, "claude").ok().flatten()?;
    let route = state["proxy_route"].as_str().unwrap_or_default().to_string();
    let stack = &state["stack"];
    let mut members = vec![];
    if stack["enabled"] == json!(true) {
        let listed: Vec<&str> = stack["members"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        for (key, row) in stack["keys"].as_object().into_iter().flatten() {
            if let Some(row) = row.as_str().filter(|r| listed.contains(r))
                && !key.is_empty()
                && !key.contains("--")
            {
                members.push((key.clone(), row.to_string()));
            }
        }
        members.sort_by_key(|(_, row)| listed.iter().position(|m| m == row));
    }
    Some(Gateway {
        base,
        route,
        members,
    })
}

fn stack_id(key: &str, model: &str) -> String {
    match model
        .strip_suffix("[1M]")
        .or_else(|| model.strip_suffix("[1m]"))
    {
        Some(bare) => format!("{STACK_PREFIX}{key}--{bare}[1M]"),
        None => format!("{STACK_PREFIX}{key}--{model}"),
    }
}

fn add_routed(rec: &mut Record, id: String, source: &Record, model: &str, tag: &str) {
    // `x` and `x[1M]` reach the same upstream model through CC Switch.
    let bare = |m: &str| m.strip_suffix("[1M]").or_else(|| m.strip_suffix("[1m]")).unwrap_or(m).to_string();
    if rec.models.iter().any(|m| bare(m) == bare(&id)) || id.encode_utf16().count() >= 200 {
        return;
    }
    let mut info = source
        .model_info
        .iter()
        .find(|m| m["id"] == model)
        .cloned()
        .unwrap_or_else(|| json!({"id": model, "label": model}));
    let label = info["label"].as_str().unwrap_or(model).to_string();
    info["id"] = json!(id);
    info["label"] = json!(format!("{label}（{tag}）"));
    if let Some(slot) = &source.probe_slot {
        rec.routed_slots.insert(id.clone(), slot.clone());
    }
    rec.models.push(id);
    rec.model_info.push(info);
}

/// Adds every model the gateway can route: the default member's models as
/// plain ids, then each aggregation member's models with its reserved prefix.
fn aggregate(rec: &mut Record, gateway: &Gateway, records: &[Record]) {
    let member = |row: &str| {
        records
            .iter()
            .find(|r| r.app == "claude" && r.row == row && r.reason.is_empty())
    };
    rec.name = if gateway.members.is_empty() {
        "CC Switch 路由".into()
    } else {
        "CC Switch 聚合".into()
    };
    if let Some(route) = member(&gateway.route) {
        let tag = format!("默认路由 · {}", route.name);
        for model in route.models.iter().filter(|m| !m.starts_with(STACK_PREFIX)) {
            add_routed(rec, model.clone(), route, model, &tag);
        }
        let default = route.route()["model"].as_str().unwrap_or_default().to_string();
        if !rec.models.contains(&rec.preferred) && rec.models.contains(&default) {
            rec.preferred = default;
        }
    }
    // The default member's models are already listed without a prefix.
    for (key, row) in gateway.members.iter().filter(|(_, row)| *row != gateway.route) {
        if let Some(source) = member(row) {
            for model in &source.models {
                add_routed(rec, stack_id(key, model), source, model, &source.name);
            }
        }
    }
}

/// `discover()` (CL:167-266).
#[cfg(test)]
async fn discover(
    sys: &Sys,
    exe: Option<PathBuf>,
    cancel: &CancellationToken,
) -> anyhow::Result<Discovery> {
    discover_with(sys, exe, cancel, true, None, None).await
}

async fn discover_with(
    sys: &Sys, exe: Option<PathBuf>, cancel: &CancellationToken, online: bool,
    reusable: Option<&Probes<'_>>, selected: Option<&Route>,
) -> anyhow::Result<Discovery> {
    let dir = sys.home.join(".cc-switch");
    let device = read_json_or(&dir.join("settings.json"));
    let state = read_json_or(&dir.join("live-state.json"))["apps"]["claude-desktop"].clone();
    let bound = read_json_or(&sys.platform_dir().join("claude-route.json"));
    let sources = cc_rows(sys, &["claude", "claude-desktop"])?
        .into_iter()
        .map(|row| {
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
            let Some((cfg, meta)) = cfg.zip(meta) else {
                let record = Record {
                    app: app.into(),
                    row: id,
                    name: name.into(),
                    reason: "无效的来源记录".into(),
                    ..Default::default()
                };
                return (record, None, false);
            };
            let mut record = configured_record(app, &id, name, &cfg);
            let uses_official = row["category"] == "official" && record.base.is_empty();
            if uses_official {
                record.auth = "sdk-login".into();
            }
            (record, Some((cfg, meta)), uses_official)
        })
        .collect::<Vec<_>>();
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
    let native_record = configured_record("native", &native_path, "当前 Claude Code 配置", &cfg);
    let claude_state = read_json_or(&dir.join("live-state.json"))["apps"]["claude"].clone();
    let gateway = gateway(sys, &claude_state);
    let gateway_source = gateway
        .as_ref()
        .filter(|gateway| {
            native_record.base.is_empty() || api_base(&native_record.base) != api_base(&gateway.base)
        })
        .map(|gateway| {
            let config = json!({"env": {
                "ANTHROPIC_BASE_URL": gateway.base,
                "ANTHROPIC_AUTH_TOKEN": "PROXY_MANAGED",
            }});
            let record = configured_record("cc-switch", "claude-proxy", "CC Switch 路由", &config);
            (record, config)
        });
    let routed_member = gateway.as_ref().and_then(|gateway| {
        let provider = gateway_source
            .as_ref()
            .map_or(&native_record, |(record, _)| record)
            .id();
        let route = selected.filter(|route| route.provider == provider)?;
        if let Some(model) = route.model.strip_prefix(STACK_PREFIX) {
            let (key, _) = model.split_once("--")?;
            gateway.members.iter().find(|(k, _)| k == key).map(|(_, row)| row.as_str())
        } else {
            Some(gateway.route.as_str())
        }
    });
    let optional_official = selected.is_some_and(|route| {
        route.provider != official_record().id()
            && !sources.iter().any(|(alias, _, uses_official)| {
                *uses_official
                    && (route.provider == alias.id()
                        || (alias.app == "claude" && routed_member == Some(alias.row.as_str())))
            })
    });
    let official_probe = discover_official(
        sys, exe.as_ref().filter(|_| online), cancel, reusable, optional_official,
    ).shared();
    let cache = DiscoveryProbes {
        http: AsyncMutex::default(), reusable, selected, routed_member,
    };
    let records = join_all(sources.into_iter().map(|(record, config, uses_official)| {
        let (cache, official) = (online.then_some(&cache), official_probe.clone());
        async move {
            let Some((cfg, meta)) = config else {
                return record;
            };
            let mut record = make_record(record, &cfg, &meta, cache, cancel).await;
            if uses_official {
                let official = official.await;
                record.models = official.models;
                record.model_info = official.model_info;
                record.reason = official.reason;
                record.catalog_error = official.catalog_error;
                record.probe_slot = official.probe_slot;
            }
            record
        }
    }));
    let meta = json!({});
    let native = make_record(native_record, &cfg, &meta, online.then_some(&cache), cancel);
    let (official, mut records, mut native) =
        tokio::join!(official_probe.clone(), records, native,);
    drop(official_probe);
    if let Some(gateway) = &gateway {
        // Routing-only mode may publish no models; a reachable listener with an
        // empty catalog still routes every member model aggregated below.
        let enrich = |rec: &mut Record| {
            if (rec.catalog_error.is_none() || rec.catalog_fallback)
                && rec.token.as_deref().is_some_and(|t| !t.is_empty())
            {
                aggregate(rec, gateway, &records);
                if !rec.models.is_empty() {
                    rec.reason.clear();
                }
            }
        };
        if let Some((record, cfg)) = gateway_source {
            let mut rec = make_record(record, &cfg, &meta, online.then_some(&cache), cancel).await;
            enrich(&mut rec);
            records.push(rec);
        } else {
            enrich(&mut native);
        }
    }
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
        record.inferred_efforts = supplement_efforts(&mut rows, &official.model_info);
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
                "detail": if let Some(error) = &r.catalog_error {
                    let fallback = if r.catalog_fallback { "已保留上次目录" } else { "以下仅为已保存型号" };
                    format!("在线目录获取失败：{error}；{fallback}")
                } else if r.reason.is_empty() { "同源认证的在线模型目录".to_string() } else { r.reason.clone() },
            })
        })
        .collect();
    let default = match active_idx.map(|i| &records[i]) {
        Some(a) if a.reason.is_empty() || a.catalog_error.is_some() => a.route(),
        _ => json!({"provider": "", "model": "", "mode": "cli"}),
    };
    let catalog = json!({
        "providers": providers,
        "default": default,
        "legacy_defaults": legacy,
        "modes": modes("Claude Code CLI", exe.is_some()),
        "detail": "CC Switch 已选直连来源、路由/聚合模式及 Claude 原生配置",
        "downloads": [
            {"label": "Claude Code 安装", "url": "https://code.claude.com/docs/en/quickstart"},
            {"label": "Claude 桌面版", "url": "https://claude.com/download"}
        ]
    });
    Ok(Discovery {
        exe,
        records,
        catalog,
    })
}

impl Adapter for Claude {
    fn agent(&self) -> &'static str {
        "claude"
    }

    fn catalog_revision(&self) -> String {
        crate::catalog_revision_with(&self.sys, self.agent(), &self.registry)
    }
    fn cli_info(&self) -> BoxFuture<'static, Value> {
        crate::compatibility::detect(self.agent(), self.sys.clone(), executable(&self.sys))
    }

    fn local_catalog(&self) -> Value {
        // Local mode reads saved source identities and defaults only.
        discover_with(&self.sys, executable(&self.sys), &CancellationToken::new(), false, None, None)
            .now_or_never().and_then(Result::ok).map(|d| d.catalog).unwrap_or_default()
    }

    fn catalog(&self) -> BoxFuture<'static, anyhow::Result<Catalog>> {
        let sys = self.sys.clone();
        let customs = custom::snapshot(&self.registry, "claude");
        let cache = self.probes.clone();
        Box::pin(async move {
            let exe = executable(&sys);
            let key = probe_key(&sys, exe.as_deref(), &customs);
            let probes = Probes::new(&cache, &key, false);
            let cancel = CancellationToken::new();
            let mut value = discover_with(&sys, exe, &cancel, true, Some(&probes), None)
                .await?
                .catalog;
            let _ = refresh_custom(&mut value, &customs, &sys, &cancel, &probes, None).await;
            probes.flush().await;
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
        let cache = self.probes.clone();
        Box::pin(async move { run(sys, customs, request, host, Emit(events), cancel, false, cache).await })
    }

    /// A forked, unpersisted one-line turn on the same session. Claude Code
    /// sends the session's tools, system prompt and history unchanged, so the
    /// request reads (and refreshes) the cache the next real turn will read.
    fn keepalive(
        &self,
        request: RunRequest,
        host: Arc<dyn TurnHost>,
        events: EventSink,
        cancel: CancellationToken,
    ) -> Option<BoxFuture<'static, Result<RunOutcome, AdapterError>>> {
        request.session_id.as_ref()?;
        let mut sys = self.sys.clone();
        let ttl = self
            .efficiency
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .claude_cache_ttl;
        crate::cache_policy::select_fallback(&mut sys, ttl);
        let customs = custom::snapshot(&self.registry, "claude");
        let cache = self.probes.clone();
        Some(Box::pin(async move { run(sys, customs, request, host, Emit(events), cancel, true, cache).await }))
    }
}

fn strip_1m(m: &str) -> &str {
    m.strip_suffix("[1M]").unwrap_or(m)
}

const MODEL_CAPABILITIES: &str = "CLAUDE_CODE_MODEL_CAPABILITIES";
const LOCAL_GATEWAY_CAPABILITIES: &str = "*=-mid_conv_system,-per_turn_effort";

fn local_gateway_settings(
    env: &Sys,
    cwd: &Path,
    high: bool,
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
    // The CLI merges matching entries per capability in order, including
    // duplicate exact selectors and wildcards, so appending one override keeps
    // every other effective token. CLI 2.1.323 sends in-conversation system
    // messages unless both capabilities are off, and matches selectors against
    // a normalized id (CC Switch `ccs-claude-…` ids included). The wildcard also
    // covers helper models that this process sends to the same gateway.
    let entry = LOCAL_GATEWAY_CAPABILITIES.to_string();
    let capabilities = capabilities.trim_end_matches(';');
    let merged = if capabilities.is_empty() {
        entry
    } else {
        format!("{capabilities};{entry}")
    };
    Ok(json!({"env":{MODEL_CAPABILITIES:merged}}))
}

/// `run(o)` (CL:267-356).
#[allow(clippy::too_many_arguments)]
async fn run(
    sys: Sys,
    customs: Vec<CustomProvider>,
    req: RunRequest,
    host: Arc<dyn TurnHost>,
    emit: Emit,
    cancel: CancellationToken,
    fork: bool,
    cache: ProbeCache,
) -> Result<RunOutcome, AdapterError> {
    crate::images::check(&req)?;
    let boundary = scope("claude", &req, &host, &cancel, false).await?;
    let started = Instant::now();
    let exe = executable(&sys);
    let key = probe_key(&sys, exe.as_deref(), &customs);
    let mut reuse = true;
    let mut retry_slots = HashSet::new();
    let mut positive_refresh = HashSet::new();
    let (d, s, timing, used) = loop {
        let forced_slots = if positive_refresh.is_empty() {
            &retry_slots
        } else {
            &positive_refresh
        };
        let probes = Probes::new(&cache, &key, reuse).retrying(forced_slots);
        let mut d = discover_with(&sys, exe.clone(), &cancel, true, Some(&probes), Some(&req.route))
            .await
            .map_err(|e| if cancel.is_cancelled() { cancelled() } else { probe_failed(&e.to_string()) })?;
        let donor = refresh_custom(&mut d.catalog, &customs, &sys, &cancel, &probes, Some(&req.route)).await;
        probes.flush().await;
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let mut used = if custom::lookup(&d.catalog, &customs, &req.route.provider).is_some() {
            probes.observed(&[&custom_probe_slot(&req.route.provider)])
        } else {
            probes.observed(&d.route_slots(&req.route))
        };
        if let Some(donor) = &donor {
            used.extend(probes.observed(&d.route_slots(donor)));
        }
        match select(&d.catalog, &req.route) {
            Ok(s) => {
                let timing = DiscoveryTiming::new(
                    started,
                    ProbeUse::all_cached(&used),
                    !reuse || !retry_slots.is_empty(),
                );
                break (d, s, timing, used);
            }
            Err(error) => {
                if reuse && positive_refresh.is_empty() {
                    if retry_slots.is_empty() {
                        retry_slots = probes.deferred_effort_slots(
                            &d.catalog,
                            &req.route,
                            donor.as_ref(),
                            OFFICIAL_SLOT,
                            |provider| d.primary_slot(provider),
                        );
                        if !retry_slots.is_empty() {
                            continue;
                        }
                    }
                    positive_refresh = ProbeUse::cached_slots_except(&used, &retry_slots);
                    if !positive_refresh.is_empty() {
                        // A second retry refreshes positive route inputs while
                        // keeping already retried sources warm.
                        reuse = !retry_slots.is_empty();
                        continue;
                    }
                }
                cache.evict_used(&key, &used).await;
                return Err(error);
            }
        }
    };
    let result = run_selected(sys, customs, req, host, emit, cancel, fork, d, s, boundary, timing).await;
    if result.as_ref().is_err_and(|error| error.class != ErrorClass::Cancelled) {
        cache.evict_used(&key, &used).await;
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn run_selected(
    sys: Sys,
    customs: Vec<CustomProvider>,
    req: RunRequest,
    host: Arc<dyn TurnHost>,
    emit: Emit,
    cancel: CancellationToken,
    fork: bool,
    d: Discovery,
    s: sa_engine::adapter::Route,
    boundary: &'static str,
    discovery: DiscoveryTiming,
) -> Result<RunOutcome, AdapterError> {
    let high = boundary == "danger-full-access";
    let chosen = custom::lookup(&d.catalog, &customs, &s.provider).cloned();
    let r = match &chosen {
        Some(_) => Record::default(),
        None => d
            .records
            .iter()
            .find(|r| r.id() == s.provider)
            .cloned()
            .ok_or_else(|| probe_failed("已选供应商在本次发现中缺失，请刷新配置"))?,
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
        env.set("ANTHROPIC_BASE_URL", api_base(&r.base));
        env.set(&r.auth, r.token.clone().unwrap_or_default());
    }
    // Local gateways (Ferryman, CC Switch routing/aggregation listeners and local
    // custom providers) may forward to backends that reject per-turn system
    // messages with HTTP 400, including after the CC Switch listener picks a
    // member. Disabling that capability for the selected model is lossless.
    let gateway_base = match &chosen {
        Some(p) => p.base_url.clone(),
        None if sdk => String::new(),
        None => r.base.clone(),
    };
    if endpoint(&gateway_base).is_ok_and(|u| local_host(u.host_str().unwrap_or_default())) {
        let settings = local_gateway_settings(&env, &req.cwd, high)?;
        let capabilities = settings["env"][MODEL_CAPABILITIES]
            .as_str()
            .unwrap_or_default();
        env.set(MODEL_CAPABILITIES, capabilities);
        // Claude applies settings.env after inherited shell variables. A narrow
        // per-session override keeps this local gateway compatibility setting effective.
        args.extend(["--settings".into(), settings.to_string()]);
    }
    if high {
        env.set("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", "1");
        // Prototype scope: `--safe-mode` (restricted turns) disables all MCP.
        if let Some(bus) = &req.bus {
            let server = crate::common::BUS_SERVER;
            let token = crate::common::BUS_TOKEN_ENV;
            let config = json!({"mcpServers": {server: {
                "type": "http",
                "url": bus.url,
                // Claude Code defers MCP tools behind tool search by default;
                // collaboration only works if the model sees these tools.
                "alwaysLoad": true,
                "headers": {"Authorization": format!("Bearer ${{{token}}}")},
            }}});
            args.extend(["--mcp-config".into(), config.to_string()]);
            env.set(token, bus.token.clone());
            // `advise` waits for a whole advisor turn; keep a longer operator value.
            let wanted = crate::common::BUS_TOOL_TIMEOUT_SECS * 1000;
            if env.get("MCP_TOOL_TIMEOUT").and_then(|v| v.parse::<u64>().ok()).is_none_or(|v| v < wanted) {
                env.set("MCP_TOOL_TIMEOUT", wanted.to_string());
            }
        }
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
        if fork {
            // Keep-alive: same prefix, but neither the session nor its
            // history changes.
            args.extend(["--fork-session".into(), "--no-session-persistence".into()]);
        }
    }
    emit.journal(
        "adapter.permission",
        json!({"profile": req.profile, "scope": boundary}),
    );
    let input = format!(
        "{}\n",
        json!({"type": "user", "message": {"role": "user", "content": crate::images::claude(&req)}})
    );
    let spawning = Instant::now();
    let mut proc = start(&exe, &args, &req.cwd, &env)?;
    let timing = discovery.spawned(spawning);
    collect_turn(
        &mut proc,
        &input,
        // A fork answers under its own new session id.
        if fork { None } else { req.session_id.as_deref() },
        (!sdk).then_some(s.model.as_str()),
        emit,
        &cancel,
        (!fork).then_some(host),
        Some(timing),
    )
    .await
}

/// Longest note written into a live Claude turn (same bound as Codex steering).
const MAX_STEER_BYTES: usize = 64 * 1024;

/// Consume the native process using the same terminal contract for real and fake CLIs.
#[allow(clippy::too_many_arguments)]
async fn collect_turn(
    proc: &mut Proc,
    input: &str,
    expected_session: Option<&str>,
    expected_model: Option<&str>,
    emit: Emit,
    cancel: &CancellationToken,
    steer: Option<Arc<dyn TurnHost>>,
    timing: Option<TurnTiming>,
) -> Result<RunOutcome, AdapterError> {
    let mut terminal: Option<Value> = None;
    let mut last_retry: Option<Value> = None;
    let mut assistant_error: Option<AdapterError> = None;
    let mut last_rate_limit: Option<Value> = None;
    let mut usage_limit_reached = false;
    let mut session: Option<String> = expected_session.map(str::to_owned);
    // Shared with the steering tick: a note written while a tool runs joins
    // the same turn (Claude Code adds it to the request after the tool
    // result); a note written between turns would start another turn.
    let open_tools: std::sync::Mutex<HashSet<String>> = std::sync::Mutex::default();
    let mut on_event = |e: Value| {
        emit.journal("native", e.clone());
        if e["rate_limit_info"].is_object() {
            last_rate_limit = Some(e["rate_limit_info"].clone());
        }
        if matches!(e["type"].as_str(), Some("assistant" | "result"))
            && e["api_error"] == "usage_limit_reached"
        {
            usage_limit_reached = true;
        }
        if e["type"] == json!("stream_event") {
            if e["event"]["delta"]["type"] == json!("text_delta") {
                emit.journal("delta", e["event"]["delta"]["text"].clone());
            }
            if e["event"]["type"] == json!("content_block_stop") {
                emit.journal("message.boundary", Value::Null);
            }
        }
        track_tools(&e, &emit, &mut open_tools.lock().unwrap_or_else(|e| e.into_inner()));
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
            return Ok(crate::proc::Feed::Close);
        }
        Ok(crate::proc::Feed::Continue)
    };
    if let Some(timing) = timing {
        timing.emit(&emit);
    }
    let streamed = match &steer {
        None => stream(proc, input, cancel, |e| on_event(e).map(|_| ())).await,
        Some(host) => {
            let mut steered: HashSet<i64> = HashSet::new();
            let on_tick = || {
                if open_tools.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
                    return None;
                }
                let note = host
                    .pending_notes()
                    .into_iter()
                    .find(|n| n["seq"].as_i64().is_some_and(|seq| !steered.contains(&seq)))?;
                let seq = note["seq"].as_i64()?;
                steered.insert(seq);
                let text = format!(
                    "Same-ticket update from the named sender:\n{}",
                    serde_json::to_string(&note).unwrap_or_default()
                );
                if text.len() > MAX_STEER_BYTES {
                    return None;
                }
                host.accept_notes(&[seq]);
                Some(format!(
                    "{}\n",
                    json!({"type": "user", "message": {"role": "user", "content": text}})
                ))
            };
            crate::proc::stream_live(proc, input, cancel, &mut on_event, on_tick).await
        }
    };
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
        err = quota_failure(err, last_rate_limit.as_ref(), usage_limit_reached);
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
        err = quota_failure(err, last_rate_limit.as_ref(), usage_limit_reached);
        if !err.quota_exhausted() {
            err.message = "Claude 未返回匹配会话的成功终态".into();
        }
        err.session_id = t["session_id"].as_str().map(str::to_string).or(session);
        return Err(err);
    }
    Ok(RunOutcome {
        text: t["result"].as_str().unwrap_or_default().to_string(),
        session_id: t["session_id"].as_str().map(str::to_string),
    })
}

fn quota_failure(error: AdapterError, info: Option<&Value>, usage_limit_reached: bool) -> AdapterError {
    let rejected = info.is_some_and(|v| v["status"] == "rejected" && v["isUsingOverage"] != true);
    if error.class == ErrorClass::Cancelled || !(usage_limit_reached || rejected || error.quota_exhausted()) {
        return error;
    }
    let kind = info
        .and_then(|v| v["rateLimitType"].as_str())
        .unwrap_or("window");
    let reset = info
        .and_then(|v| v["resetsAt"].as_f64())
        .zip(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok())
        .map(|(at, now)| at - now.as_secs_f64())
        .filter(|secs| secs.is_finite() && *secs > 0.0)
        .map(|secs| Duration::from_secs_f64(secs.min(MAX_QUOTA_RESET.as_secs_f64())))
        .or(error.retry_after);
    let mut limited = quota(kind, reset);
    limited.session_id = error.session_id;
    limited
}

fn terminal_reports_error(terminal: &Value) -> bool {
    terminal["is_error"] == true
        || terminal["subtype"]
            .as_str()
            .is_some_and(|kind| kind.starts_with("error"))
}

fn classify_claude_text(text: &str) -> AdapterError {
    let error = classify_text(text);
    if error.quota_exhausted() {
        return error;
    }
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
    error
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
            None,
            None,
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
    #[tokio::test]
    async fn rejected_subscription_window_refines_failure_and_preserves_early_session() {
        let future = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
            + 20 * 86_400;
        let info = json!({"type":"rate_limit_event","rate_limit_info":{
            "status":"rejected","rateLimitType":"seven_day","resetsAt":future,"isUsingOverage":false
        }});
        let mut terminal = result(true);
        terminal.as_object_mut().unwrap().remove("session_id");
        let error = fake_cli(vec![json!({"type":"system","subtype":"init","session_id":"early-session"}),
            info, terminal], 1, "").await.unwrap_err();
        assert!(error.quota_exhausted());
        assert_eq!(error.class, ErrorClass::Billing);
        assert_eq!(error.code.as_deref(), Some("quota:seven_day"));
        assert_eq!(error.retry_after, Some(Duration::from_secs(8 * 86_400)));
        assert_eq!(error.session_id.as_deref(), Some("early-session"));
        assert_eq!(error.message, "订阅额度窗口已用尽，等待额度重置后继续");
    }

    #[tokio::test]
    async fn usage_limit_markers_in_assistant_or_terminal_and_prose_become_quota() {
        for assistant in [false, true] {
            let mut terminal = result(true);
            let mut records = vec![];
            if assistant {
                records.push(json!({"type":"assistant","api_error":"usage_limit_reached",
                    "message":{"content":[{"type":"text","text":"private provider error"}]}}));
            } else {
                terminal["api_error"] = json!("usage_limit_reached");
            }
            records.push(terminal);
            let error = fake_cli(records, 0, "").await.unwrap_err();
            assert!(error.quota_exhausted());
            assert_eq!(error.code.as_deref(), Some("quota:window"));
            assert_eq!(error.session_id.as_deref(), Some("fixture-session"));
            assert_eq!(error.retry_after, None);
            assert!(!error.message.contains("private"));
        }
        let mut terminal = result(true);
        terminal["result"] = json!("API Error: 429 usage_limit_reached; try again in45minutes");
        let error = fake_cli(vec![terminal], 1, "").await.unwrap_err();
        assert!(error.quota_exhausted());
        assert_eq!(error.retry_after, Some(Duration::from_secs(45 * 60)));
    }

    #[tokio::test]
    async fn latest_window_overage_success_and_past_reset_keep_their_contracts() {
        let rejected = json!({"type":"rate_limit_event","rate_limit_info":{
            "status":"rejected","rateLimitType":"five_hour","resetsAt":1,"isUsingOverage":false
        }});
        let allowed = json!({"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}});
        let mut overage = rejected.clone();
        overage["rate_limit_info"]["isUsingOverage"] = json!(true);
        for records in [
            vec![rejected.clone(), allowed, assistant_error(), result(true)],
            vec![overage, assistant_error(), result(true)],
        ] {
            let error = fake_cli(records, 1, "").await.unwrap_err();
            assert_eq!(error.class, ErrorClass::Protocol);
            assert!(!error.quota_exhausted());
        }
        assert!(fake_cli(vec![rejected.clone(), result(false)], 0, "").await.is_ok());
        let error = fake_cli(vec![rejected, result(true)], 0, "").await.unwrap_err();
        assert!(error.quota_exhausted());
        assert_eq!(error.code.as_deref(), Some("quota:five_hour"));
        assert_eq!(error.retry_after, None);
    }

    #[test]
    fn quota_reset_accepts_fractional_unix_seconds_and_clamps_before_duration() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
        let info = json!({"status":"rejected","resetsAt":now + 3600.25});
        assert!(info["resetsAt"].as_u64().is_none());
        let error = quota_failure(classified(ErrorClass::Other, "fixture"), Some(&info), false);
        let reset = error.retry_after.expect("fractional future reset preserved").as_secs_f64();
        assert!((3590.0..=3600.25).contains(&reset));
        let huge = json!({"status":"rejected","resetsAt":1e300});
        let error = quota_failure(classified(ErrorClass::Other, "fixture"), Some(&huge), false);
        assert_eq!(error.retry_after, Some(MAX_QUOTA_RESET));
    }

    #[test]
    fn cancelled_turn_is_not_relabelled_as_quota() {
        let error = quota_failure(classified(ErrorClass::Cancelled, "cancelled"), None, true);
        assert_eq!(error.class, ErrorClass::Cancelled);
        assert!(!error.quota_exhausted());
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
        let settings = local_gateway_settings(&env, &cwd, true).unwrap();
        assert_eq!(
            settings["env"][MODEL_CAPABILITIES],
            "unrelated=effort;claude-fable-*=effort,mid_conv_system;claude-fable-5-2=-effort;claude-fable-*=effort;*=-mid_conv_system,-per_turn_effort"
        );
        assert_eq!(settings["env"].as_object().unwrap().len(), 1);
        assert!(!settings.to_string().contains("fixture-private"));
        std::fs::write(
            cwd.join(".claude/settings.local.json"),
            json!({"env":{MODEL_CAPABILITIES:"local=effort;"}}).to_string(),
        )
        .unwrap();
        assert_eq!(
            local_gateway_settings(&env, &cwd, true).unwrap()["env"]
                [MODEL_CAPABILITIES],
            "local=effort;*=-mid_conv_system,-per_turn_effort"
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
            local_gateway_settings(&env, data.path(), true).unwrap_err();
        assert!(!error.message.contains("invalid-json-private"));
        assert_eq!(
            local_gateway_settings(&env, data.path(), false).unwrap()["env"]
                [MODEL_CAPABILITIES],
            "shell=effort;claude-fable-5-2=effort;*=-mid_conv_system,-per_turn_effort"
        );
        std::fs::write(
            data.path().join(".claude/settings.json"),
            json!({"env":{MODEL_CAPABILITIES:42}}).to_string(),
        )
        .unwrap();
        assert!(local_gateway_settings(&env, data.path(), true).is_err());
    }
}
