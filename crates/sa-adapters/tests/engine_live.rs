//! Billable end-to-end check (port of `tests-node/adapters-engine-live.test.cjs`).
//! Opt-in: `SA_LIVE_ENGINE=1`.
//!
//! One real ticket in a temporary journal: the DeepSeek lead dispatches file
//! challenges to Codex (writer), Claude and Antigravity, and must relay the
//! three exact tokens. Checks that every child's journaled native terminal
//! equals its stored result and the report the lead received, that the
//! Codex writer produced the exact handoff bytes, and that every event hash
//! matches its payload.

use sa_adapters::sys::Sys;
use sa_engine::{CreateParams, Engine, EngineOptions};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration;

const AGENTS: [&str; 3] = ["codex", "claude", "antigravity"];

fn hash(s: &[u8]) -> String {
    format!("{:x}", Sha256::digest(s))
}

fn token(tag: &str) -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    hash(format!("{tag}-{}-{}", t.as_nanos(), std::process::id()).as_bytes())[..20].to_string()
}

fn native_terminal(agent: &str, e: &Value) -> Option<String> {
    let s = match agent {
        "codex" if e["method"] == json!("item/completed")
            && e["params"]["item"]["type"] == json!("agentMessage")
            && e["params"]["item"]["phase"] != json!("commentary") =>
        {
            &e["params"]["item"]["text"]
        }
        "claude" if e["type"] == json!("result") => &e["result"],
        "antigravity" if e["event"] == json!("result") => &e["result"]["response"],
        _ => return None,
    };
    s.as_str().map(str::to_string)
}

/// Pick an available route per agent: the catalog default when usable,
/// else the first available provider (the local Claude default can point at
/// an unavailable source).
async fn selections(adapters: &[std::sync::Arc<dyn sa_engine::adapter::Adapter>]) -> Value {
    let mut out = Map::new();
    for a in adapters {
        let Ok(c) = a.catalog().await else { continue };
        let cat = c.value;
        let default = serde_json::from_value::<sa_engine::adapter::Route>(cat["default"].clone()).ok();
        let usable = default.filter(|r| sa_adapters::common::select(&cat, r).is_ok());
        let route = usable.or_else(|| {
            let p = cat["providers"].as_array()?.iter().find(|p| {
                p["available"] != json!(false) && p["models"].as_array().is_some_and(|m| !m.is_empty())
            })?;
            let model = p["defaultModel"]
                .as_str()
                .filter(|d| p["models"].as_array().is_some_and(|m| m.iter().any(|x| x["id"] == json!(d))))
                .or_else(|| p["models"][0]["id"].as_str())?;
            Some(sa_engine::adapter::Route { provider: p["id"].as_str()?.into(), model: model.into(), mode: "cli".into(), reasoning_effort: None })
        });
        if let Some(r) = route {
            out.insert(a.agent().into(), json!({"provider": r.provider, "model": r.model, "mode": r.mode}));
        }
    }
    Value::Object(out)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_engine_routes_three_real_children_and_relays_exact_reports() {
    if std::env::var("SA_LIVE_ENGINE").as_deref() != Ok("1") {
        eprintln!("set SA_LIVE_ENGINE=1 to run a billable four-harness ticket");
        return;
    }
    let sys = Sys::from_process();
    let parent = sys.platform_dir().join("checks");
    std::fs::create_dir_all(&parent).unwrap();
    let base = tempfile::Builder::new().prefix("rust-engine-native-").tempdir_in(&parent).unwrap().keep();
    let cwd = base.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    eprintln!("engine check directory {}", base.display());

    let mut values = Map::new();
    let mut commands = vec![];
    for agent in AGENTS {
        let v = token(agent);
        std::fs::write(cwd.join(format!("{agent}.txt")), &v).unwrap();
        values.insert(agent.into(), json!(v));
        let write = if agent == "codex" {
            "Write its identical bytes without adding a newline into handoff.txt. "
        } else {
            ""
        };
        let body = format!(
            "Read {agent}.txt from the assigned local project. {write}This is an exact-output file challenge. \
The entire final reply must consist only of the 20-character text from that file. \
Do not emit SA_ROLE or any other SA_ control messages, preamble or markdown. Do not delegate."
        );
        commands.push(format!(
            "SA_SEND {}",
            json!({"id": agent, "to": agent, "body": body, "write": agent == "codex"})
        ));
    }
    let goal = format!(
        "This is the primary agent instruction only. Every child must follow only its own assignment. \
On turn one output these exact three dispatch commands, one complete physical line each, then end the turn.\n{}\n\
After you receive all three native reports, return the three exact reported tokens grouped by sender. \
Do not send more work. Do not inspect the challenge files yourself.",
        commands.join("\n")
    );

    let adapters = sa_adapters::all(sys);
    let picked = selections(&adapters).await;
    let engine = Engine::open(base.join("ledger"), adapters, EngineOptions::default()).unwrap();
    let ticket = engine
        .create(CreateParams {
            goal,
            cwd: cwd.clone(),
            lead: Some("deepseek".into()),
            permission_profile: Some("partial".into()),
            allow_lead_routing: Some(false),
            selections: Some(picked.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    engine.start().unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(420);
    let state = loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Some(f) = engine.fault() {
            panic!("engine fault: {f}");
        }
        let s = engine.root(ticket).unwrap()["state"].as_str().unwrap_or_default().to_string();
        if matches!(s.as_str(), "done" | "failed" | "partial" | "cancelled" | "interrupted")
            || tokio::time::Instant::now() > deadline
        {
            break s;
        }
    };
    let all = |sql: &str| engine.db().with(|q| q.all(sql, &[])).unwrap();
    let jobs = all(&format!("SELECT id,parent,agent,state,result FROM jobs WHERE ticket={ticket} ORDER BY id"));
    let root = &jobs[0];
    let root_id = root["id"].as_i64().unwrap();
    let events = all(&format!("SELECT seq,job,kind,payload,sha256 FROM events WHERE ticket={ticket} ORDER BY seq"));
    let payload = |e: &Map<String, Value>| serde_json::from_str::<Value>(e["payload"].as_str().unwrap()).unwrap();
    let reports = events
        .iter()
        .rev()
        .find(|e| e["kind"] == json!("input") && e["job"] == json!(root_id))
        .map(|e| payload(e)["reports"].clone())
        .unwrap_or(json!([]));
    let mut children = Map::new();
    for agent in AGENTS {
        let child = jobs.iter().find(|j| j["parent"] == json!(root_id) && j["agent"] == json!(agent));
        let id = child.and_then(|c| c["id"].as_i64());
        let native = events
            .iter()
            .rev()
            .filter(|e| e["kind"] == json!("native") && e["job"].as_i64() == id)
            .find_map(|e| native_terminal(agent, &payload(e)));
        let result = child.and_then(|c| c["result"].as_str()).map(str::to_string);
        let delivered = reports
            .as_array()
            .and_then(|r| r.iter().find(|r| r["id"].as_i64() == id))
            .and_then(|r| r["result"].as_str())
            .map(str::to_string);
        let want = values[agent].as_str().unwrap();
        children.insert(
            agent.into(),
            json!({
                "state": child.map(|c| c["state"].clone()),
                "challenge": result.as_deref().map(str::trim) == Some(want),
                "exact_delivery": native.is_some() && native == result && native == delivered,
                "root_includes": root["result"].as_str().is_some_and(|r| r.contains(want)),
            }),
        );
    }
    let exact_file = std::fs::read(cwd.join("handoff.txt")).ok() == values["codex"].as_str().map(|v| v.as_bytes().to_vec());
    let exact_hashes = events
        .iter()
        .all(|e| hash(e["payload"].as_str().unwrap().as_bytes()) == e["sha256"].as_str().unwrap());
    let kinds: std::collections::BTreeMap<String, usize> = events.iter().fold(Default::default(), |mut m, e| {
        *m.entry(e["kind"].as_str().unwrap().to_string()).or_default() += 1;
        m
    });
    let passed = jobs.len() == 4
        && state == "done"
        && exact_file
        && exact_hashes
        && children.values().all(|c| {
            c["state"] == json!("done") && c["challenge"] == json!(true) && c["exact_delivery"] == json!(true) && c["root_includes"] == json!(true)
        });
    let receipt = json!({
        "state": state,
        "jobs": jobs.len(),
        "selections": picked,
        "children": children,
        "exact_file": exact_file,
        "exact_hashes": exact_hashes,
        "events": events.len(),
        "kinds": kinds,
        "passed": passed,
    });
    std::fs::write(base.join("receipt.json"), serde_json::to_string_pretty(&receipt).unwrap()).unwrap();
    eprintln!("{receipt}");
    if !passed {
        let _ = engine.cancel(ticket);
    }
    engine.close().await;
    assert!(passed, "four-harness receipt mismatch");
}
