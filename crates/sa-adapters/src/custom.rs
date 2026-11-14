//! Operator-managed custom providers (see `PROVIDERS.md`).
//!
//! The desktop keeps a live [`Registry`] loaded from
//! `sa_store::providers::list`. Supporting adapters merge the enabled entries
//! for their agent into `catalog()` as `sa-<id>` rows and, when a route names
//! one, launch the harness against that endpoint. Keys are read from the
//! environment variable the provider names and are handed to the harness only
//! through its environment, never argv, journals or catalog rows.

use crate::common::local_host;
use crate::sys::Sys;
use anyhow::{Result, anyhow, bail};
use futures::{StreamExt, stream};
pub use sa_store::providers::CustomProvider;
use sa_store::providers::{MAX_MODELS, ROUTE_PREFIX, valid_model_id, validate};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use tokio_util::sync::CancellationToken;

/// Live provider list shared by the adapters and the desktop.
pub type Registry = Arc<RwLock<Vec<CustomProvider>>>;

/// Credential sent to a keyless loopback endpoint so the harness does not
/// fall back to its own login. Not a secret.
pub const LOCAL_PLACEHOLDER: &str = "superagent-local";

/// Wire protocol DeepSeek Harness (`llm-pi-ai`) uses for custom providers.
pub const DSH_PROTOCOL: &str = "openai-completions";

/// Codex `wire_api`; `responses` is the only value Codex accepts.
pub const CODEX_WIRE_API: &str = "responses";

pub fn registry(providers: Vec<CustomProvider>) -> Registry {
    Arc::new(RwLock::new(providers))
}

/// Enabled providers for `agent`, in registry order.
pub fn snapshot(registry: &Registry, agent: &str) -> Vec<CustomProvider> {
    let guard = registry.read().unwrap_or_else(|e| e.into_inner());
    guard
        .iter()
        .filter(|p| p.enabled && p.agent == agent)
        .cloned()
        .collect()
}

/// Why a provider cannot run right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyError {
    /// `key_env` is set but the variable is missing or empty.
    Missing(String),
    /// Stored record no longer validates.
    Invalid,
}

/// The key for `p` from `sys`: `Ok(None)` for a keyless loopback endpoint.
pub fn key(p: &CustomProvider, sys: &Sys) -> Result<Option<String>, KeyError> {
    let p = validate(p).map_err(|_| KeyError::Invalid)?;
    if p.key_env.is_empty() {
        return Ok(None);
    }
    sys.var(&p.key_env)
        .map(|k| Some(k.to_string()))
        .ok_or(KeyError::Missing(p.key_env.clone()))
}

/// The key, or the loopback placeholder for keyless endpoints.
fn credential(p: &CustomProvider, sys: &Sys) -> Result<String, KeyError> {
    Ok(key(p, sys)?.unwrap_or_else(|| LOCAL_PLACEHOLDER.to_string()))
}

/// Catalog row (spec §7.2) for one custom provider. Contains no secret: only
/// the variable name appears when it is missing.
pub fn provider_row(p: &CustomProvider, sys: &Sys) -> Value {
    let models: Vec<Value> = p
        .models
        .iter()
        .map(|m| json!({"id": m, "label": m}))
        .collect();
    let (available, detail, reason) = match key(p, sys) {
        Err(KeyError::Invalid) => (
            false,
            "自定义供应商配置无效，请重新编辑".to_string(),
            Some("not_configured"),
        ),
        Err(KeyError::Missing(var)) => (
            false,
            format!("环境变量 {var} 未设置，无法读取密钥"),
            Some("auth"),
        ),
        Ok(_) if p.models.is_empty() => (
            false,
            "未配置模型，请先获取或填写模型".to_string(),
            Some("not_configured"),
        ),
        Ok(None) => (true, "自定义供应商 · 本机端点（无密钥）".to_string(), None),
        Ok(Some(_)) => (
            true,
            format!("自定义供应商 · 密钥来自环境变量 {}", p.key_env),
            None,
        ),
    };
    let mut row = json!({
        "id": p.route_id(),
        "label": format!("{} · 自定义", p.name),
        "available": available,
        "models": models,
        "defaultModel": p.models.first().cloned().unwrap_or_default(),
        "detail": detail,
        "custom": true,
    });
    if let Some(r) = reason {
        row["unavailable_reason"] = json!(r);
    }
    row
}

/// Append custom rows to `catalog.providers`. A row whose id is already
/// listed by the harness's own configuration is skipped, so a native
/// provider is never shadowed.
pub fn merge(catalog: &mut Value, customs: &[CustomProvider], sys: &Sys) {
    if customs.is_empty() {
        return;
    }
    if !catalog["providers"].is_array() {
        catalog["providers"] = json!([]);
    }
    let Some(list) = catalog["providers"].as_array_mut() else {
        return;
    };
    let mut taken: HashSet<String> = list
        .iter()
        .filter_map(|p| p["id"].as_str().map(str::to_string))
        .collect();
    for p in customs {
        let id = p.route_id();
        if taken.insert(id) {
            list.push(provider_row(p, sys));
        }
    }
}

/// The custom provider a selected route names, if the merged catalog row is
/// a custom one.
pub fn lookup<'a>(
    catalog: &Value,
    customs: &'a [CustomProvider],
    provider: &str,
) -> Option<&'a CustomProvider> {
    if !provider.starts_with(ROUTE_PREFIX) {
        return None;
    }
    let is_custom = catalog["providers"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|r| r["id"] == json!(provider) && r["custom"] == json!(true));
    is_custom
        .then(|| customs.iter().find(|p| p.route_id() == provider))
        .flatten()
}

/// Anthropic API root: Claude Code appends `/v1/messages` itself.
pub fn anthropic_root(base: &str) -> String {
    let b = base.trim_end_matches('/');
    b.strip_suffix("/v1").unwrap_or(b).to_string()
}

/// Environment for a Claude Code turn against `p` (docs: code.claude.com
/// env-vars). Inherited Anthropic/cloud-provider variables are removed; the
/// endpoint, bearer token and every model alias are pinned, and
/// `CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST` stops settings files from
/// overriding the endpoint or credentials.
pub fn claude_env(p: &CustomProvider, model: &str, base: &Sys) -> Result<Sys, KeyError> {
    let token = credential(p, base)?;
    let mut env = base.clone();
    env.retain(|k| {
        let u = k.to_ascii_uppercase();
        !(u.starts_with("ANTHROPIC_")
            || u.starts_with("CLAUDE_CODE_OAUTH")
            || matches!(
                u.as_str(),
                "CLAUDE_CODE_USE_BEDROCK"
                    | "CLAUDE_CODE_USE_VERTEX"
                    | "CLAUDE_CODE_USE_FOUNDRY"
                    | "CLAUDE_CODE_SUBAGENT_MODEL"
                    | "SUPERAGENT_CLAUDE_AUTH_TOKEN"
            ))
    });
    env.set("ANTHROPIC_BASE_URL", anthropic_root(&p.base_url));
    env.set("ANTHROPIC_AUTH_TOKEN", token);
    for k in [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "CLAUDE_CODE_SUBAGENT_MODEL",
    ] {
        env.set(k, model);
    }
    env.set("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", "1");
    Ok(env)
}

/// Codex provider definition (`model_providers.<id>`). `env_key` is the
/// variable name; Codex reads the key from its inherited environment.
pub fn codex_provider(p: &CustomProvider) -> Value {
    let mut def = json!({
        "name": p.name,
        "base_url": p.base_url,
        "wire_api": CODEX_WIRE_API,
    });
    if !p.key_env.is_empty() {
        def["env_key"] = json!(p.key_env);
    }
    def
}

/// `-c` overrides selecting `p` for one app-server, plus the thread `config`
/// (sent only when a Codex profile is active, as for native providers).
pub fn codex_routing(p: &CustomProvider, profile: bool) -> (Vec<String>, Map<String, Value>) {
    let id = p.route_id();
    let def = codex_provider(p);
    let mut args = vec!["-c".to_string(), format!("model_provider={}", json!(id))];
    for (k, v) in def.as_object().into_iter().flatten() {
        args.push("-c".into());
        args.push(format!("model_providers.{id}.{k}={v}"));
    }
    let mut settings = Map::new();
    if profile {
        settings.insert("model_providers".into(), json!({ id: def }));
    }
    (args, settings)
}

/// Model-list URL for `p`: Anthropic `/v1/models` for Claude, OpenAI
/// `<base>/models` otherwise.
pub fn models_url(p: &CustomProvider) -> String {
    if p.agent == "claude" {
        format!("{}/v1/models?limit=1000", anthropic_root(&p.base_url))
    } else {
        format!("{}/models", p.base_url.trim_end_matches('/'))
    }
}

fn auth_headers(p: &CustomProvider, key: Option<&str>) -> Vec<(String, String)> {
    let mut h = vec![];
    if p.agent == "claude" {
        h.push(("anthropic-version".to_string(), "2023-06-01".to_string()));
    }
    if let Some(k) = key {
        h.push(("Authorization".to_string(), format!("Bearer {k}")));
        if p.agent == "claude" {
            h.push(("x-api-key".to_string(), k.to_string()));
        }
    }
    h
}

/// Model ids from an OpenAI (`data[].id`), Anthropic (`data[].id`) or Codex
/// catalog (`models[].slug`) response: valid, unique, at most `MAX_MODELS`.
pub fn parse_models(body: &Value) -> Vec<String> {
    let rows = body["data"]
        .as_array()
        .or_else(|| body["models"].as_array());
    let mut seen = HashSet::new();
    rows.into_iter()
        .flatten()
        .filter_map(|m| {
            m["id"]
                .as_str()
                .or_else(|| m["slug"].as_str())
                .or_else(|| m.as_str())
        })
        .filter(|id| valid_model_id(id) && seen.insert(id.to_string()))
        .map(str::to_string)
        .collect()
}

/// "获取模型": list the endpoint's models (10 s, 4 MiB). Makes one GET with
/// the provider's key; never follows redirects or environment proxies.
pub async fn probe_models(
    provider: &CustomProvider,
    sys: &Sys,
    cancel: &CancellationToken,
) -> Result<Vec<String>> {
    Ok(probe_catalog(provider, sys, cancel)
        .await?
        .iter()
        .filter_map(|m| m["id"].as_str().map(str::to_string))
        .collect())
}

async fn probe_catalog(
    provider: &CustomProvider,
    sys: &Sys,
    cancel: &CancellationToken,
) -> Result<Vec<Value>> {
    let p = validate(provider)?;
    let key = match key(&p, sys) {
        Ok(k) => k,
        Err(KeyError::Missing(var)) => bail!("环境变量 {var} 未设置，无法读取密钥"),
        Err(KeyError::Invalid) => bail!("自定义供应商配置无效"),
    };
    let url = url::Url::parse(&p.base_url).map_err(|_| anyhow!("供应商端点无效"))?;
    let key = key.or_else(|| {
        local_host(url.host_str().unwrap_or_default()).then(|| LOCAL_PLACEHOLDER.to_string())
    });
    let models = crate::catalog::fetch(&models_url(&p), &auth_headers(&p, key.as_deref()), cancel)
        .await
        .map_err(|e| anyhow!("获取模型失败：{e}"))?;
    if models.len() > MAX_MODELS {
        bail!("模型数量超过 {MAX_MODELS}，无法保存完整目录");
    }
    Ok(models)
}

/// Refresh custom providers at the same time as native catalogs, retaining exact
/// native capability metadata when an endpoint only returns model IDs.
pub async fn refresh(
    catalog: &mut Value,
    customs: &[CustomProvider],
    sys: &Sys,
    cancel: &CancellationToken,
) {
    let metadata: Vec<Value> = catalog["providers"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|p| p["models"].as_array().into_iter().flatten().cloned())
        .collect();
    merge(catalog, customs, sys);
    let probes: Vec<_> = customs
        .iter()
        .filter(|p| {
            catalog["providers"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|row| row["id"] == p.route_id() && row["custom"] == true)
        })
        .map(|p| async move { (p, probe_catalog(p, sys, cancel).await) })
        .collect();
    let results = stream::iter(probes).buffered(4).collect::<Vec<_>>().await;
    for (p, result) in results {
        let Some(row) = catalog["providers"].as_array_mut().and_then(|ps| {
            ps.iter_mut()
                .find(|r| r["id"] == p.route_id() && r["custom"] == true)
        }) else {
            continue;
        };
        match result {
            Ok(mut models) => {
                crate::catalog::supplement(&mut models, &metadata);
                if p.agent == "claude" {
                    crate::catalog::restrict_efforts(
                        &mut models,
                        &["low", "medium", "high", "xhigh", "max"],
                    );
                } else if p.agent == "deepseek" {
                    crate::catalog::restrict_efforts(
                        &mut models,
                        &["off", "minimal", "low", "medium", "high", "xhigh", "max"],
                    );
                }
                row["models"] = json!(models);
                row["available"] = json!(true);
                row["detail"] = json!("供应商在线完整模型目录");
                row["catalog_complete"] = json!(true);
            }
            Err(error) => {
                row["detail"] = json!(format!("在线目录获取失败：{error}；以下仅为已保存型号"));
                row["catalogError"] = json!(true);
                row["catalog_complete"] = json!(false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    pub(crate) fn sys(vars: &[(&str, &str)]) -> Sys {
        Sys::new(
            vars.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            PathBuf::from("/nonexistent-home"),
            std::env::temp_dir(),
        )
    }

    pub(crate) fn provider(agent: &str, base: &str, key_env: &str) -> CustomProvider {
        CustomProvider {
            id: "gw1".into(),
            agent: agent.into(),
            name: "网关".into(),
            base_url: base.into(),
            key_env: key_env.into(),
            models: vec!["m-a".into(), "m-b".into()],
            enabled: true,
        }
    }

    /// Neither origin responds until both requests have arrived. A sequential
    /// refresh therefore times out instead of passing through a timing threshold.
    async fn paired_catalogs(
        responses: [(u16, Value); 2],
    ) -> ([String; 2], tokio::task::JoinHandle<[String; 2]>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

        async fn receive(listener: TcpListener) -> (TcpStream, String) {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                assert_eq!(socket.read(&mut byte).await.unwrap(), 1);
                request.push(byte[0]);
                assert!(request.len() < 16 * 1024);
            }
            (socket, String::from_utf8(request).unwrap())
        }

        let first = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let second = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bases = [
            format!("http://{}", first.local_addr().unwrap()),
            format!("http://{}", second.local_addr().unwrap()),
        ];
        let server = tokio::spawn(async move {
            let ((first, first_request), (second, second_request)) =
                tokio::join!(receive(first), receive(second));
            // Respond in reverse registry order to exercise result association.
            for (mut socket, (status, body)) in [
                (second, responses[1].clone()),
                (first, responses[0].clone()),
            ] {
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            [first_request, second_request]
        });
        (bases, server)
    }

    #[tokio::test]
    async fn refresh_probes_concurrently_preserving_order_auth_and_capabilities() {
        let (bases, server) = paired_catalogs([
            (
                200,
                json!({"data": [{"id": "shared", "reasoningEfforts": []}]}),
            ),
            (200, json!({"data": [{"id": "shared"}]})),
        ])
        .await;
        let first = provider("claude", &bases[0], "FIRST_KEY");
        let mut second = provider("deepseek", &bases[1], "SECOND_KEY");
        second.id = "gw2".into();
        let customs = [first, second];
        let mut catalog = json!({"providers": [{
            "id": "native", "models": [{
                "id": "shared", "reasoningEfforts": ["high", "unsupported"],
                "defaultReasoningEffort": "unsupported"
            }]
        }]});
        let sys = sys(&[
            ("FIRST_KEY", "first-fixture"),
            ("SECOND_KEY", "second-fixture"),
        ]);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            refresh(&mut catalog, &customs, &sys, &CancellationToken::new()),
        )
        .await
        .expect("both catalog requests must arrive before either response");
        let requests = server.await.unwrap();
        assert!(requests[0].starts_with("GET /v1/models?limit=1000 "));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("authorization: bearer first-fixture")
        );
        assert!(requests[0].contains("x-api-key: first-fixture"));
        assert!(!requests[0].contains("second-fixture"));
        assert!(requests[1].starts_with("GET /models "));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("authorization: bearer second-fixture")
        );
        assert!(!requests[1].contains("first-fixture"));
        let rows = catalog["providers"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1]["id"], "sa-gw1");
        assert_eq!(rows[2]["id"], "sa-gw2");
        assert_eq!(rows[1]["models"][0]["reasoningEfforts"], json!([]));
        assert_eq!(rows[2]["models"][0]["reasoningEfforts"], json!(["high"]));
        assert!(rows[2]["models"][0].get("defaultReasoningEffort").is_none());
        for row in &rows[1..] {
            assert_eq!(row["available"], true);
            assert_eq!(row["catalog_complete"], true);
            assert_eq!(row["detail"], "供应商在线完整模型目录");
        }
    }

    #[tokio::test]
    async fn refresh_keeps_failed_origin_saved_models_separate_from_success() {
        let (bases, server) = paired_catalogs([
            (401, json!({"error": "fixture auth failure"})),
            (200, json!({"data": [{"id": "online-success"}]})),
        ])
        .await;
        let mut first = provider("codex", &bases[0], "FIRST_KEY");
        first.models = vec!["saved-failed".into()];
        let mut second = provider("codex", &bases[1], "SECOND_KEY");
        second.id = "gw2".into();
        second.models = vec!["saved-success".into()];
        let customs = [first, second];
        let mut catalog = json!({"providers": []});
        let sys = sys(&[
            ("FIRST_KEY", "first-fixture"),
            ("SECOND_KEY", "second-fixture"),
        ]);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            refresh(&mut catalog, &customs, &sys, &CancellationToken::new()),
        )
        .await
        .expect("both catalog requests must arrive before either response");
        let requests = server.await.unwrap();
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("authorization: bearer first-fixture")
        );
        assert!(!requests[0].contains("second-fixture"));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("authorization: bearer second-fixture")
        );
        assert!(!requests[1].contains("first-fixture"));
        let rows = catalog["providers"].as_array().unwrap();
        assert_eq!(rows[0]["id"], "sa-gw1");
        assert_eq!(
            rows[0]["models"],
            json!([{"id": "saved-failed", "label": "saved-failed"}])
        );
        assert_eq!(rows[0]["defaultModel"], "saved-failed");
        assert_eq!(rows[0]["catalog_complete"], false);
        assert_eq!(rows[0]["catalogError"], true);
        assert!(rows[0]["detail"].as_str().unwrap().contains("已保存型号"));
        assert_eq!(rows[1]["id"], "sa-gw2");
        assert_eq!(rows[1]["models"][0]["id"], "online-success");
        assert_eq!(rows[1]["catalog_complete"], true);
        assert!(rows[1].get("catalogError").is_none());
        assert!(!catalog.to_string().contains("first-fixture"));
        assert!(!catalog.to_string().contains("second-fixture"));
    }

    #[test]
    fn rows_report_availability_without_secrets() {
        let p = provider("codex", "https://gw.example/v1", "GW_KEY");
        let ok = provider_row(&p, &sys(&[("GW_KEY", "sk-secret")]));
        assert_eq!(ok["id"], "sa-gw1");
        assert_eq!(ok["available"], true);
        assert_eq!(ok["models"][1]["id"], "m-b");
        assert!(!ok.to_string().contains("sk-secret"));

        let missing = provider_row(&p, &sys(&[("GW_KEY", "")]));
        assert_eq!(missing["available"], false);
        assert_eq!(missing["unavailable_reason"], "auth");
        assert!(missing["detail"].as_str().unwrap().contains("GW_KEY"));

        let mut empty = p.clone();
        empty.models.clear();
        assert_eq!(
            provider_row(&empty, &sys(&[("GW_KEY", "k")]))["unavailable_reason"],
            "not_configured"
        );

        let keyless = provider("claude", "http://127.0.0.1:9/", "");
        assert_eq!(provider_row(&keyless, &sys(&[]))["available"], true);
        let mut broken = keyless.clone();
        broken.base_url = "http://remote.example".into();
        assert_eq!(provider_row(&broken, &sys(&[]))["available"], false);
    }

    #[test]
    fn merged_routes_round_trip_through_select() {
        use crate::common::{modes, select};
        use sa_engine::adapter::{ErrorClass, Route};
        let mut cat = json!({
            "providers": [{"id": "openai", "models": [{"id": "gpt"}]}, {"id": "sa-taken", "models": [{"id": "x"}]}],
            "modes": modes("Codex", true),
            "default": {"provider": "openai", "model": "gpt", "mode": "cli"},
        });
        let up = provider("codex", "https://gw.example/v1", "GW_KEY");
        let mut down = provider("codex", "https://down.example/v1", "NOPE");
        down.id = "down".into();
        let mut clash = provider("codex", "https://c.example/v1", "GW_KEY");
        clash.id = "taken".into();
        let customs = vec![up.clone(), down.clone(), clash.clone()];
        let env = sys(&[("GW_KEY", "k")]);
        merge(&mut cat, &customs, &env);
        let ids: Vec<&str> = cat["providers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["openai", "sa-taken", "sa-gw1", "sa-down"]);

        let route = |p: &str, m: &str| Route {
            provider: p.into(),
            model: m.into(),
            mode: "cli".into(),
            reasoning_effort: None,
        };
        assert_eq!(
            select(&cat, &route("sa-gw1", "m-b")).unwrap(),
            route("sa-gw1", "m-b")
        );
        assert_eq!(lookup(&cat, &customs, "sa-gw1"), Some(&up));
        assert_eq!(lookup(&cat, &customs, "sa-taken"), None, "native row wins");
        assert_eq!(lookup(&cat, &customs, "openai"), None);
        for r in [route("sa-down", "m-a"), route("sa-gw1", "other")] {
            assert_eq!(
                select(&cat, &r).unwrap_err().class,
                ErrorClass::RouteUnavailable
            );
        }
    }

    #[test]
    fn claude_env_pins_endpoint_token_and_models() {
        let p = provider("claude", "https://gw.example/v1/", "GW_KEY");
        let base = sys(&[
            ("GW_KEY", "sk-1"),
            ("ANTHROPIC_API_KEY", "other"),
            ("anthropic_base_url", "https://elsewhere"),
            ("CLAUDE_CODE_USE_BEDROCK", "1"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "o"),
            ("PATH", "/bin"),
        ]);
        let env = claude_env(&p, "m-a", &base).unwrap();
        assert_eq!(env.get("ANTHROPIC_BASE_URL"), Some("https://gw.example"));
        assert_eq!(env.get("ANTHROPIC_AUTH_TOKEN"), Some("sk-1"));
        assert_eq!(env.get("ANTHROPIC_API_KEY"), None);
        assert_eq!(env.get("CLAUDE_CODE_USE_BEDROCK"), None);
        assert_eq!(env.get("CLAUDE_CODE_OAUTH_TOKEN"), None);
        assert_eq!(env.get("ANTHROPIC_DEFAULT_HAIKU_MODEL"), Some("m-a"));
        assert_eq!(env.get("CLAUDE_CODE_SUBAGENT_MODEL"), Some("m-a"));
        assert_eq!(env.get("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST"), Some("1"));
        assert_eq!(env.get("PATH"), Some("/bin"));
        let anthropic: Vec<_> = env
            .vars()
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("ANTHROPIC_BASE_URL"))
            .collect();
        assert_eq!(anthropic.len(), 1);

        assert_eq!(
            claude_env(&p, "m-a", &sys(&[])).unwrap_err(),
            KeyError::Missing("GW_KEY".into())
        );
        let local = provider("claude", "http://localhost:8080", "");
        let env = claude_env(&local, "m", &sys(&[])).unwrap();
        assert_eq!(env.get("ANTHROPIC_AUTH_TOKEN"), Some(LOCAL_PLACEHOLDER));
        assert_eq!(env.get("ANTHROPIC_BASE_URL"), Some("http://localhost:8080"));
    }

    #[test]
    fn codex_overrides_name_the_key_variable_only() {
        let p = provider("codex", "https://gw.example/v1", "GW_KEY");
        let (args, settings) = codex_routing(&p, false);
        assert_eq!(
            args,
            vec![
                "-c",
                "model_provider=\"sa-gw1\"",
                "-c",
                "model_providers.sa-gw1.name=\"网关\"",
                "-c",
                "model_providers.sa-gw1.base_url=\"https://gw.example/v1\"",
                "-c",
                "model_providers.sa-gw1.wire_api=\"responses\"",
                "-c",
                "model_providers.sa-gw1.env_key=\"GW_KEY\"",
            ]
        );
        assert!(settings.is_empty());
        for a in args.iter().filter(|a| *a != "-c") {
            let (_, v) = a.split_once('=').unwrap();
            toml::from_str::<toml::Table>(&format!("v = {v}")).expect("TOML value");
        }
        let (args, settings) = codex_routing(&provider("codex", "http://127.0.0.1:1/v1", ""), true);
        assert!(!args.iter().any(|a| a.contains("env_key")));
        assert_eq!(
            settings["model_providers"]["sa-gw1"]["wire_api"],
            "responses"
        );
    }

    #[test]
    fn model_lists_parse_all_shapes() {
        assert_eq!(
            parse_models(
                &json!({"data": [{"id": "a"}, {"id": "a"}, {"id": "bad id"}, {"id": "b[1m]"}]})
            ),
            vec!["a", "b[1m]"]
        );
        assert_eq!(
            parse_models(&json!({"models": [{"slug": "gpt-x"}]})),
            vec!["gpt-x"]
        );
        assert!(parse_models(&json!({"error": "x"})).is_empty());
        assert_eq!(
            models_url(&provider("claude", "https://gw.example/v1", "K")),
            "https://gw.example/v1/models?limit=1000"
        );
        assert_eq!(
            models_url(&provider("codex", "https://gw.example/v1", "K")),
            "https://gw.example/v1/models"
        );
    }
}
