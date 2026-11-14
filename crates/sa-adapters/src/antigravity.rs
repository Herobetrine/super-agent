//! Antigravity CLI adapter (port of `runtime/adapters/antigravity.cjs`,
//! spec §7.3).
//!
//! `agy --input-format stream-json --output-format stream-json`: one user
//! record on stdin, `step_update` frames on stdout and a final `result`.
//! Reader turns run under a private HOME whose settings deny writes,
//! commands, MCP and URL access.

use crate::common::{
    Emit, cc_rows, endpoint, fingerprint, modes, probe_failed, read_json_or, route_gone, scope,
    select, session_mismatch,
};
use crate::proc::{capture, start, stream};
use crate::sys::Sys;
use futures::future::BoxFuture;
use futures::stream::{self, StreamExt};
use regex::Regex;
use sa_engine::adapter::{
    Adapter, AdapterError, Catalog, ErrorClass, EventSink, Route, RunOutcome, RunRequest, TurnHost,
};
use sa_engine::resilience::classify::{classified, classify_text, from_http_status};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

static MODEL_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\w./-]{1,150}$").expect("valid regex"));

pub(crate) const SETTINGS_REL: &str = ".gemini/antigravity-cli/settings.json";
const FAILED: &str = "Antigravity 未返回匹配会话的成功终态";

pub struct Antigravity {
    sys: Sys,
}

impl Antigravity {
    pub fn new(sys: Sys) -> Self {
        Self { sys }
    }
}

pub fn executable(sys: &Sys) -> Option<PathBuf> {
    sys.which(if cfg!(windows) { "agy.exe" } else { "agy" })
}

fn native_settings(sys: &Sys) -> Value {
    read_json_or(&sys.home.join(SETTINGS_REL))
}

fn adc(sys: &Sys) -> bool {
    sys.get("AGY_ADC_AUTH")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

/// A configured Gemini API route (AG:22-44).
#[derive(Clone, Debug)]
struct Row {
    id: String,
    base: String,
    token: Option<String>,
    model: Option<String>,
    public: Value,
}

fn row(id: &str, label: &str, base: &str, token: Option<&str>, model: Option<&str>) -> Row {
    let key = fingerprint("agy-", &[json!(id), json!(base)]);
    let safe = base.is_empty() || endpoint(base).is_ok();
    let token = token.filter(|t| !t.is_empty());
    let model = model.filter(|m| !m.is_empty());
    let available = token.is_some() && safe && model.is_some();
    Row {
        id: key.clone(),
        base: base.into(),
        token: token.map(str::to_string),
        model: model.map(str::to_string),
        public: json!({
            "id": key,
            "label": label,
            "models": model.map(|m| vec![json!({"id": m, "label": m})]).unwrap_or_default(),
            "defaultModel": model.unwrap_or_default(),
            "available": available,
            "detail": if available { "本机保存的成对认证与端点" } else { "缺少配对凭据、端点或模型" },
        }),
    }
}

/// `configured()` (AG:45-72).
fn configured(sys: &Sys) -> Vec<Row> {
    let mut records = vec![];
    let cfg = native_settings(sys);
    if cfg["modelProvider"] == json!("gemini") {
        let model = cfg["model"].as_str().or_else(|| sys.get("GEMINI_MODEL"));
        records.push(row(
            "native",
            "Gemini API · 当前配置",
            sys.get("GOOGLE_GEMINI_BASE_URL").unwrap_or_default(),
            sys.get("GEMINI_API_KEY"),
            model,
        ));
    }
    for rec in cc_rows(sys, &["agy"]).unwrap_or_default() {
        let id = rec.get("id").and_then(Value::as_str).unwrap_or_default();
        let name = rec.get("name").and_then(Value::as_str).unwrap_or_default();
        let parsed = rec
            .get("settings_config")
            .and_then(Value::as_str)
            .and_then(|s| serde_json::from_str::<Value>(s).ok());
        match parsed {
            Some(c) => {
                let env = &c["env"];
                let model = c["config"]["model"]["name"]
                    .as_str()
                    .or_else(|| env["GEMINI_MODEL"].as_str());
                records.push(row(
                    id,
                    name,
                    env["GOOGLE_GEMINI_BASE_URL"].as_str().unwrap_or_default(),
                    env["GEMINI_API_KEY"].as_str(),
                    model,
                ));
            }
            None => records.push(row(id, name, "", None, None)),
        }
    }
    records
}

/// Write `content` to `target` atomically with a fresh temp file.
fn write_atomic(target: &Path, content: &str) -> std::io::Result<()> {
    let dir = target.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(content.as_bytes())?;
    tmp.persist(target).map_err(|e| e.error)?;
    Ok(())
}

/// `environment(provider, route, reader)` (AG:73-104).
fn environment(
    sys: &Sys,
    provider: &str,
    route: Option<&Row>,
    reader: bool,
) -> Result<(Sys, Value), AdapterError> {
    let mut env = sys.clone();
    let mut overrides = serde_json::Map::new();
    overrides.insert("modelProvider".into(), json!(""));
    for key in ["GEMINI_API_KEY", "GOOGLE_GEMINI_BASE_URL", "AGY_ADC_AUTH"] {
        env.remove(key);
    }
    if let Some(r) = route {
        if r.public["available"] != json!(true) || provider != r.id {
            return Err(route_gone("Antigravity 路由不可用"));
        }
        env.set("GEMINI_API_KEY", r.token.clone().unwrap_or_default());
        if !r.base.is_empty() {
            env.set("GOOGLE_GEMINI_BASE_URL", r.base.clone());
        }
        overrides.insert("modelProvider".into(), json!("gemini"));
        overrides.insert("model".into(), json!(r.model));
    } else if provider == "adc" && adc(sys) {
        env.set("AGY_ADC_AUTH", "true");
    } else if provider != "google" {
        return Err(route_gone("Antigravity 供应商失效"));
    }
    if reader {
        overrides.insert("toolPermission".into(), json!("strict"));
        overrides.insert("disableSlashCommands".into(), json!(true));
        overrides.insert("agentMode".into(), json!(""));
        let name = if provider == "google" {
            "agy-reader".to_string()
        } else {
            format!("agy-reader-{provider}")
        };
        let dir = sys.platform_dir().join(name);
        let target = dir.join(SETTINGS_REL);
        overrides.insert(
            "permissions".into(),
            json!({"deny": ["write_file(*)", "command(*)", "mcp(*)", "execute_url(*)", "read_url(*)"]}),
        );
        let dir_text = dir.to_string_lossy().into_owned();
        env.set("HOME", dir_text.clone());
        env.set("USERPROFILE", dir_text);
        let wanted = Value::Object(overrides.clone());
        if read_json_or(&target) != wanted {
            write_atomic(&target, &wanted.to_string())
                .map_err(|_| classified(ErrorClass::Other, "agy:reader_settings"))?;
        }
    }
    Ok((env, Value::Object(overrides)))
}

struct Discovery {
    exe: Option<PathBuf>,
    rows: Vec<Row>,
    catalog: Value,
}

fn native_models(output: &str) -> Result<Vec<Value>, String> {
    let models: Vec<Value> = output
        .lines()
        .filter_map(|line| {
            let (id, label) = line.split_once('\t')?;
            (MODEL_ID.is_match(id) && !label.trim().is_empty())
                .then(|| json!({"id":id,"label":label.trim()}))
        })
        .collect();
    if models.is_empty() {
        Err("agy models 未返回可识别的模型列表，仅保留原生默认".into())
    } else {
        Ok(models)
    }
}

/// `discover(signal)` (AG:105-150).
async fn discover(
    sys: &Sys,
    cancel: &CancellationToken,
    route: Option<&Route>,
) -> Result<Discovery, AdapterError> {
    let exe = executable(sys);
    let settings = native_settings(sys);
    let mut rows = configured(sys);
    let mut ids = vec!["google"];
    if adc(sys) {
        ids.push("adc");
    }
    // Execution admission refreshes only its selected source. The model picker
    // still obtains every source and every model's effort capabilities.
    if let Some(route) = route {
        ids.retain(|id| *id == route.provider);
        rows.retain(|row| row.id == route.provider);
    }
    let mut providers: Vec<Value> = vec![];
    for id in ids {
        let mut models: Vec<Value> = vec![];
        let mut catalog_error = None;
        if let Some(cli) = &exe {
            let (env, overrides) = environment(sys, id, None, false)?;
            match capture(
                cli,
                &[
                    "--settings".into(),
                    overrides.to_string(),
                    "models".to_string(),
                ],
                &sys.temp,
                &env,
                "",
                Duration::from_secs(15),
                cancel,
            )
            .await
            {
                Ok(out) => {
                    match native_models(&out) {
                        Ok(found) => models = found,
                        Err(error) => catalog_error = Some(error),
                    }
                    models.insert(
                        0,
                        json!({"id": "@native", "label": "跟随 agy 原生默认模型"}),
                    );
                    models = stream::iter(models.into_iter().map(|mut model| {
                        let (env, overrides) = (&env, &overrides);
                        async move {
                            if route.is_some_and(|route| {
                                route.reasoning_effort.is_none() || model["id"] != route.model
                            }) {
                                return model;
                            }
                            let mut args = vec![
                                "--settings".into(),
                                overrides.to_string(),
                                "-p".into(),
                                "/effort".into(),
                                "--output-format".into(),
                                "json".into(),
                            ];
                            if model["id"] != "@native" {
                                args.extend([
                                    "--model".into(),
                                    model["id"].as_str().unwrap_or_default().to_string(),
                                ]);
                            }
                            if let Ok(out) = capture(
                                cli,
                                &args,
                                &sys.temp,
                                env,
                                "",
                                Duration::from_secs(15),
                                cancel,
                            )
                            .await
                                && let Ok(info) = serde_json::from_str::<Value>(&out)
                            {
                                let data = &info["command"]["data"];
                                if data["adjustable"] == true {
                                    model["reasoningEfforts"] = data["available"].clone();
                                    model["defaultReasoningEffort"] = data["current"].clone();
                                }
                            }
                            model
                        }
                    }))
                    .buffered(4)
                    .collect()
                    .await;
                }
                Err(e) if e.class == ErrorClass::Cancelled => return Err(e),
                Err(error) => {
                    catalog_error = Some(format!("原生模型目录读取失败：{}", error.message))
                }
            }
        }
        providers.push(json!({
            "id": id,
            "label": if id == "adc" { "Google Cloud ADC" } else { "Google · Antigravity 登录账号" },
            "available": !models.is_empty(),
            "catalog_complete": catalog_error.is_none() && !models.is_empty(),
            "detail": catalog_error.as_deref().unwrap_or("agy 原生在线模型目录"),
            "models": models,
        }));
    }
    for row in &mut rows {
        let Some(token) = &row.token else { continue };
        let base = if row.base.is_empty() {
            "https://generativelanguage.googleapis.com"
        } else {
            row.base.trim_end_matches('/')
        };
        let target = if base.ends_with("/v1beta") || base.ends_with("/v1") {
            format!("{base}/models?pageSize=1000")
        } else {
            format!("{base}/v1beta/models?pageSize=1000")
        };
        match crate::catalog::fetch(&target, &[("x-goog-api-key".into(), token.clone())], cancel)
            .await
        {
            Ok(mut models) => {
                models.retain(|m| {
                    m["supportedGenerationMethods"]
                        .as_array()
                        .is_none_or(|methods| methods.iter().any(|s| s == "generateContent"))
                });
                row.public["available"] = json!(!models.is_empty());
                row.public["models"] = json!(models);
                row.public["detail"] = json!("Gemini API 在线模型目录");
                row.public["catalog_complete"] = json!(true);
            }
            Err(error) => {
                row.public["catalogError"] = json!(true);
                row.public["catalog_complete"] = json!(false);
                row.public["detail"] =
                    json!(format!("在线目录获取失败：{error}；以下仅为已保存型号"));
            }
        }
    }
    providers.extend(rows.iter().map(|r| r.public.clone()));
    let selected = if settings["modelProvider"] == json!("gemini") {
        rows.first().map(|r| r.public.clone())
    } else {
        providers
            .iter()
            .find(|p| p["id"] == json!("adc"))
            .or(providers.first())
            .cloned()
    };
    let configured_model = sys
        .var("SUPERAGENT_AGY_MODEL")
        .map(str::to_string)
        .or_else(|| settings["model"].as_str().map(str::to_string));
    let model = selected
        .as_ref()
        .and_then(|s| {
            let models = s["models"].as_array()?;
            let hit = configured_model.as_deref().and_then(|c| {
                models
                    .iter()
                    .find(|m| m["id"] == json!(c) || m["label"] == json!(c))
            });
            hit.and_then(|m| m["id"].as_str().map(str::to_string))
                .or_else(|| {
                    s["defaultModel"]
                        .as_str()
                        .filter(|d| !d.is_empty())
                        .map(str::to_string)
                })
                .or_else(|| {
                    models
                        .iter()
                        .any(|m| m["id"] == json!("@native"))
                        .then(|| "@native".to_string())
                })
        })
        .unwrap_or_default();
    let catalog = json!({
        "providers": providers,
        "modes": modes("Antigravity CLI", exe.is_some()),
        "default": {
            "mode": "cli",
            "provider": selected.as_ref().and_then(|s| s["id"].as_str()).unwrap_or_default(),
            "model": model,
        },
        "detail": "Google 登录或保存的 Gemini 路由；目录按次读取。",
        "downloads": [
            {"label": "Antigravity CLI 安装", "url": "https://antigravity.google/docs/cli/install"},
            {"label": "Antigravity 桌面版", "url": "https://antigravity.google/download"},
        ],
    });
    Ok(Discovery { exe, rows, catalog })
}

impl Adapter for Antigravity {
    fn agent(&self) -> &'static str {
        "antigravity"
    }

    fn catalog(&self) -> BoxFuture<'static, anyhow::Result<Catalog>> {
        let sys = self.sys.clone();
        Box::pin(async move {
            let d = discover(&sys, &CancellationToken::new(), None)
                .await
                .map_err(|e| anyhow::anyhow!(e.message))?;
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
        Box::pin(async move { run(sys, request, host, Emit(events), cancel).await })
    }
}

/// Tool steps carry no id in the documented frames; key them by the
/// step's own id when present, else by tool name.
fn tool_key(step: &Value) -> Option<String> {
    ["step_id", "id", "tool_call_id"]
        .iter()
        .find_map(|k| step[*k].as_str().map(str::to_string))
        .or_else(|| step["tool_name"].as_str().map(|n| format!("tool:{n}")))
}

/// `run(o)` (AG:151-205).
async fn run(
    sys: Sys,
    req: RunRequest,
    host: Arc<dyn TurnHost>,
    emit: Emit,
    cancel: CancellationToken,
) -> Result<RunOutcome, AdapterError> {
    crate::images::check(&req)?;
    let boundary = scope("antigravity", &req, &host, &cancel, false).await?;
    let d = discover(&sys, &cancel, Some(&req.route))
        .await
        .map_err(|e| {
            if e.class == ErrorClass::Cancelled {
                e
            } else {
                probe_failed(&e.message)
            }
        })?;
    let s = select(&d.catalog, &req.route)?;
    let high = boundary == "danger-full-access";
    if !req.images.is_empty() {
        image_provider(&s.provider)?;
        if high {
            image_account_settings(&native_settings(&sys))?;
        }
    }
    let r = d.rows.iter().find(|v| v.id == s.provider);
    let (env, overrides) = environment(&sys, &s.provider, r, !high)?;
    let exe = d
        .exe
        .clone()
        .ok_or_else(|| route_gone("本机未安装所选原生执行端"))?;
    if !req.images.is_empty() {
        // ACP reads permissions from its actual settings file. Root
        // --settings overrides are not propagated into the ACP server.
        // Reuse the same persistent root as text turns for session continuity.
        let gemini = if high {
            sys.home.join(".gemini")
        } else {
            PathBuf::from(env.get("HOME").unwrap_or_default()).join(".gemini")
        };
        let mut args = vec![
            "acp".into(),
            "--gemini-dir".into(),
            gemini.to_string_lossy().into_owned(),
        ];
        if high {
            args.push("--dangerously-skip-permissions".into());
        }
        let server = crate::acp::Server {
            id: s.provider.clone(),
            label: "Antigravity CLI".into(),
            command: exe.to_string_lossy().into_owned(),
            args,
            env: vec![],
            models: vec![s.model.clone()],
            model_config_id: None,
            permission_modes: ["readonly", "ask", "partial", "legacy"]
                .into_iter()
                .map(|profile| (profile.into(), "default".into()))
                .collect(),
            trusted_sandbox: true,
        };
        return crate::acp::run_server(env, req, host, emit, cancel, server, true).await;
    }
    let mut args: Vec<String> = [
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
    ]
    .map(String::from)
    .to_vec();
    if let Some(sid) = &req.session_id {
        args.extend(["--conversation".to_string(), sid.clone()]);
    }
    if s.model != "@native" {
        args.extend(["--model".to_string(), s.model.clone()]);
    }
    if let Some(effort) = &s.reasoning_effort {
        args.extend(["--effort".into(), effort.clone()]);
    }
    if high {
        args.extend([
            "--dangerously-skip-permissions".to_string(),
            "--settings".to_string(),
            overrides.to_string(),
        ]);
    } else {
        args.push("--disable-slash-commands".into());
    }
    emit.journal(
        "adapter.permission",
        json!({"profile": req.profile, "scope": boundary}),
    );
    let input = format!(
        "{}\n",
        json!({"event": "user", "message": {"content": req.prompt}})
    );
    let mut proc = start(&exe, &args, &req.cwd, &env)?;
    let mut last: Option<Value> = None;
    let mut session = req.session_id.clone();
    let mut open_tools: HashSet<String> = HashSet::new();
    let streamed = stream(&mut proc, &input, &cancel, |e| {
        emit.journal("native", e.clone());
        if e["event"] == json!("init")
            && let Some(id) = e["conversation_id"].as_str()
        {
            emit.session(id);
            session = Some(id.to_string());
        }
        if e["event"] == json!("step_update") {
            let step = &e["step_update"];
            match step["step_type"].as_str() {
                Some("agent_response") => {
                    if let Some(t) = step["text_delta"].as_str()
                        && !t.is_empty()
                    {
                        emit.journal("delta", json!(t));
                    }
                    if step["state"] == json!("DONE") {
                        emit.journal("message.boundary", Value::Null);
                    }
                }
                Some("tool") => {
                    if let Some(key) = tool_key(step) {
                        match step["state"].as_str() {
                            Some("ACTIVE") if open_tools.insert(key.clone()) => {
                                emit.tool_started(&key)
                            }
                            Some("DONE" | "ERROR" | "FAILED") if open_tools.remove(&key) => {
                                emit.tool_finished(&key)
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        if e["event"] == json!("result") {
            last = Some(e["result"].clone());
        }
        Ok(())
    })
    .await;
    if let Err(mut err) = streamed {
        // A nonzero exit may accompany an upstream ERROR terminal. Preserve
        // that terminal's provider classification instead of its exit code.
        if err.class == ErrorClass::Cancelled
            || last
                .as_ref()
                .is_none_or(|result| result["status"] != "ERROR")
        {
            err.session_id = err.session_id.or(session);
            return Err(err);
        }
    }
    let last = last.unwrap_or(Value::Null);
    let conversation = last["conversation_id"].as_str().filter(|c| !c.is_empty());
    if let (Some(want), Some(got)) = (req.session_id.as_deref(), conversation)
        && want != got
    {
        return Err(session_mismatch(FAILED));
    }
    let ok = last["status"] == json!("SUCCESS")
        && last["response"].is_string()
        && conversation.is_some();
    if !ok {
        let mut err = if last.is_null() {
            classified(ErrorClass::ProcessCrashed, "agy:no_terminal")
        } else {
            native_failure(last["error"].as_str().unwrap_or_default())
        };
        if matches!(err.class, ErrorClass::Other | ErrorClass::ProcessCrashed) {
            err.message = FAILED.into();
        }
        err.session_id = conversation.map(str::to_string).or(session);
        return Err(err);
    }
    Ok(RunOutcome {
        text: last["response"].as_str().unwrap_or_default().to_string(),
        session_id: conversation.map(str::to_string),
    })
}

fn image_provider(provider: &str) -> Result<(), AdapterError> {
    if provider != "google" {
        return Err(AdapterError::new(
            ErrorClass::Protocol,
            "Antigravity 原生图片输入目前支持 Google 登录账号；所选 API 或 ADC 来源的 ACP 认证与会话尚未接入",
        ));
    }
    Ok(())
}

fn image_account_settings(settings: &Value) -> Result<(), AdapterError> {
    let custom_model_provider = settings["modelProvider"]
        .as_str()
        .is_some_and(|provider| !provider.is_empty());
    let custom_auth = settings["auth"]["type"]
        .as_str()
        .is_some_and(|auth| !matches!(auth, "" | "oauth-personal" | "oauth-business"));
    if custom_model_provider || custom_auth {
        return Err(AdapterError::new(
            ErrorClass::Protocol,
            "Antigravity 当前本机 ACP 配置使用其他认证来源，无法为所选 Google 账号安全复用图片会话",
        ));
    }
    Ok(())
}

/// Inspect upstream failures without copying native text into operator errors.
pub(crate) fn native_failure(text: &str) -> AdapterError {
    static STATUS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(?:code|status|http)[ :]*(\d{3})\b").unwrap());
    static ATTEMPTS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)API error \(attempt (\d+)\)").unwrap());
    let mut error =
        if let Some(status) = STATUS.captures(text).and_then(|c| c[1].parse::<u16>().ok()) {
            from_http_status(status, None)
        } else {
            classify_text(text)
        };
    if error.class == ErrorClass::Other
        && text.to_ascii_lowercase().contains("no capacity available")
    {
        error = classified(ErrorClass::Overloaded, "agy:no_capacity");
    }
    if let Some(attempts) = ATTEMPTS
        .captures(text)
        .and_then(|c| c[1].parse::<u32>().ok())
        .filter(|n| *n >= 2)
    {
        error.code = Some(format!("native:retry_exhausted:agy:attempts:{attempts}"));
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Allow;

    impl TurnHost for Allow {
        fn approve(&self, _: Value) -> BoxFuture<'static, bool> {
            Box::pin(async { true })
        }
        fn pending_notes(&self) -> Vec<Value> {
            vec![]
        }
        fn accept_notes(&self, _: &[i64]) {}
    }

    fn fake_cli() -> &'static Path {
        static FIXTURE: std::sync::OnceLock<(tempfile::TempDir, PathBuf)> =
            std::sync::OnceLock::new();
        &FIXTURE
            .get_or_init(|| {
                let dir = tempfile::tempdir().unwrap();
                let source = dir.path().join("agy.c");
                let exe = dir.path().join(if cfg!(windows) { "agy.exe" } else { "agy" });
                std::fs::write(
                    &source,
                    r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifdef _WIN32
#include <windows.h>
#else
#include <fcntl.h>
#include <unistd.h>
#endif
int main(int argc, char **argv) {
    const char *model = "@native", *kind = "run";
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "models")) kind = "models";
        if (!strcmp(argv[i], "/effort")) kind = "effort";
        if (!strcmp(argv[i], "--model") && i + 1 < argc) {
            model = argv[++i];
            if (!strcmp(model, "@native")) return 5;
        }
    }
    char line[1024];
    int length = snprintf(line, sizeof(line), "%s\t%s\t%s\n", getenv("AGY_ADC_AUTH") ? "adc" : "google", kind, model);
#ifdef _WIN32
    HANDLE log = CreateFileA(getenv("FAKE_AGY_LOG"), FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE, NULL, OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    if (log == INVALID_HANDLE_VALUE) return 2;
    DWORD written;
    WriteFile(log, line, length, &written, NULL);
    CloseHandle(log);
#else
    int log = open(getenv("FAKE_AGY_LOG"), O_WRONLY | O_CREAT | O_APPEND, 0600);
    if (log < 0) return 2;
    write(log, line, length);
    close(log);
#endif
    if (!strcmp(kind, "models")) puts(getenv("FAKE_AGY_MODELS"));
    else if (!strcmp(kind, "effort")) puts(getenv("FAKE_AGY_EFFORT"));
    else {
        const char *token = getenv("FAKE_AGY_EXPECT_TOKEN"), *base = getenv("FAKE_AGY_EXPECT_BASE");
        if (token && (!getenv("GEMINI_API_KEY") || strcmp(token, getenv("GEMINI_API_KEY")))) return 3;
        if (base && (!getenv("GOOGLE_GEMINI_BASE_URL") || strcmp(base, getenv("GOOGLE_GEMINI_BASE_URL")))) return 4;
        char input[8192];
        while (fgets(input, sizeof(input), stdin)) {}
        puts("{\"event\":\"init\",\"conversation_id\":\"fixture-session\"}");
        puts("{\"event\":\"result\",\"result\":{\"conversation_id\":\"fixture-session\",\"status\":\"SUCCESS\",\"response\":\"fixture-ok\"}}");
    }
    return 0;
}
"#,
                )
                .unwrap();
                let compiler = std::env::var_os("CC_x86_64_pc_windows_gnu")
                    .or_else(|| std::env::var_os("CC"))
                    .unwrap_or_else(|| if cfg!(windows) { "clang" } else { "cc" }.into());
                let output = std::process::Command::new(compiler)
                    .arg(&source)
                    .arg("-o")
                    .arg(&exe)
                    .output()
                    .expect("the fake agy CLI requires the workspace C compiler");
                assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
                (dir, exe)
            })
            .1
    }

    fn fixture_sys(root: &Path) -> Sys {
        let mut sys = Sys::new(vec![], root.to_path_buf(), root.to_path_buf());
        sys.set("PATH", fake_cli().parent().unwrap().to_string_lossy());
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            sys.set("SystemRoot", system_root.to_string_lossy());
        }
        sys.set("FAKE_AGY_LOG", root.join("calls.tsv").to_string_lossy());
        sys.set("FAKE_AGY_MODELS", "model-a\tModel A\nmodel-b\tModel B");
        sys.set(
            "FAKE_AGY_EFFORT",
            json!({"command":{"data":{
                "adjustable":true,"available":["low","high"],"current":"low"
            }}})
            .to_string(),
        );
        sys
    }

    fn route(provider: &str, model: &str, effort: Option<&str>) -> Route {
        Route {
            provider: provider.into(),
            model: model.into(),
            mode: "cli".into(),
            reasoning_effort: effort.map(str::to_string),
        }
    }

    fn calls(root: &Path) -> Vec<String> {
        std::fs::read_to_string(root.join("calls.tsv"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    async fn run_fixture(sys: Sys, route: Route) -> Result<RunOutcome, AdapterError> {
        let req = RunRequest {
            ticket: 1,
            job: 1,
            turn: 1,
            agent: "antigravity".into(),
            prompt: "fixture".into(),
            images: vec![],
            cwd: sys.temp.clone(),
            route,
            profile: "full".into(),
            writable: true,
            session_id: None,
        };
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        Antigravity::new(sys)
            .run(req, Arc::new(Allow), events, CancellationToken::new())
            .await
    }

    #[tokio::test]
    async fn catalog_refreshes_all_native_sources_and_efforts() {
        let root = tempfile::tempdir().unwrap();
        let mut sys = fixture_sys(root.path());
        sys.set("AGY_ADC_AUTH", "true");
        let adapter = Antigravity::new(sys);
        for count in [8, 16] {
            let catalog = adapter.catalog().await.unwrap().value;
            assert_eq!(catalog["providers"].as_array().unwrap().len(), 2);
            assert_eq!(calls(root.path()).len(), count);
        }
        let log = calls(root.path());
        for source in ["google", "adc"] {
            for action in [
                "models\t@native",
                "effort\t@native",
                "effort\tmodel-a",
                "effort\tmodel-b",
            ] {
                assert_eq!(
                    log.iter()
                        .filter(|line| **line == format!("{source}\t{action}"))
                        .count(),
                    2
                );
            }
        }
    }

    #[tokio::test]
    async fn run_only_probes_selected_source_and_explicit_effort() {
        for (source, model, effort) in [
            ("google", "model-b", None),
            ("adc", "model-a", None),
            ("google", "model-b", Some("high")),
            ("adc", "@native", Some("low")),
        ] {
            let root = tempfile::tempdir().unwrap();
            let mut sys = fixture_sys(root.path());
            sys.set("AGY_ADC_AUTH", "true");
            let outcome = run_fixture(sys, route(source, model, effort))
                .await
                .unwrap();
            assert_eq!(outcome.text, "fixture-ok");
            let mut expected = vec![format!("{source}\tmodels\t@native")];
            if effort.is_some() {
                expected.push(format!("{source}\teffort\t{model}"));
            }
            expected.push(format!("{source}\trun\t{model}"));
            assert_eq!(calls(root.path()), expected);
        }
    }

    #[tokio::test]
    async fn run_rechecks_removed_models_and_refuses_unsupported_effort() {
        let root = tempfile::tempdir().unwrap();
        let mut sys = fixture_sys(root.path());
        let selected = route("google", "model-b", None);
        run_fixture(sys.clone(), selected.clone()).await.unwrap();
        sys.set("FAKE_AGY_MODELS", "model-a\tModel A");
        assert!(run_fixture(sys.clone(), selected).await.is_err());
        assert_eq!(calls(root.path()).len(), 3);
        sys.set(
            "FAKE_AGY_EFFORT",
            json!({"command":{"data":{"adjustable":false}}}).to_string(),
        );
        assert!(
            run_fixture(sys.clone(), route("google", "model-a", Some("high")))
                .await
                .is_err()
        );
        assert_eq!(calls(root.path()).len(), 5);
        assert!(
            run_fixture(sys.clone(), route("missing", "model-a", None))
                .await
                .is_err()
        );
        assert!(
            run_fixture(sys, route("adc", "model-a", None))
                .await
                .is_err()
        );
        assert_eq!(calls(root.path()).len(), 5);
    }

    #[tokio::test]
    async fn run_refreshes_only_selected_api_source_with_its_current_credentials() {
        use std::io::{BufRead, BufReader};
        use std::net::TcpListener;

        let root = tempfile::tempdir().unwrap();
        let mut sys = fixture_sys(root.path());
        let unused = TcpListener::bind("127.0.0.1:0").unwrap();
        unused.set_nonblocking(true).unwrap();
        let unused_base = format!("http://{}", unused.local_addr().unwrap());
        std::fs::create_dir_all(root.path().join(".gemini/antigravity-cli")).unwrap();
        std::fs::write(
            root.path().join(SETTINGS_REL),
            r#"{"modelProvider":"gemini","model":"model-a"}"#,
        )
        .unwrap();
        sys.set("GEMINI_API_KEY", "unused-token");
        sys.set("GOOGLE_GEMINI_BASE_URL", unused_base);
        sys.set("AGY_ADC_AUTH", "true");

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut requests = vec![];
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = String::new();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    request.push_str(&line);
                }
                requests.push(request);
                let body = r#"{"models":[{"name":"models/model-b","supportedGenerationMethods":["generateContent"]}]}"#;
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            requests
        });
        std::fs::create_dir_all(root.path().join(".cc-switch")).unwrap();
        let db = rusqlite::Connection::open(root.path().join(".cc-switch/cc-switch.db")).unwrap();
        db.execute_batch("CREATE TABLE providers(app_type TEXT,id TEXT,name TEXT,settings_config TEXT,is_current INTEGER)").unwrap();
        for (index, token) in ["selected-token-1", "selected-token-2"]
            .into_iter()
            .enumerate()
        {
            let config = json!({"env":{"GEMINI_API_KEY":token,"GOOGLE_GEMINI_BASE_URL":base,"GEMINI_MODEL":"model-b"}}).to_string();
            if index == 0 {
                db.execute(
                    "INSERT INTO providers VALUES ('agy','selected','Selected',?1,0)",
                    [&config],
                )
                .unwrap();
            } else {
                db.execute("UPDATE providers SET settings_config=?1", [&config])
                    .unwrap();
            }
            sys.set("FAKE_AGY_EXPECT_TOKEN", token);
            sys.set("FAKE_AGY_EXPECT_BASE", &base);
            let selected = configured(&sys)
                .into_iter()
                .find(|row| row.base == base)
                .unwrap();
            assert_eq!(
                run_fixture(sys.clone(), route(&selected.id, "model-b", None))
                    .await
                    .unwrap()
                    .text,
                "fixture-ok"
            );
        }
        let requests = server.join().unwrap();
        for (request, token) in requests
            .iter()
            .zip(["selected-token-1", "selected-token-2"])
        {
            assert!(request.starts_with("GET /v1beta/models?pageSize=1000 "));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains(&format!("x-goog-api-key: {token}"))
            );
            assert!(!request.contains("unused-token"));
        }
        assert_eq!(
            unused.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(
            calls(root.path()),
            ["google\trun\tmodel-b", "google\trun\tmodel-b"]
        );
        let selected = configured(&sys)
            .into_iter()
            .find(|row| row.base == base)
            .unwrap();
        db.execute("DELETE FROM providers", []).unwrap();
        assert!(
            run_fixture(sys, route(&selected.id, "model-b", None))
                .await
                .is_err()
        );
        assert_eq!(calls(root.path()).len(), 2);
    }

    #[test]
    fn image_routes_reject_other_providers_without_falling_back() {
        assert!(image_provider("google").is_ok());
        for provider in ["adc", "agy-configured-api", "missing"] {
            assert_eq!(
                image_provider(provider).unwrap_err().class,
                ErrorClass::Protocol
            );
        }
    }

    #[test]
    fn google_image_routes_refuse_other_saved_authentication_sources() {
        assert!(image_account_settings(&json!({})).is_ok());
        assert!(image_account_settings(&json!({"auth":{"type":"oauth-personal"}})).is_ok());
        assert!(image_account_settings(&json!({"modelProvider":"gemini"})).is_err());
        for auth in ["gemini-api-key", "agent-platform", "unknown"] {
            assert!(image_account_settings(&json!({"auth":{"type":auth}})).is_err());
        }
    }

    #[test]
    fn terminal_native_retries_preserve_overload_and_exhaustion() {
        let error = native_failure(
            "API error (attempt 4): UNAVAILABLE (code 503): No capacity available for model private-model",
        );
        assert_eq!(error.class, ErrorClass::Overloaded);
        assert_eq!(error.http_status, Some(503));
        assert_eq!(
            error.code.as_deref(),
            Some("native:retry_exhausted:agy:attempts:4")
        );
        assert!(!error.message.contains("private-model"));
        for text in ["API error (attempt 1): UNAVAILABLE (code 503)", "HTTP 503"] {
            let error = native_failure(text);
            assert_eq!(error.http_status, Some(503));
            assert!(!error.code.unwrap().starts_with("native:retry_exhausted:"));
        }
    }

    #[test]
    fn empty_or_changed_native_format_is_not_a_complete_model_list() {
        for output in [
            "",
            "Fetching available models...",
            "model without tab",
            "bad id\tName",
        ] {
            assert!(native_models(output).is_err());
        }
        assert_eq!(
            native_models("Fetching available models...\ngemini-x\tGemini X\n").unwrap()[0]["id"],
            "gemini-x"
        );
    }
}
