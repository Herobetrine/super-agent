//! Billable acceptance check (port of `tests-node/adapters-live.test.cjs`).
//! Opt-in: `SA_LIVE_TURNS=1`; `SA_LIVE_AGENTS=codex,claude` limits the set.
//!
//! Each harness gets one read-only file challenge in a fresh project under
//! `~/.local/share/superagent/checks`, then a second turn that resumes the
//! returned native session. The receipt stores hashes, never the nonce or
//! the model text.

use futures::future::BoxFuture;
use sa_adapters::sys::Sys;
use sa_engine::adapter::{Adapter, AdapterEvent, Route, RunRequest, TurnHost};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn hash(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}

fn nonce() -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    hash(&format!("{}-{}", t.as_nanos(), std::process::id()))[..24].to_string()
}

struct Host;

impl TurnHost for Host {
    fn approve(&self, _action: Value) -> BoxFuture<'static, bool> {
        Box::pin(async { false })
    }
    fn pending_notes(&self) -> Vec<Value> {
        vec![]
    }
    fn accept_notes(&self, _seqs: &[i64]) {}
}

/// The terminal text as the native harness itself reported it.
fn native_terminal(agent: &str, e: &Value) -> Option<String> {
    let s = match agent {
        "codex" if e["method"] == json!("item/completed")
            && e["params"]["item"]["type"] == json!("agentMessage")
            && e["params"]["item"]["phase"] != json!("commentary") =>
        {
            &e["params"]["item"]["text"]
        }
        "claude" if e["type"] == json!("result") => &e["result"],
        "deepseek" if e["type"] == json!("final") => &e["text"],
        "antigravity" if e["event"] == json!("result") => &e["result"]["response"],
        _ => return None,
    };
    s.as_str().map(str::to_string)
}

/// The catalog default, or the first available provider and model when the
/// local default points at an unavailable source.
fn live_route(catalog: &Value) -> Option<Route> {
    let default = serde_json::from_value::<Route>(catalog["default"].clone()).ok();
    if let Some(r) = default.filter(|r| sa_adapters::common::select(catalog, r).is_ok()) {
        return Some(r);
    }
    let p = catalog["providers"].as_array()?.iter().find(|p| {
        p["available"] != json!(false) && p["models"].as_array().is_some_and(|m| !m.is_empty())
    })?;
    let models = p["models"].as_array()?;
    let model = p["defaultModel"]
        .as_str()
        .filter(|d| models.iter().any(|m| m["id"] == json!(d)))
        .or_else(|| models.iter().find(|m| m["available"] != json!(false)).and_then(|m| m["id"].as_str()))?;
    Some(Route { provider: p["id"].as_str()?.into(), model: model.into(), mode: "cli".into() })
}

struct Turn {
    text: String,
    session: Option<String>,
    native: Option<String>,
    frames: usize,
    early_session: Option<String>,
}

async fn turn(adapter: &Arc<dyn Adapter>, project: &Path, route: Route, prompt: &str, session: Option<String>) -> Result<Turn, String> {
    let req = RunRequest {
        ticket: 0,
        job: 0,
        turn: 1,
        agent: adapter.agent().into(),
        prompt: prompt.into(),
        images: vec![],
        cwd: project.to_path_buf(),
        route,
        profile: "readonly".into(),
        writable: false,
        session_id: session,
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let run = adapter.run(req, Arc::new(Host), tx, CancellationToken::new());
    let out = tokio::time::timeout(Duration::from_secs(180), run)
        .await
        .map_err(|_| "timed out after 180 s".to_string())?
        .map_err(|e| format!("{:?} {} ({})", e.class, e.message, e.code.unwrap_or_default()))?;
    let (mut native, mut frames, mut early) = (None, 0, None);
    while let Ok(ev) = rx.try_recv() {
        match ev {
            AdapterEvent::Journal { kind, data } if kind == "native" => {
                frames += 1;
                if let Some(t) = native_terminal(adapter.agent(), &data) {
                    native = Some(t);
                }
            }
            AdapterEvent::Session(s) if early.is_none() => early = Some(s),
            _ => {}
        }
    }
    Ok(Turn { text: out.text, session: out.session_id, native, frames, early_session: early })
}

async fn check(adapter: Arc<dyn Adapter>, base: &Path) -> Value {
    let agent = adapter.agent();
    let project = base.join(agent);
    std::fs::create_dir_all(&project).unwrap();
    let first = nonce();
    std::fs::write(project.join("challenge.txt"), &first).unwrap();
    let mut row = json!({"agent": agent, "passed": false, "expected": hash(&first)});
    let catalog = match adapter.catalog().await {
        Ok(c) => c.value,
        Err(e) => {
            row["error"] = json!(format!("catalog: {e}"));
            return row;
        }
    };
    let Some(route) = live_route(&catalog) else {
        row["error"] = json!("catalog has no usable route");
        return row;
    };
    row["route"] = json!(route);
    let ask = "Read challenge.txt in this project. Return only the exact text inside it. Do not modify any files.";
    let started = std::time::Instant::now();
    let one = match turn(&adapter, &project, route.clone(), ask, None).await {
        Ok(t) => t,
        Err(e) => {
            row["error"] = json!(format!("turn 1: {e}"));
            return row;
        }
    };
    row["turn1"] = json!({
        "ms": started.elapsed().as_millis() as u64,
        "frames": one.frames,
        "answer": hash(&one.text),
        "native": one.native.as_deref().map(hash),
        "matched": one.text.trim() == first,
        "exact_native": one.native.as_deref() == Some(one.text.as_str()),
        "session": one.session.is_some(),
        "early_session": one.early_session.is_some() && one.early_session == one.session,
    });
    // Resume: the file changes, the session must be reused.
    let second = nonce();
    std::fs::write(project.join("challenge.txt"), &second).unwrap();
    let started = std::time::Instant::now();
    let two = turn(&adapter, &project, route, "challenge.txt changed. Read it again and return only its exact text.", one.session.clone()).await;
    match two {
        Ok(t) => {
            row["turn2"] = json!({
                "ms": started.elapsed().as_millis() as u64,
                "matched": t.text.trim() == second,
                "same_session": t.session.is_some() && t.session == one.session,
            });
            row["passed"] = json!(
                one.text.trim() == first
                    && one.native.as_deref() == Some(one.text.as_str())
                    && one.session.is_some()
                    && t.text.trim() == second
                    && t.session == one.session
            );
        }
        Err(e) => row["error"] = json!(format!("turn 2: {e}")),
    }
    row
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn four_real_readonly_harnesses_answer_resume_and_match_native_text() {
    if std::env::var("SA_LIVE_TURNS").as_deref() != Ok("1") {
        eprintln!("set SA_LIVE_TURNS=1 to run billable live turns");
        return;
    }
    let sys = Sys::from_process();
    let parent = sys.platform_dir().join("checks");
    std::fs::create_dir_all(&parent).unwrap();
    let base = tempfile::Builder::new().prefix("rust-native-").tempdir_in(&parent).unwrap().keep();
    eprintln!("live check directory {}", base.display());
    let only: Option<Vec<String>> =
        std::env::var("SA_LIVE_AGENTS").ok().map(|v| v.split(',').map(|s| s.trim().to_string()).collect());
    let adapters: Vec<Arc<dyn Adapter>> = sa_adapters::all(sys)
        .into_iter()
        .filter(|a| only.as_ref().is_none_or(|o| o.iter().any(|x| x == a.agent())))
        .collect();
    let rows = futures::future::join_all(adapters.into_iter().map(|a| {
        let base = base.clone();
        async move { check(a, &base).await }
    }))
    .await;
    let receipt = base.join("receipt.json");
    std::fs::write(&receipt, serde_json::to_string_pretty(&rows).unwrap()).unwrap();
    for r in &rows {
        eprintln!("{r}");
    }
    eprintln!("receipt {}", receipt.display());
    let failed: Vec<&str> = rows
        .iter()
        .filter(|r| r["passed"] != json!(true))
        .map(|r| r["agent"].as_str().unwrap_or("?"))
        .collect();
    assert!(failed.is_empty(), "failed: {failed:?}");
}
