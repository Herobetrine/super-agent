//! Codex configuration and model discovery (port of `codex-config.cjs`).

use crate::common::{endpoint, modes, read_json};
use crate::probe_cache::{
    ModelProbeResult, ProbeCache, ProbeKey, Probes, config_digest, file_stamp, supplement_efforts,
};
use crate::proc::capture;
use crate::sys::{Sys, resolve};
use anyhow::{Result, anyhow, bail};
use futures::future::join_all;
use regex::Regex;
use sa_engine::adapter::Route;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

static PROVIDER_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\w-]{1,96}$").expect("valid regex"));
static PROFILE_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]+$").expect("valid regex"));

pub fn id_ok(v: &Value) -> bool {
    v.as_str().is_some_and(sa_store::providers::valid_model_id)
}

pub fn config_home(sys: &Sys) -> PathBuf {
    sys.var("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| sys.home.join(".codex"))
}

/// `executable()` (CXC:17-34): the native binary behind the npm shim.
pub fn executable(sys: &Sys) -> Option<PathBuf> {
    if !cfg!(windows) {
        return sys.which("codex");
    }
    if let Some(shim) = sys.which("codex.cmd") {
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "x64"
        };
        let vendor = shim
            .parent()
            .unwrap_or(Path::new("."))
            .join("node_modules/@openai/codex/node_modules/@openai")
            .join(format!("codex-win32-{arch}"))
            .join("vendor");
        if let Ok(entries) = std::fs::read_dir(&vendor) {
            let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            dirs.sort();
            for d in dirs {
                let f = d.join("bin/codex.exe");
                if f.is_file() {
                    return Some(f);
                }
            }
        }
    }
    sys.which("codex.exe")
}

fn toml_file(p: &Path) -> Result<Value> {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let text = std::fs::read_to_string(p).map_err(|_| anyhow!("无法读取 Codex 配置 {name}"))?;
    let table: toml::Table =
        toml::from_str(&text).map_err(|_| anyhow!("无法读取 Codex 配置 {name}"))?;
    Ok(serde_json::to_value(table)?)
}

fn merge(a: &Value, b: &Value) -> Value {
    let mut c = a.as_object().cloned().unwrap_or_default();
    for (k, v) in b.as_object().into_iter().flatten() {
        let merged = match (v, c.get(k)) {
            (Value::Object(_), Some(Value::Object(_))) => merge(&c[k], v),
            _ => v.clone(),
        };
        c.insert(k.clone(), merged);
    }
    Value::Object(c)
}

pub struct Settings {
    pub config: Value,
    pub profile: Option<String>,
    pub dir: PathBuf,
    pub saved: Vec<Value>,
}

/// `settings()` (CXC:50-65).
pub fn settings(sys: &Sys) -> Result<Settings> {
    let dir = config_home(sys);
    let f = dir.join("config.toml");
    let base = if f.is_file() {
        toml_file(&f)?
    } else {
        json!({})
    };
    let profile = sys
        .get("SUPERAGENT_CODEX_PROFILE")
        .map(str::trim)
        .filter(|p| !p.is_empty());
    if let Some(p) = profile
        && !PROFILE_NAME.is_match(p)
    {
        bail!("Codex profile 名无效");
    }
    let config = match profile {
        Some(p) => merge(&base, &toml_file(&dir.join(format!("{p}.config.toml")))?),
        None => base.clone(),
    };
    let mut saved = vec![base.clone(), config.clone()];
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut names: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().ends_with(".config.toml"))
            })
            .collect();
        names.sort();
        for p in names {
            if let Ok(v) = toml_file(&p) {
                saved.push(merge(&base, &v));
            }
        }
    }
    Ok(Settings {
        config,
        profile: profile.map(str::to_string),
        dir,
        saved,
    })
}

/// `visible(data)` (CXC:67-70).
fn visible(data: &Value) -> Vec<Value> {
    data["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| {
            m.get("visibility")
                .and_then(Value::as_str)
                .unwrap_or("list")
                == "list"
        })
        .filter_map(crate::catalog::model)
        .collect()
}

fn file_models(path: &Path) -> Result<Vec<Value>> {
    if std::fs::metadata(path)?.len() > 32 * 1024 * 1024 {
        bail!("too large");
    }
    Ok(visible(&read_json(path, json!({}))?))
}

pub(crate) fn local_catalog(sys: &Sys) -> Result<Value> {
    let Settings {config, dir, ..} = settings(sys)?;
    let provider = match config.get("model_provider") {
        None => "openai",
        Some(value) => value.as_str().filter(|id| PROVIDER_ID.is_match(id))
            .ok_or_else(|| anyhow!("当前 Codex 配置含有无效的供应商 ID"))?,
    };
    if !config["model"].is_null() && !id_ok(&config["model"]) {
        bail!("当前 Codex 配置含有无效的模型 ID");
    }
    let mut models = config["model_catalog_json"].as_str()
        .and_then(|p| file_models(&resolve(&dir, p)).ok()).unwrap_or_default();
    let configured = config["model"].as_str();
    let default = configured.or_else(|| models.first()?.get("id")?.as_str());
    let route = json!({"mode":"cli","provider":provider,"model":default});
    let mut local = crate::compatibility::local(route, executable(sys).is_some());
    if !models.is_empty() {
        if let Some(id) = configured && !models.iter().any(|r| r["id"] == id) {
            models.push(json!({"id":id}));
        }
        local["providers"][0]["models"] = Value::Array(models);
    }
    Ok(local)
}

/// `customModels(cfg)` (CXC:71-86).
async fn custom_models(sys: &Sys, cfg: &Value, cancel: &CancellationToken) -> Result<ModelProbeResult> {
    let origin = endpoint(cfg["base_url"].as_str().unwrap_or_default())?;
    let catalog_url = cfg["model_catalog_url"]
        .as_str()
        .map(|u| url::Url::parse(u).map_err(|_| anyhow!("目录和认证来源不匹配")))
        .transpose()?;
    if let Some(u) = &catalog_url
        && (u.origin() != origin.origin()
            || !u.username().is_empty()
            || u.password().is_some()
            || u.fragment().is_some())
    {
        bail!("目录和认证来源不匹配");
    }
    let mut headers: Vec<(String, String)> = cfg["http_headers"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
        .collect();
    for (name, var) in cfg["env_http_headers"].as_object().into_iter().flatten() {
        if let Some(v) = var.as_str().and_then(|r| sys.var(r)) {
            headers.retain(|(k, _)| k != name);
            headers.push((name.clone(), v.to_string()));
        }
    }
    // Codex's API-key auth is valid for the explicitly selected provider. OAuth
    // access tokens remain with the CLI and are never sent to custom origins.
    let auth = if cfg["requires_openai_auth"] == true {
        read_json(&config_home(sys).join("auth.json"), json!({}))?
    } else {
        json!({})
    };
    let token = cfg["experimental_bearer_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .or_else(|| cfg["env_key"].as_str().and_then(|k| sys.var(k)))
        .or_else(|| auth["OPENAI_API_KEY"].as_str());
    if let Some(t) = token {
        headers.push(("Authorization".into(), format!("Bearer {t}")));
    }
    let target = match &catalog_url {
        Some(u) => u.to_string(),
        None => format!("{}/models", origin.as_str().trim_end_matches('/')),
    };
    crate::catalog::fetch_with_status(&target, &headers, cancel).await
}

const NATIVE_SELECTED: &str = "native:selected";
const NATIVE_OPENAI: &str = "native:openai";

struct ProviderProbes {
    primary: Option<String>,
    inferred_efforts: HashSet<String>,
}

pub struct Discovery {
    pub exe: Option<PathBuf>,
    pub config: Value,
    pub profile: Option<String>,
    pub dir: PathBuf,
    pub catalog: Value,
}

pub(crate) struct RouteProbes(HashMap<String, ProviderProbes>);

impl RouteProbes {
    pub(crate) fn route_slots(&self, route: &Route) -> Vec<&str> {
        let Some(probes) = self.0.get(&route.provider) else { return Vec::new() };
        let mut slots: Vec<_> = probes.primary.as_deref().into_iter().collect();
        if route.reasoning_effort.is_some() && probes.inferred_efforts.contains(&route.model)
            && probes.primary.as_deref() != Some(NATIVE_SELECTED)
        {
            slots.push(NATIVE_SELECTED);
        }
        slots
    }
}

pub(crate) fn probe_key(sys: &Sys, exe: Option<&Path>, customs: &[crate::custom::CustomProvider]) -> ProbeKey {
    let dir = config_home(sys);
    // The CLI rewrites this generated catalog during ordinary runs.
    let cli_cache = dir.join("models_cache.json");
    let mut paths = crate::revision::watch_paths(sys, "codex");
    if let Some(index) = paths.iter().position(|path| path == &cli_cache) {
        // Keep any separate occurrence from an explicit model_catalog_json.
        paths.remove(index);
    }
    let config: Vec<_> = paths.iter()
        .filter(|path| path.parent() == Some(dir.as_path()) && path.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            name == "config.toml" || name.ends_with(".config.toml")
        }))
        .map(|path| (path, config_digest(path)))
        .collect();
    ProbeKey::new("codex", sys, exe, json!([
        crate::revision::fingerprint(&paths), config, customs,
        file_stamp(&dir.join("auth.json")),
    ]))
}

async fn native_models(
    exe: &Path, args: &[String], sys: &Sys, timeout: Duration, cancel: &CancellationToken,
) -> Result<ModelProbeResult> {
    let output = capture(exe, args, &sys.home, sys, "", timeout, cancel)
        .await.map_err(|e| anyhow!(e.message))?;
    Ok((visible(&serde_json::from_str(&output)?), None))
}

/// `discover()` (CXC:87-173).
pub async fn discover(sys: &Sys, cancel: &CancellationToken) -> Result<Discovery> {
    let exe = executable(sys);
    let key = probe_key(sys, exe.as_deref(), &[]);
    let cache = ProbeCache::new(None);
    let probes = Probes::new(&cache, &key, false);
    discover_cached(sys, exe, cancel, &probes, None).await.map(|(discovery, _)| discovery)
}

pub(crate) async fn discover_cached(
    sys: &Sys, exe: Option<PathBuf>, cancel: &CancellationToken, probes: &Probes<'_>,
    selected: Option<&Route>,
) -> Result<(Discovery, RouteProbes)> {
    let Settings {
        config,
        profile,
        dir,
        saved,
    } = settings(sys)?;
    let custom = config["model_providers"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let def = config
        .get("model_provider")
        .cloned()
        .unwrap_or_else(|| json!("openai"));
    let def = match def.as_str() {
        Some(d)
            if PROVIDER_ID.is_match(d)
                && (config["model"].is_null() || id_ok(&config["model"])) =>
        {
            d.to_string()
        }
        _ => bail!("当前 Codex 配置含有无效的模型或供应商 ID"),
    };
    let file_catalog = config["model_catalog_json"]
        .as_str()
        .map(|p| resolve(&dir, p));
    let optional_provider = |name: &str| selected.is_some_and(|route| route.provider != name);
    let default_uses_native = file_catalog.is_none()
        && custom
            .get(&def)
            .and_then(|cfg| cfg["base_url"].as_str())
            .is_none()
        && (def != "openai" || !config["openai_base_url"].is_string());
    let optional_native = selected.is_some() && (optional_provider(&def) || !default_uses_native);
    let native = match &exe {
        Some(exe) => probes.fetch_models(
            NATIVE_SELECTED,
            optional_native,
            cancel,
            native_models(exe, &["debug".into(), "models".into()], sys, Duration::from_secs(15), cancel),
        ).await.map(|(models, _)| models),
        None => Err(anyhow!("本机未安装 Codex CLI")),
    };
    let mut names: Vec<String> = vec!["openai".into()];
    for n in custom.keys().cloned().chain([def.clone()]) {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    names.retain(|n| PROVIDER_ID.is_match(n));
    let sources = names.iter().map(|name| {
        let (sys, config, custom, file_catalog, exe, def, saved, native) =
            (sys, &config, &custom, &file_catalog, &exe, &def, &saved, &native);
        async move {
            let mut cfg = custom.get(name).cloned();
            if name == "openai"
                && let Some(base) = config["openai_base_url"].as_str()
            {
                let mut c = cfg.and_then(|c| c.as_object().cloned()).unwrap_or_default();
                c.insert("base_url".into(), json!(base));
                cfg = Some(Value::Object(c));
            }
            let mut models: Vec<Value> = vec![];
            let mut detail = "没有该来源的模型目录".to_string();
            let mut primary = None;
            let mut fallback = None;
            let attempt: Result<()> = async {
                if let (Some(fc), true) = (file_catalog, name == def) {
                    models = file_models(fc)?;
                    detail = "本机 model_catalog_json".into();
                } else if let Some(c) = cfg.as_ref().filter(|c| c["base_url"].as_str().is_some()) {
                    let slot = primary.insert(format!("provider:{name}"));
                    (models, fallback) = probes.fetch_models(
                        slot, optional_provider(name), cancel, custom_models(sys, c, cancel),
                    ).await?;
                    detail = "供应商在线模型目录".into();
                } else if name == def {
                    primary = Some(NATIVE_SELECTED.into());
                    models = native.as_ref().map_err(|e| anyhow!("{e}"))?.clone();
                    detail = "Codex CLI 模型目录（遵循原生刷新与缓存策略）".into();
                } else if let (true, Some(exe)) = (name == "openai", exe) {
                    primary = Some(NATIVE_OPENAI.into());
                    let mut args: Vec<String> =
                        ["-c", "model_provider=\"openai\"", "debug", "models"].iter().map(|s| s.to_string()).collect();
                    if file_catalog.is_some() {
                        args.push("--bundled".into());
                    }
                    (models, _) = probes.fetch_models(
                        NATIVE_OPENAI, optional_provider(name), cancel,
                        native_models(exe, &args, sys, Duration::from_secs(8), cancel),
                    ).await?;
                    detail = "Codex CLI 模型目录".into();
                }
                Ok(())
            }
            .await;
            let failed = attempt.is_err() || fallback.is_some();
            let complete = !failed && !models.is_empty();
            if let Err(error) = attempt {
                detail = format!("在线模型目录获取失败：{error}；以下仅为已保存型号");
            } else if let Some(error) = fallback {
                detail = format!("在线模型目录获取失败：{error}；已保留上次目录");
            }
            let inferred_efforts = native.as_ref()
                .map(|metadata| supplement_efforts(&mut models, metadata))
                .unwrap_or_default();
            let mut by_id: Vec<Value> = models;
            for c in saved.iter() {
                let provider = c["model_provider"].as_str().unwrap_or("openai");
                let model = &c["model"];
                let known = by_id.iter().any(|m| m["id"] == *model);
                if provider == name
                    && id_ok(model)
                    && !known
                    && c["model_providers"].get(name) == custom.get(name)
                    && (name != "openai" || c.get("openai_base_url") == config.get("openai_base_url"))
                {
                    let id = model.as_str().unwrap_or_default();
                    by_id.push(json!({"id": id, "label": format!("{id}（本机配置）")}));
                }
            }
            let label = cfg
                .as_ref()
                .and_then(|c| c["name"].as_str())
                .unwrap_or(name)
                .to_string();
            let available = !by_id.is_empty();
            let source = cfg.as_ref().unwrap_or(&Value::Null);
            let identity = crate::common::fingerprint("src-", &[
                source["base_url"].clone(), source["model_catalog_url"].clone(),
                source["env_key"].clone(), source["requires_openai_auth"].clone(),
                source["env_http_headers"].clone(), source["http_headers"].clone(),
                json!(file_catalog.as_ref().filter(|_| name == def)),
            ]);
            let row = json!({"id": name, "sourceIdentity":identity, "label": label, "models": by_id, "detail": detail, "available": available, "catalog_complete":complete, "catalogError":failed});
            (row, (name.clone(), ProviderProbes { primary, inferred_efforts }))
        }
    });
    let (providers, provider_probes): (Vec<Value>, HashMap<_, _>) = join_all(sources).await.into_iter().unzip();
    if cancel.is_cancelled() {
        bail!("已取消");
    }
    let model = config["model"]
        .as_str()
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .or_else(|| {
            providers
                .iter()
                .find(|p| p["id"] == json!(def))
                .and_then(|p| p["models"][0]["id"].as_str().map(str::to_string))
        })
        .unwrap_or_default();
    let catalog = json!({
        "providers": providers,
        "modes": modes("Codex CLI · app-server", exe.is_some()),
        "default": {"provider": def, "model": model, "mode": "cli"},
        "detail": "当前 CODEX_HOME、profile 与原生模型目录",
        "downloads": [
            {"label": "Codex CLI 安装", "url": "https://developers.openai.com/codex/cli/"},
            {"label": "Windows 桌面端", "url": "https://developers.openai.com/codex/app/windows/"}
        ]
    });
    Ok((Discovery {
        exe,
        config,
        profile,
        dir,
        catalog,
    }, RouteProbes(provider_probes)))
}

/// `routing(d, s)` (CXC:174-200): CLI overrides and thread `config`.
pub fn routing(d: &Discovery, provider: &str) -> Result<(Vec<String>, Map<String, Value>)> {
    let cfg = d.config["model_providers"]
        .get(provider)
        .filter(|v| !v.is_null());
    let mut args = vec![
        "-c".to_string(),
        format!("model_provider={}", json!(provider)),
    ];
    let mut settings = Map::new();
    let override_base = (provider == "openai")
        .then(|| d.config["openai_base_url"].as_str())
        .flatten()
        .filter(|b| !b.is_empty());
    let base = override_base.or_else(|| cfg.and_then(|c| c["base_url"].as_str()));
    if let Some(base) = base {
        endpoint(base)?;
        let key = if override_base.is_some() {
            "openai_base_url".to_string()
        } else {
            format!("model_providers.{provider}.base_url")
        };
        args.push("-c".into());
        args.push(format!("{key}={}", json!(base)));
        if d.profile.is_some() && cfg.is_some() {
            args.push("-c".into());
            args.push(format!(
                "model_providers.{provider}.name={}",
                json!(provider)
            ));
        }
    }
    if d.profile.is_some() {
        if let Some(c) = cfg {
            settings.insert("model_providers".into(), json!({provider: c}));
        }
        if let Some(b) = override_base {
            settings.insert("openai_base_url".into(), json!(b));
        }
        if let Some(fc) = d.config["model_catalog_json"].as_str()
            && provider == d.config["model_provider"].as_str().unwrap_or("openai")
        {
            settings.insert(
                "model_catalog_json".into(),
                json!(resolve(&d.dir, fc).to_string_lossy()),
            );
        }
    }
    Ok((args, settings))
}
