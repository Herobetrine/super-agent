//! DeepSeek Harness adapter (port of `runtime/adapters/deepseek.cjs`,
//! spec §7.3).
//!
//! `node --expose-internals dsh/lib/bin.js --profile headless --patch
//! <policy.yml> --json [--session-id <id>] -`: the prompt goes to stdin and
//! NDJSON frames come back on stdout. The policy patch pins the provider,
//! model, sandbox and approval mode for this one turn.

use crate::common::{
    Emit, cc_rows, endpoint, is_file, modes, probe_failed, read_json, route_gone, scope, select,
    session_mismatch,
};
use crate::custom::{self, CustomProvider, Registry};
use crate::proc::{capture, start, stream};
use crate::sys::Sys;
use anyhow::{Result, anyhow};
use futures::future::BoxFuture;
use regex::Regex;
use sa_engine::adapter::{
    Adapter, AdapterError, Catalog, ErrorClass, EventSink, RunOutcome, RunRequest, TurnHost,
};
use sa_engine::resilience::classify::classified;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Pinned DSH runtime version (DS:21).
pub const VERSION: &str = "0.2.1-rc.5";
const FAILED: &str = "DeepSeek 未返回匹配会话的完整终稿";

static ENV_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z_]\w*$").expect("valid regex"));
static BASE_URL_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"_(BASE_URL|BASEURL)$").expect("valid regex"));

/// Module script that asks the installed DSH packages for the official
/// provider spec. Selectable efforts follow the installed Messages schema;
/// a disabled default is overridden explicitly when the operator selects effort.
const NATIVE_INFO: &str = r#"import {supportedProtocols} from '@deepseek-ai/dsh-llm-pi-ai';
import {resolveAdapterOptions} from '@deepseek-ai/dsh-llm-deepseek-api-key';
import {readFileSync} from 'node:fs';
const {cfg,env}=JSON.parse(readFileSync(0,'utf8'));
const a=resolveAdapterOptions(cfg,{get:k=>Object.hasOwn(env,k)?{value:env[k]}:undefined});
console.log(JSON.stringify({protocols:supportedProtocols(),url:a.baseURL,ref:a.apiKeyEnv,
 models:a.models.map(m=>({...m,reasoningEfforts:['off','low','high','max'],defaultReasoningEffort:a.defaults.thinking==='disabled'?'off':a.defaults.reasoningEffort??'high'}))}));"#;

pub struct DeepSeek {
    sys: Sys,
    registry: Registry,
}

impl DeepSeek {
    pub fn new(sys: Sys) -> Self {
        Self::with_registry(sys, custom::registry(vec![]))
    }

    pub fn with_registry(sys: Sys, registry: Registry) -> Self {
        Self { sys, registry }
    }
}

fn runtime(sys: &Sys) -> PathBuf {
    sys.var("SUPERAGENT_DSH_RUNTIME")
        .map(PathBuf::from)
        .unwrap_or_else(|| sys.platform_dir().join("dsh-runtime"))
}

fn dsh_home(sys: &Sys) -> PathBuf {
    sys.var("DSH_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| sys.home.join(".dsh"))
}

/// The Rust host is never Electron, so `node` always comes from PATH.
fn node(sys: &Sys) -> Option<PathBuf> {
    sys.which(if cfg!(windows) { "node.exe" } else { "node" })
}

fn cli(sys: &Sys) -> PathBuf {
    runtime(sys).join("node_modules/@deepseek-ai/dsh/lib/bin.js")
}

/// `yaml(p, def)` (DS:27-34): missing file → `def`; invalid YAML is an error.
fn yaml(p: &Path, def: Value) -> Result<Value> {
    if !is_file(p) {
        return Ok(def);
    }
    let text =
        std::fs::read_to_string(p).map_err(|_| anyhow!("DSH YAML 配置无效：{}", base_name(p)))?;
    let v: Value =
        serde_yaml::from_str(&text).map_err(|_| anyhow!("DSH YAML 配置无效：{}", base_name(p)))?;
    Ok(if v.is_null() { def } else { v })
}

fn base_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `plugins(rows)` (DS:35-39): enabled plugin records, `insert` groups first.
fn plugins(rows: &Value) -> Vec<Value> {
    let mut out = vec![];
    for r in rows.as_array().into_iter().flatten() {
        if !r.is_object() {
            continue;
        }
        out.extend(plugins(&r["insert"]));
        if truthy(&r["id"]) && !truthy(&r["disabled"]) {
            out.push(r.clone());
        }
    }
    out
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn non_empty(v: &Value) -> bool {
    v.as_object().is_some_and(|o| !o.is_empty())
}

/// `models(data)` (DS:56-63): `id → row`, array or `{id: {id, ...}}` form.
fn models(data: &Value) -> Map<String, Value> {
    let rows: Vec<&Value> = match data {
        Value::Array(a) => a.iter().collect(),
        Value::Object(o) => o
            .iter()
            .filter(|(k, v)| v["id"].as_str() == Some(k.as_str()))
            .map(|(_, v)| v)
            .collect(),
        _ => vec![],
    };
    let mut out = Map::new();
    for r in rows {
        if let Some(id) = r["id"].as_str().filter(|s| !s.is_empty()) {
            out.insert(id.to_string(), r.clone());
        }
    }
    out
}

/// `binding(c)` (DS:64): the `[api, baseURL, apiKeyEnv]` triple.
fn binding(c: &Value) -> [Value; 3] {
    ["api", "baseURL", "apiKeyEnv"].map(|k| c.get(k).cloned().unwrap_or(Value::Null))
}

/// `util.parseEnv` subset: `KEY=value`, optional `export `, quoted values
/// and `#` comments. Process variables still take precedence.
pub fn parse_env(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim();
        if key.is_empty() {
            continue;
        }
        let v = v.trim();
        let value = match v.chars().next() {
            Some(q @ ('"' | '\'' | '`')) if v.len() >= 2 && v[1..].contains(q) => {
                let end = v[1..].find(q).map(|i| i + 1).unwrap_or(v.len());
                let inner = &v[1..end];
                if q == '"' {
                    inner.replace("\\n", "\n")
                } else {
                    inner.to_string()
                }
            }
            _ => v.split(" #").next().unwrap_or_default().trim().to_string(),
        };
        out.insert(key.to_string(), value);
    }
    out
}

/// One runnable provider (DS:119-174).
#[derive(Clone, Debug)]
struct DsRoute {
    official: bool,
    label: String,
    url: String,
    api: Option<Value>,
    env_ref: String,
    key: Option<String>,
    store: PathBuf,
    models: Map<String, Value>,
    reasoning: Option<Value>,
}

struct Discovery {
    node: Option<PathBuf>,
    catalog: Value,
    routes: Vec<(String, DsRoute)>,
}

impl Discovery {
    fn route(&self, id: &str) -> Option<&DsRoute> {
        self.routes.iter().find(|(k, _)| k == id).map(|(_, r)| r)
    }
}

/// `info(cfg, env)` (DS:47-55): run the DSH packages' own resolver.
async fn info(
    sys: &Sys,
    node: &Path,
    cfg: &Value,
    env: &Value,
    cancel: &CancellationToken,
) -> Result<Value> {
    let args = ["--input-type=module", "-e", NATIVE_INFO].map(String::from);
    let input = json!({"cfg": cfg, "env": env}).to_string();
    let out = capture(
        node,
        &args,
        &runtime(sys),
        sys,
        &input,
        Duration::from_secs(10),
        cancel,
    )
    .await
    .map_err(|e| anyhow!(e.message))?;
    Ok(serde_json::from_str(out.trim())?)
}

fn set_route(routes: &mut Vec<(String, DsRoute)>, id: String, r: DsRoute) {
    match routes.iter_mut().find(|(k, _)| *k == id) {
        Some(slot) => slot.1 = r,
        None => routes.push((id, r)),
    }
}

/// `discover(signal)` (DS:65-210).
async fn discover(sys: &Sys, cancel: &CancellationToken) -> Result<Discovery> {
    let h = dsh_home(sys);
    let mut routes: Vec<(String, DsRoute)> = vec![];
    let mut native: Map<String, Value> = Map::new();
    let mut defaults: Map<String, Value> = Map::new();
    let mut official: Map<String, Value> = Map::new();
    let mut builtin: Option<String> = None;
    let node_exe = node(sys);
    let cli_path = cli(sys);
    let pkg = cli_path
        .parent()
        .and_then(Path::parent)
        .map(|d| d.join("package.json"));
    let ok = node_exe.is_some()
        && is_file(&cli_path)
        && pkg.is_some_and(|p| {
            read_json(&p, json!({})).is_ok_and(|v| v["version"] == json!(VERSION))
        });
    let routes_file = sys.platform_dir().join("deepseek-default.json");
    let base_file = runtime(sys).join("node_modules/@deepseek-ai/dsh-base/cordis.patch.yml");
    let mut files = vec![base_file.clone()];
    for rel in [
        "profiles/desktop/cordis.patch.yml",
        "profiles/headless/cordis.patch.yml",
        "cordis.patch.yml",
    ] {
        files.push(h.join(rel));
    }
    for p in &files {
        for plug in plugins(&yaml(p, json!([]))?) {
            let cfg = plug.get("config").cloned().unwrap_or_else(|| json!({}));
            if plug["id"] == json!("agent-default-model") {
                for k in ["model", "provider"] {
                    if cfg[k].is_string() {
                        defaults.insert(k.into(), cfg[k].clone());
                    }
                }
                if *p == base_file {
                    builtin = defaults
                        .get("provider")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
            }
            if plug["id"] == json!("llm-pi-ai") {
                for (id, v) in cfg["providers"].as_object().into_iter().flatten() {
                    native.insert(id.clone(), v.clone());
                }
            }
            if plug["id"] == json!("llm-deepseek") {
                for (k, v) in cfg.as_object().into_iter().flatten() {
                    official.insert(k.clone(), v.clone());
                }
            }
        }
    }
    let settings = yaml(&h.join("settings.yaml"), json!({}))?;
    for (id, v) in settings["llm-pi-ai"]["providers"]
        .as_object()
        .into_iter()
        .flatten()
    {
        native.insert(id.clone(), v.clone());
    }
    for (k, v) in settings["agent-default-model"]
        .as_object()
        .into_iter()
        .flatten()
    {
        defaults.insert(k.clone(), v.clone());
    }
    for (k, v) in settings["llm-deepseek"].as_object().into_iter().flatten() {
        official.insert(k.clone(), v.clone());
    }
    let store = h.join(".credentials.yaml");
    let refs = yaml(&store, json!({}))?["refs"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let dotenv_path = h.join(".env");
    let mut env: HashMap<String, String> = if is_file(&dotenv_path) {
        parse_env(&std::fs::read_to_string(&dotenv_path).unwrap_or_default())
    } else {
        HashMap::new()
    };
    for (k, v) in sys.vars() {
        env.insert(k.clone(), v.clone());
    }
    let env_get = |k: &str| env.get(k).filter(|v| !v.is_empty()).cloned();
    let mut clean_cfg = Map::new();
    for k in [
        "baseURL",
        "models",
        "apiKeyEnv",
        "thinking",
        "reasoningEffort",
    ] {
        if let Some(v) = official.get(k) {
            clean_cfg.insert(k.into(), v.clone());
        }
    }
    if let (true, Some(node_exe)) = (ok, &node_exe) {
        let base = info(sys, node_exe, &json!({}), &json!({}), cancel).await?;
        let attempt: Result<()> = async {
            let overrides: Map<String, Value> = env
                .iter()
                .filter(|(k, _)| BASE_URL_KEY.is_match(k))
                .map(|(k, v)| (k.clone(), json!(v)))
                .collect();
            let spec = if !clean_cfg.is_empty() || !overrides.is_empty() {
                info(
                    sys,
                    node_exe,
                    &Value::Object(clean_cfg.clone()),
                    &Value::Object(overrides),
                    cancel,
                )
                .await?
            } else {
                base.clone()
            };
            let u = endpoint(spec["url"].as_str().unwrap_or_default())?;
            let b = endpoint(base["url"].as_str().unwrap_or_default())?;
            let paired = u.origin() == b.origin()
                || (truthy(&clean_cfg.get("apiKeyEnv").cloned().unwrap_or_default())
                    && spec["ref"] != base["ref"]);
            let spec_ref = spec["ref"].as_str().unwrap_or_default().to_string();
            if let Some(builtin) = &builtin
                && paired
                && (env_get(&spec_ref).is_some() || refs.contains_key(&spec_ref))
            {
                set_route(
                    &mut routes,
                    builtin.clone(),
                    DsRoute {
                        official: true,
                        label: "DeepSeek 原生 API".into(),
                        url: spec["url"].as_str().unwrap_or_default().into(),
                        api: None,
                        key: env_get(&spec_ref),
                        env_ref: spec_ref,
                        store: store.clone(),
                        models: models(&spec["models"]),
                        reasoning: spec["models"]
                            .as_array()
                            .and_then(|ms| ms.first())
                            .and_then(|m| m.get("defaultReasoningEffort"))
                            .cloned(),
                    },
                );
            }
            Ok(())
        }
        .await;
        if attempt.is_err() && cancel.is_cancelled() {
            return Err(anyhow!("已取消"));
        }
        let protocols: Vec<Value> = base["protocols"].as_array().cloned().unwrap_or_default();
        let records = cc_rows(sys, &["dsh"])?;
        let ccids: HashSet<String> = records
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        let mut candidates: Vec<(String, String, Value, Option<String>)> = vec![];
        for row in &records {
            let Some(cfg) = row
                .get("settings_config")
                .and_then(Value::as_str)
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
            else {
                continue;
            };
            let o = cfg.get("options").cloned().unwrap_or_else(|| json!({}));
            let api_key = &o["apiKey"];
            let bad_key =
                !api_key.is_null() && !api_key.as_str().is_some_and(|s| !s.trim().is_empty());
            let base_mismatch = o
                .get("baseURL")
                .is_some_and(|b| Some(b) != cfg.get("baseURL"));
            if bad_key || base_mismatch || non_empty(&o["headers"]) {
                continue;
            }
            candidates.push((
                row.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                row.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                cfg.clone(),
                api_key.as_str().map(str::to_string),
            ));
        }
        for (id, cfg) in &native {
            if !ccids.contains(id) {
                let label = cfg["displayName"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(id)
                    .to_string();
                candidates.push((id.clone(), label, cfg.clone(), None));
            }
        }
        for (id, label, cfg, credential) in candidates {
            let Some(env_ref) = cfg["apiKeyEnv"].as_str().filter(|r| ENV_REF.is_match(r)) else {
                continue;
            };
            if builtin.as_deref() == Some(id.as_str())
                || !protocols.contains(&cfg["api"])
                || non_empty(&cfg["headers"])
                || endpoint(cfg["baseURL"].as_str().unwrap_or_default()).is_err()
            {
                continue;
            }
            let key = credential
                .filter(|k| !k.is_empty())
                .or_else(|| env_get(env_ref));
            let peer = native.get(&id);
            if (key.is_none() && !refs.contains_key(env_ref))
                || peer.is_some_and(|p| binding(p) != binding(&cfg))
            {
                continue;
            }
            let mut entries = peer.map(|p| models(&p["models"])).unwrap_or_default();
            for (k, v) in models(&cfg["models"]) {
                entries.insert(k, v);
            }
            {
                set_route(
                    &mut routes,
                    id,
                    DsRoute {
                        official: false,
                        label,
                        url: cfg["baseURL"].as_str().unwrap_or_default().into(),
                        api: Some(cfg["api"].clone()),
                        env_ref: env_ref.into(),
                        key,
                        store: store.clone(),
                        models: entries,
                        reasoning: cfg.get("reasoning").filter(|r| truthy(r)).cloned(),
                    },
                );
            }
        }
    }
    let fallback = json!({
        "mode": "cli",
        "provider": defaults.get("provider").and_then(Value::as_str).unwrap_or_default(),
        "model": defaults.get("model").and_then(Value::as_str).unwrap_or_default(),
    });
    let pref = read_json(&routes_file, fallback)?;
    let valid = pref.as_object().is_some_and(|o| {
        let mut keys: Vec<&str> = o.keys().map(String::as_str).collect();
        keys.sort_unstable();
        keys == ["mode", "model", "provider"]
            && o.values()
                .all(|v| v.as_str().is_some_and(|s| !s.is_empty()))
    });
    let default = if valid {
        json!({"mode": pref["mode"], "provider": pref["provider"], "model": pref["model"]})
    } else {
        json!({})
    };
    let failures = futures::future::join_all(routes.iter_mut().map(|(id, route)| {
        let refs = &refs;
        let node = node_exe.as_deref();
        async move {
            let key = route.key.clone().or_else(|| {
                refs.get(&route.env_ref)
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
            let Some(key) = key else {
                return Some((id.clone(), "缺少可用于读取目录的配对凭据".to_string()));
            };
            match online_models(sys, node, route, &key, cancel).await {
                Ok(found) => {
                    route.models = found;
                    None
                }
                Err(error) => Some((id.clone(), error.to_string())),
            }
        }
    }))
    .await;
    let catalog_errors: HashMap<String, String> = failures.into_iter().flatten().collect();
    let providers: Vec<Value> = routes
        .iter()
        .map(|(id, r)| {
            json!({
                "id": id,
                "label": r.label,
                "models": r.models.values().filter_map(crate::catalog::model).collect::<Vec<_>>(),
                "available": !r.models.is_empty(),
                "catalogError": catalog_errors.contains_key(id),
                "catalog_complete": !catalog_errors.contains_key(id),
                "detail": catalog_errors.get(id).map(|e| format!("在线目录获取失败：{e}；以下仅为已保存型号")).unwrap_or_else(|| "供应商在线模型目录".into()),
            })
        })
        .collect();
    let catalog = json!({
        "default": default,
        "providers": providers,
        "modes": modes("DeepSeek Harness CLI", ok),
        "downloads": [
            {"label": "DeepSeek Harness Windows 桌面版", "url": "https://download.deepseek.com/desktop/dsh-latest-windows-x64.exe"},
            {"label": "DeepSeek Harness 官网", "url": "https://www.deepseek.com/zh/harness/"},
            {"label": "Harness CLI 安装", "url": "https://github.com/deepseek-ai/deepseek-harness#run-from-npm"},
        ],
        "detail": "本机 DSH / CC Switch 与已保存工单默认来源，重新读取时生效。",
    });
    Ok(Discovery {
        node: node_exe,
        catalog,
        routes,
    })
}

async fn online_models(
    sys: &Sys,
    node: Option<&Path>,
    route: &DsRoute,
    key: &str,
    cancel: &CancellationToken,
) -> Result<Map<String, Value>> {
    let base = route.url.trim_end_matches('/');
    let anthropic = route.api.as_ref().and_then(Value::as_str) == Some("anthropic-messages");
    let target = if route.official {
        endpoint(base)?.join("/models")?.to_string()
    } else if anthropic {
        format!("{}/v1/models?limit=1000", custom::anthropic_root(base))
    } else {
        format!("{base}/models")
    };
    let mut headers = vec![("Authorization".into(), format!("Bearer {key}"))];
    if route.official || anthropic {
        headers.push(("x-api-key".into(), key.to_string()));
    }
    if anthropic {
        headers.push(("anthropic-version".into(), "2023-06-01".into()));
    }
    let mut found = crate::catalog::fetch(&target, &headers, cancel).await?;
    if route.official {
        // Match upstream's offered catalog, including version/protocol refusals.
        let node = node.ok_or_else(|| anyhow!("DeepSeek 运行时不可用"))?;
        let script = r#"import {parseModelListing,resolveListedCatalog,DEFAULT_MAX_TOKENS} from '@deepseek-ai/dsh-llm-deepseek';
import {APP_IDENTITY} from '@deepseek-ai/dsh-llm';
import {readFileSync} from 'node:fs';
const v=JSON.parse(readFileSync(0,'utf8'));
const r=resolveListedCatalog({listing:parseModelListing({data:v.listed}).entries,supported:v.supported,overrides:{},routeMaxTokens:DEFAULT_MAX_TOKENS,clientVersion:APP_IDENTITY.version});
console.log(JSON.stringify(r.offered));"#;
        let supported: Vec<Value> = route
            .models
            .values()
            .map(|m| {
                let mut m = m.clone();
                if let Some(o) = m.as_object_mut() {
                    o.remove("reasoningEfforts");
                    o.remove("defaultReasoningEffort");
                }
                m
            })
            .collect();
        let out = capture(
            node,
            &["--input-type=module".into(), "-e".into(), script.into()],
            &runtime(sys),
            sys,
            &json!({"listed":found,"supported":supported}).to_string(),
            Duration::from_secs(10),
            cancel,
        )
        .await
        .map_err(|_| anyhow!("DeepSeek 在线目录与当前执行端不兼容"))?;
        found = serde_json::from_str(&out)?;
        for m in &mut found {
            m["reasoningEfforts"] = json!(["off", "low", "high", "max"]);
            m["defaultReasoningEffort"] = route.reasoning.clone().unwrap_or_else(|| json!("high"));
        }
    } else {
        let metadata: Vec<Value> = route.models.values().cloned().collect();
        crate::catalog::supplement(&mut found, &metadata);
        for model in &mut found {
            if let Some(original) = route.models.get(model["id"].as_str().unwrap_or_default()) {
                if let Some(mapping) = original.get("reasoningEfforts") {
                    model["reasoningEfforts"] = mapping.clone();
                }
                for field in ["input", "contextWindow", "maxTokens", "compat"] {
                    if let Some(v) = original.get(field) {
                        model[field] = v.clone();
                    }
                }
            }
            restore_effort_mapping(model);
        }
    }
    anyhow::ensure!(!found.is_empty(), "在线目录没有当前执行端支持的模型");
    Ok(models(&json!(found)))
}

/// A DSH `llm-pi-ai` route for an operator provider (OpenAI chat
/// completions). The key comes from `sys` only; a keyless loopback endpoint
/// gets the non-secret placeholder so DSH never consults its credential
/// store.
fn custom_route(p: &CustomProvider, sys: &Sys) -> DsRoute {
    let key = custom::key(p, sys)
        .ok()
        .map(|k| k.unwrap_or_else(|| custom::LOCAL_PLACEHOLDER.to_string()));
    DsRoute {
        official: false,
        label: p.name.clone(),
        url: p.base_url.clone(),
        api: Some(json!(custom::DSH_PROTOCOL)),
        env_ref: p.key_env.clone(),
        key,
        store: dsh_home(sys).join(".credentials.yaml"),
        models: p
            .models
            .iter()
            .map(|m| (m.clone(), json!({"id": m, "name": m})))
            .collect(),
        reasoning: None,
    }
}

/// Merge operator providers into a discovery: catalog rows plus runnable
/// routes for the rows that were not shadowed by a native provider.
fn merge_custom(d: &mut Discovery, customs: &[CustomProvider], sys: &Sys) {
    custom::merge(&mut d.catalog, customs, sys);
    for p in customs {
        let id = p.route_id();
        if custom::lookup(&d.catalog, customs, &id).is_some() && d.route(&id).is_none() {
            d.routes.push((id, custom_route(p, sys)));
        }
    }
}

async fn refresh_custom(
    d: &mut Discovery,
    customs: &[CustomProvider],
    sys: &Sys,
    cancel: &CancellationToken,
) {
    custom::refresh(&mut d.catalog, customs, sys, cancel).await;
    for (id, route) in &mut d.routes {
        if !customs.iter().any(|p| p.route_id() == *id) {
            continue;
        }
        if let Some(provider) = d.catalog["providers"]
            .as_array()
            .and_then(|ps| ps.iter().find(|p| p["id"] == *id))
        {
            route.models = models(&provider["models"]);
            for model in route.models.values_mut() {
                restore_effort_mapping(model);
            }
        }
    }
}

fn yaml_list(items: Value) -> String {
    serde_yaml::to_string(&items).unwrap_or_default()
}

fn restore_effort_mapping(model: &mut Value) {
    if model["reasoningEfforts"].is_array() {
        let levels = crate::catalog::efforts(model);
        let mapping: Map<String, Value> = levels
            .iter()
            .map(|level| {
                let wire = model["reasoningEffortMapping"]
                    .get(level)
                    .cloned()
                    .unwrap_or_else(|| json!(level));
                (level.clone(), wire)
            })
            .collect();
        model["reasoningEfforts"] = if mapping.is_empty() {
            json!(false)
        } else {
            json!(mapping)
        };
    }
}

/// The per-turn policy patch (DS:233-262). The `sandbox-policy` record is
/// written literally because it carries the DSH-specific `!!js` tag.
fn policy(provider: &str, model: &str, boundary: &str, r: &DsRoute, spec: Value) -> String {
    let llm = if r.official {
        "llm-deepseek"
    } else {
        "llm-pi-ai"
    };
    let other = if r.official {
        "llm-pi-ai"
    } else {
        "llm-deepseek"
    };
    let llm_config = if r.official {
        spec
    } else {
        json!({"providers": {provider: spec}})
    };
    let mut out = yaml_list(
        json!([{"id": "agent-default-model", "config": {"provider": provider, "model": model}}]),
    );
    out.push_str(&format!(
        "- id: sandbox-policy\n  config:\n    mode: {boundary}\n    workspaceRoot: !!js process.cwd()\n"
    ));
    out.push_str(&yaml_list(json!([
        {"id": "approval", "config": {"policy": "never"}},
        {"id": "permission", "config": {
            "defaultPreset": boundary,
            "presets": {boundary: {"sandbox": boundary, "approval": "never"}},
        }},
        {"id": other, "disabled": true},
        {"id": llm, "config": llm_config},
    ])));
    if r.key.is_none() {
        out.push_str(&yaml_list(json!([
            {"id": "credentials", "disabled": false, "config": {"path": r.store, "watch": false}}
        ])));
    }
    out
}

const ALLOWED_MODEL_FIELDS: [&str; 7] = [
    "id",
    "name",
    "contextWindow",
    "input",
    "reasoningEfforts",
    "maxTokens",
    "compat",
];

/// Variable the per-turn patch names when the adapter supplies the key.
const CALL_KEY: &str = "SUPERAGENT_DSH_CALL_KEY";

/// The provider spec for the policy patch and, when the adapter holds the
/// key, the `(variable, key)` pair to put in the child environment. The key
/// itself never enters the patch file.
fn call_spec(
    r: &DsRoute,
    model: &str,
    effort: Option<&str>,
) -> (Value, Option<(&'static str, String)>) {
    let env_ref = if r.key.is_some() {
        CALL_KEY.to_string()
    } else {
        r.env_ref.clone()
    };
    let selected: Map<String, Value> = r
        .models
        .get(model)
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(k, _)| ALLOWED_MODEL_FIELDS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut spec = Map::new();
    spec.insert("baseURL".into(), json!(r.url));
    spec.insert("apiKeyEnv".into(), json!(env_ref));
    spec.insert(
        "models".into(),
        if r.official {
            json!(
                r.models
                    .values()
                    .map(|m| m
                        .as_object()
                        .into_iter()
                        .flatten()
                        .filter(|(k, _)| [
                            "id",
                            "name",
                            "description",
                            "inputModalities",
                            "contextWindow",
                            "maxTokens",
                            "systemPromptUpdate",
                            "toolUpdate"
                        ]
                        .contains(&k.as_str()))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect::<Map<String, Value>>())
                    .collect::<Vec<_>>()
            )
        } else {
            json!([selected])
        },
    );
    if !r.official {
        spec.insert("api".into(), r.api.clone().unwrap_or(Value::Null));
        if let Some(reasoning) = &r.reasoning {
            spec.insert("reasoning".into(), reasoning.clone());
        }
    }
    let effort = effort.or_else(|| {
        r.official
            .then(|| r.reasoning.as_ref().and_then(Value::as_str))
            .flatten()
    });
    if let Some(effort) = effort {
        spec.insert(
            if r.official {
                "reasoningEffort"
            } else {
                "reasoning"
            }
            .into(),
            json!(effort),
        );
        if r.official {
            spec.insert(
                "thinking".into(),
                json!(if effort == "off" {
                    "disabled"
                } else {
                    "enabled"
                }),
            );
        }
    }
    (Value::Object(spec), r.key.clone().map(|k| (CALL_KEY, k)))
}

impl Adapter for DeepSeek {
    fn agent(&self) -> &'static str {
        "deepseek"
    }

    fn catalog(&self) -> BoxFuture<'static, anyhow::Result<Catalog>> {
        let sys = self.sys.clone();
        let customs = custom::snapshot(&self.registry, "deepseek");
        Box::pin(async move {
            let mut d = discover(&sys, &CancellationToken::new()).await?;
            merge_custom(&mut d, &customs, &sys);
            refresh_custom(&mut d, &customs, &sys, &CancellationToken::new()).await;
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
        let customs = custom::snapshot(&self.registry, "deepseek");
        Box::pin(async move { run(sys, customs, request, host, Emit(events), cancel).await })
    }
}

/// `run(o)` (DS:211-293).
async fn run(
    sys: Sys,
    customs: Vec<CustomProvider>,
    req: RunRequest,
    host: Arc<dyn TurnHost>,
    emit: Emit,
    cancel: CancellationToken,
) -> Result<RunOutcome, AdapterError> {
    crate::images::check(&req)?;
    let boundary = scope("deepseek", &req, &host, &cancel, true).await?;
    let mut d = discover(&sys, &cancel)
        .await
        .map_err(|e| probe_failed(&e.to_string()))?;
    merge_custom(&mut d, &customs, &sys);
    refresh_custom(&mut d, &customs, &sys, &cancel).await;
    let s = select(&d.catalog, &req.route)?;
    let r = d
        .route(&s.provider)
        .cloned()
        .ok_or_else(|| route_gone("已选供应商或模型失效，请刷新配置"))?;
    let node_exe = d
        .node
        .clone()
        .ok_or_else(|| route_gone("本机未安装所选原生执行端"))?;
    let mut env = sys.clone();
    env.set(
        "DSH_HOME",
        sys.platform_dir()
            .join("dsh-state")
            .join(boundary)
            .to_string_lossy()
            .into_owned(),
    );
    env.set("DSH_PERMISSION_MODE", boundary);
    let (spec, call_key) = call_spec(&r, &s.model, s.reasoning_effort.as_deref());
    if let Some((name, key)) = call_key {
        env.set(name, key);
    }
    let policy_text = policy(&s.provider, &s.model, boundary, &r, spec);
    let temp = tempfile::Builder::new()
        .prefix("superagent-dsh-")
        .tempdir_in(&sys.temp)
        .map_err(|_| classified(ErrorClass::Other, "dsh:temp"))?;
    let patch = temp.path().join("policy.yml");
    write_private(&patch, &policy_text).map_err(|_| classified(ErrorClass::Other, "dsh:policy"))?;
    let mut args: Vec<String> = vec![
        "--expose-internals".into(),
        cli(&sys).to_string_lossy().into_owned(),
        "--profile".into(),
        "headless".into(),
        "--patch".into(),
        patch.to_string_lossy().into_owned(),
        "--json".into(),
    ];
    if let Some(sid) = &req.session_id {
        args.push("--session-id".into());
        args.push(sid.clone());
    }
    args.push("-".into());
    emit.journal(
        "adapter.permission",
        json!({"profile": req.profile, "scope": boundary}),
    );
    emit.journal(
        "adapter.route",
        json!({"provider": s.provider, "requested_model": s.model, "mode": "cli"}),
    );
    let mut proc = start(&node_exe, &args, &req.cwd, &env)?;
    let mut text: Option<String> = None;
    let mut session: Option<String> = None;
    let mut turn_error = false;
    let mut open_tools: HashSet<String> = HashSet::new();
    let streamed = stream(&mut proc, &req.prompt, &cancel, |e| {
        if e["type"] == json!("thinking") {
            return Ok(());
        }
        emit.journal("native", e.clone());
        match e["type"].as_str() {
            Some("session") => {
                if let Some(id) = e["sessionId"].as_str() {
                    emit.session(id);
                    session = Some(id.to_string());
                }
            }
            Some("text") => {
                if let Some(t) = e["text"].as_str() {
                    emit.journal("delta", json!(t));
                    emit.journal("message.boundary", Value::Null);
                }
            }
            Some("final") => text = e["text"].as_str().map(str::to_string),
            // Tool events are not a verified DSH schema (spec §9.2); tracking
            // them only lengthens the stall timeout while a tool is open.
            Some("tool_call") => {
                if let Some(id) = e["callId"].as_str().filter(|s| !s.is_empty())
                    && open_tools.insert(id.to_string())
                {
                    emit.tool_started(id);
                }
            }
            Some("tool_result") => {
                if let Some(id) = e["callId"].as_str()
                    && open_tools.remove(id)
                {
                    emit.tool_finished(id);
                }
            }
            Some("status")
                if e["phase"] == json!("turn_end") && e["reason"]["kind"] == json!("error") =>
            {
                turn_error = true;
            }
            _ => {}
        }
        Ok(())
    })
    .await;
    drop(temp);
    if let Err(mut err) = streamed {
        err.session_id = err.session_id.or(session).or(req.session_id.clone());
        return Err(err);
    }
    if let (Some(want), Some(got)) = (&req.session_id, &session)
        && want != got
    {
        return Err(session_mismatch(FAILED));
    }
    match (text.filter(|t| !t.is_empty()), session) {
        (Some(text), Some(sid)) => Ok(RunOutcome {
            text,
            session_id: Some(sid),
        }),
        (_, sid) => {
            let code = if turn_error {
                "dsh:turn_error"
            } else {
                "dsh:no_final"
            };
            let class = if turn_error {
                ErrorClass::Other
            } else {
                ErrorClass::ProcessCrashed
            };
            let mut err = classified(class, code);
            err.message = FAILED.into();
            err.session_id = sid.or(req.session_id.clone());
            Err(err)
        }
    }
}

fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    opts.open(path)?.write_all(content.as_bytes())
}

/// Files whose change invalidates the catalog (DS:295-319).
pub fn watch_paths(sys: &Sys) -> Vec<PathBuf> {
    let h = dsh_home(sys);
    let pkg = runtime(sys).join("node_modules/@deepseek-ai");
    let mut out = vec![sys.platform_dir().join("deepseek-default.json")];
    for n in [
        ".credentials.yaml",
        ".env",
        "settings.yaml",
        "profiles/desktop/cordis.patch.yml",
        "profiles/headless/cordis.patch.yml",
        "cordis.patch.yml",
    ] {
        out.push(h.join(n));
    }
    out.push(pkg.join("dsh-base/cordis.patch.yml"));
    out.push(cli(sys));
    out.push(pkg.join("dsh/package.json"));
    out.extend(node(sys));
    for name in [
        "dsh-llm-deepseek",
        "dsh-llm-deepseek-api-key",
        "dsh-llm-pi-ai",
    ] {
        out.push(pkg.join(name).join("package.json"));
        out.push(pkg.join(name).join("lib/index.js"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotenv_subset_matches_node_parse_env() {
        let env = parse_env("# c\nexport A=1\nB = \"x\\ny\"\nC='q # r'\nD=plain # note\n\nbad\n");
        assert_eq!(env["A"], "1");
        assert_eq!(env["B"], "x\ny");
        assert_eq!(env["C"], "q # r");
        assert_eq!(env["D"], "plain");
        assert!(!env.contains_key("bad"));
    }

    #[test]
    fn plugins_flatten_insert_groups_and_skip_disabled() {
        let rows = json!([
            {"insert": [{"id": "a"}, {"id": "b", "disabled": true}]},
            {"id": "c"},
            "noise"
        ]);
        let ids: Vec<Value> = plugins(&rows)
            .into_iter()
            .map(|r| r["id"].clone())
            .collect();
        assert_eq!(ids, vec![json!("a"), json!("c")]);
    }

    #[test]
    fn model_rows_accept_array_and_keyed_forms() {
        let a = models(&json!([{"id": "x"}, {"id": ""}, {"name": "n"}]));
        assert_eq!(a.keys().collect::<Vec<_>>(), vec!["x"]);
        let b = models(&json!({"y": {"id": "y"}, "z": {"id": "other"}}));
        assert_eq!(b.keys().collect::<Vec<_>>(), vec!["y"]);
    }

    fn custom_provider(base: &str, key_env: &str) -> CustomProvider {
        CustomProvider {
            id: "gw".into(),
            agent: "deepseek".into(),
            name: "网关".into(),
            base_url: base.into(),
            key_env: key_env.into(),
            models: vec!["m-1".into(), "m-2".into()],
            enabled: true,
        }
    }

    #[test]
    fn custom_providers_become_pi_ai_routes_with_the_key_in_env_only() {
        let sys = Sys::new(
            vec![("GW_KEY".into(), "sk-secret".into())],
            PathBuf::from("/nonexistent-home"),
            std::env::temp_dir(),
        );
        let mut d = Discovery {
            node: None,
            catalog: json!({"providers": [], "modes": modes("DSH", true), "default": {}}),
            routes: vec![],
        };
        let customs = vec![custom_provider("https://gw.example/v1", "GW_KEY")];
        merge_custom(&mut d, &customs, &sys);
        assert_eq!(d.catalog["providers"][0]["id"], "sa-gw");
        let r = d.route("sa-gw").expect("route").clone();
        let (spec, key) = call_spec(&r, "m-2", None);
        assert_eq!(key, Some((CALL_KEY, "sk-secret".to_string())));
        assert_eq!(spec["api"], "openai-completions");
        assert_eq!(spec["apiKeyEnv"], CALL_KEY);
        assert_eq!(spec["baseURL"], "https://gw.example/v1");
        assert_eq!(spec["models"], json!([{"id": "m-2", "name": "m-2"}]));
        let text = policy("sa-gw", "m-2", "read-only", &r, spec);
        assert!(!text.contains("sk-secret"));
        assert!(!text.contains("credentials"));
        assert!(text.contains("llm-pi-ai"));

        let keyless = custom_route(&custom_provider("http://127.0.0.1:9/v1", ""), &sys);
        assert_eq!(keyless.key.as_deref(), Some(custom::LOCAL_PLACEHOLDER));
        let missing = custom_route(&custom_provider("https://gw.example/v1", "NOPE"), &sys);
        assert_eq!(missing.key, None);
    }

    #[test]
    fn policy_pins_route_and_keeps_the_js_tag_literal() {
        let r = DsRoute {
            official: false,
            label: "p".into(),
            url: "https://example.invalid/v1".into(),
            api: Some(json!("openai-completions")),
            env_ref: "SUPERAGENT_DSH_CALL_KEY".into(),
            key: Some("k".into()),
            store: PathBuf::from("store"),
            models: Map::new(),
            reasoning: None,
        };
        let text = policy(
            "prov",
            "mod",
            "workspace-write",
            &r,
            json!({"baseURL": r.url}),
        );
        assert!(text.contains("workspaceRoot: !!js process.cwd()"));
        assert!(text.contains("- id: llm-deepseek\n  disabled: true"));
        assert!(!text.contains("credentials"));
        assert!(!text.contains("\"k\""));
        let parsed: Vec<serde_yaml::Value> =
            serde_yaml::from_str(&text.replace("!!js process.cwd()", "x")).expect("valid YAML");
        assert_eq!(parsed.len(), 6);
    }

    #[test]
    fn explicit_effort_can_enable_a_disabled_official_default() {
        let sys = Sys::new(
            vec![],
            PathBuf::from("/nonexistent-home"),
            std::env::temp_dir(),
        );
        let mut route = custom_route(&custom_provider("http://localhost:9", ""), &sys);
        route.official = true;
        route.reasoning = Some(json!("off"));
        let (default, _) = call_spec(&route, "m-1", None);
        assert_eq!(default["thinking"], "disabled");
        assert_eq!(default["reasoningEffort"], "off");
        let (high, _) = call_spec(&route, "m-1", Some("high"));
        assert_eq!(high["thinking"], "enabled");
        assert_eq!(high["reasoningEffort"], "high");
        let patch = policy("deepseek-official", "m-1", "read-only", &route, high);
        assert!(patch.contains("thinking: enabled"));
        assert!(patch.contains("reasoningEffort: high"));
        route.official = false;
        let (third_party, _) = call_spec(&route, "m-1", Some("high"));
        assert_eq!(third_party["reasoning"], "high");
        assert!(third_party.get("thinking").is_none());
        let mut mapped =
            crate::catalog::model(&json!({"id":"m","reasoningEfforts":{"off":null,"high":"max"}}))
                .unwrap();
        restore_effort_mapping(&mut mapped);
        assert_eq!(mapped["reasoningEfforts"], json!({"off":null,"high":"max"}));
    }
}
