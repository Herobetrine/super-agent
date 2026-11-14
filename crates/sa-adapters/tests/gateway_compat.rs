//! Real Claude Code CLI against a loopback stub through a CC Switch aggregation
//! listener. Opt-in: `SA_LOCAL_GATEWAY_CLI=1`. No model request leaves this
//! machine; the stub answers locally with a dummy key. Re-run after CLI updates.

use futures::future::BoxFuture;
use sa_adapters::{claude::Claude, sys::Sys};
use sa_engine::adapter::{Adapter, Route, RunRequest, TurnHost};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

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

/// `(roles, anthropic-beta)` of each Messages request.
type Seen = Arc<Mutex<Vec<(Vec<String>, String)>>>;

fn stub() -> (u16, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Seen = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let log = log.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let header = |name: &str| {
                    head.lines().find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
                    })
                };
                let length: usize = header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let (kind, payload) = if head.starts_with("GET") {
                    ("application/json", json!({"data":[{"id":"claude-opus-5-5"}],"has_more":false}).to_string())
                } else {
                    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let roles = req["messages"].as_array().into_iter().flatten()
                        .map(|m| m["role"].as_str().unwrap_or_default().to_string()).collect();
                    log.lock().unwrap().push((roles, header("anthropic-beta").unwrap_or_default()));
                    let model = req["model"].as_str().unwrap_or("m").to_string();
                    let events = [
                        json!({"type":"message_start","message":{"id":"msg_local","type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"usage":{"input_tokens":1,"output_tokens":0}}}),
                        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
                        json!({"type":"content_block_stop","index":0}),
                        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
                        json!({"type":"message_stop"}),
                    ];
                    let sse = events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect();
                    ("text/event-stream", sse)
                };
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len());
            });
        }
    });
    (port, seen)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aggregation_route_turn_sends_no_in_conversation_system_messages() {
    if std::env::var("SA_LOCAL_GATEWAY_CLI").is_err() {
        eprintln!("set SA_LOCAL_GATEWAY_CLI=1 to run the installed Claude CLI against a loopback stub");
        return;
    }
    let (port, seen) = stub();
    let home = tempfile::tempdir().unwrap();
    let base = format!("http://127.0.0.1:{port}");
    std::fs::create_dir_all(home.path().join(".cc-switch")).unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    let db = rusqlite::Connection::open(home.path().join(".cc-switch/cc-switch.db")).unwrap();
    db.execute_batch("CREATE TABLE providers(app_type TEXT,id TEXT,name TEXT,settings_config TEXT,is_current INTEGER,meta TEXT,category TEXT);
        CREATE TABLE proxy_config(app_type TEXT,listen_address TEXT,listen_port INTEGER,enabled INTEGER);").unwrap();
    let member = json!({"env":{"ANTHROPIC_BASE_URL":base,"ANTHROPIC_API_KEY":"dummy"}}).to_string();
    db.execute("INSERT INTO providers VALUES ('claude','ferry','ferryman',?1,0,'{}','')", [&member]).unwrap();
    db.execute("INSERT INTO providers VALUES ('claude','other','other',?1,0,'{}','')", [&member]).unwrap();
    db.execute("INSERT INTO proxy_config VALUES ('claude','127.0.0.1',?1,1)", [port]).unwrap();
    std::fs::write(home.path().join(".cc-switch/live-state.json"), json!({"apps":{"claude":{"mode":"proxy","proxy_route":"ferry",
        "stack":{"enabled":true,"members":["ferry","other"],"keys":{"ferryman":"ferry","api":"other"}}}}}).to_string()).unwrap();
    std::fs::write(home.path().join(".claude/settings.json"),
        json!({"env":{"ANTHROPIC_BASE_URL":base,"ANTHROPIC_AUTH_TOKEN":"PROXY_MANAGED"}}).to_string()).unwrap();
    let mut vars = vec![];
    for name in ["PATH", "SystemRoot", "COMSPEC", "PATHEXT", "WINDIR", "TEMP", "TMP"] {
        if let Ok(value) = std::env::var(name) {
            vars.push((name.to_string(), value));
        }
    }
    let home_text = home.path().to_string_lossy().to_string();
    for name in ["HOME", "USERPROFILE"] {
        vars.push((name.into(), home_text.clone()));
    }
    let sys = Sys::new(vars, home.path().into(), home.path().into());
    let adapter = Claude::new(sys);
    let catalog = adapter.catalog().await.unwrap().value;
    let provider = catalog["providers"].as_array().unwrap().iter()
        .find(|p| p["label"].as_str().is_some_and(|l| l.starts_with("CC Switch 聚合")))
        .expect("aggregation source is listed").clone();
    for model in ["claude-opus-5-5", "ccs-claude-api--claude-opus-5-5"] {
        let before = seen.lock().unwrap().len();
        let request = RunRequest {
            ticket: 1,
            job: 1,
            turn: 1,
            agent: "claude".into(),
            prompt: "reply with ok".into(),
            images: vec![],
            cwd: home.path().into(),
            route: Route { provider: provider["id"].as_str().unwrap().into(), model: model.into(), mode: "cli".into(), reasoning_effort: None },
            profile: "readonly".into(),
            writable: false,
            session_id: None,
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let outcome = adapter.run(request, Arc::new(Host), tx, CancellationToken::new()).await;
        assert!(outcome.is_ok(), "{model}: {:?}", outcome.err().map(|e| e.message));
        let requests = seen.lock().unwrap()[before..].to_vec();
        eprintln!("{model}: {requests:?}");
        assert!(!requests.is_empty());
        for (roles, beta) in requests {
            assert!(roles.iter().all(|r| r != "system"), "{model}: {roles:?}");
            assert!(!beta.contains("per-turn-control"), "{model}: {beta}");
        }
    }
}
