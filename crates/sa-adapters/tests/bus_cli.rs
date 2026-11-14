//! Real harness CLIs reach the collaboration bus through the adapters. A
//! loopback stub plays the model and asks for one bus call; the journal must
//! then hold the message. Opt-in: `SA_LOCAL_GATEWAY_CLI=1`. No request leaves
//! this machine.

use futures::future::BoxFuture;
use sa_adapters::{claude::Claude, sys::Sys};
use sa_bus::CollabBus;
use sa_engine::adapter::{Adapter, Route, RunRequest, TurnHost};
use sa_engine::db::Db;
use sa_engine::services::Bus;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct Host;

impl TurnHost for Host {
    fn approve(&self, _action: Value) -> BoxFuture<'static, bool> {
        Box::pin(async { true })
    }
    fn pending_notes(&self) -> Vec<Value> {
        vec![]
    }
    fn accept_notes(&self, _seqs: &[i64]) {}
}

/// Anthropic Messages stub: asks for `mcp__sa_bus__send` once, then ends.
fn model() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
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
                let length: usize = head
                    .lines()
                    .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")))
                    .and_then(|(_, v)| v.trim().parse().ok())
                    .unwrap_or(0);
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let has_bus = req["tools"].as_array().into_iter().flatten().any(|t| t["name"] == "mcp__sa_bus__send");
                let answered = body.windows(11).any(|w| w == b"tool_result");
                let model = req["model"].as_str().unwrap_or("m").to_string();
                let (kind, payload) = if head.starts_with("GET") {
                    ("application/json", json!({"data":[{"id":"claude-opus-5-5"}],"has_more":false}).to_string())
                } else {
                    let block = if has_bus && !answered {
                        vec![
                            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_bus","name":"mcp__sa_bus__send","input":{}}}),
                            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"to\":\"lead\",\"body\":\"hello from claude\"}"}}),
                        ]
                    } else {
                        vec![
                            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}),
                        ]
                    };
                    let stop = if has_bus && !answered { "tool_use" } else { "end_turn" };
                    let mut events = vec![json!({"type":"message_start","message":{"id":"msg_local","type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"usage":{"input_tokens":1,"output_tokens":0}}})];
                    events.extend(block);
                    events.push(json!({"type":"content_block_stop","index":0}));
                    events.push(json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":{"output_tokens":1}}));
                    events.push(json!({"type":"message_stop"}));
                    let sse: String = events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect();
                    ("text/event-stream", sse)
                };
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len());
            });
        }
    });
    port
}

fn sys(home: &std::path::Path) -> Sys {
    let mut vars = vec![];
    for name in ["PATH", "SystemRoot", "COMSPEC", "PATHEXT", "WINDIR", "TEMP", "TMP"] {
        if let Ok(value) = std::env::var(name) {
            vars.push((name.to_string(), value));
        }
    }
    let text = home.to_string_lossy().to_string();
    vars.push(("HOME".into(), text.clone()));
    vars.push(("USERPROFILE".into(), text));
    Sys::new(vars, home.into(), home.into())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claude_full_access_turn_sends_through_the_bus() {
    if std::env::var("SA_LOCAL_GATEWAY_CLI").is_err() {
        eprintln!("set SA_LOCAL_GATEWAY_CLI=1 to run the installed Claude CLI against loopback stubs");
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let db = Db::open(data.path()).unwrap();
    db.with(|q| {
        q.conn().execute_batch(
            "INSERT INTO tickets(id,goal,cwd,writes,lead,created) VALUES(1,'g','c',0,'claude',0);
             INSERT INTO jobs(id,ticket,parent,agent,body,writing,depth,turn,state) VALUES
               (1,1,NULL,'codex','b',0,0,1,'waiting'),(2,1,1,'claude','b',0,1,1,'running');",
        )?;
        Ok(())
    })
    .unwrap();
    let bus = CollabBus::start(data.path()).await.unwrap();
    let grant = bus.grant(1, 2, 1, "claude");

    let home = tempfile::tempdir().unwrap();
    let base = format!("http://127.0.0.1:{}", model());
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    std::fs::write(
        home.path().join(".claude/settings.json"),
        json!({"env":{"ANTHROPIC_BASE_URL":base,"ANTHROPIC_AUTH_TOKEN":"dummy"}}).to_string(),
    )
    .unwrap();
    let adapter = Claude::new(sys(home.path()));
    let catalog = adapter.catalog().await.unwrap().value;
    let provider = catalog["providers"].as_array().unwrap().iter()
        .find(|p| p["available"] == true && p["label"].as_str().is_some_and(|l| l.ends_with("native")))
        .expect("native loopback source").clone();
    let request = RunRequest {
        ticket: 1,
        job: 2,
        turn: 1,
        agent: "claude".into(),
        prompt: "say hello to the lead".into(),
        images: vec![],
        cwd: home.path().into(),
        route: Route { provider: provider["id"].as_str().unwrap().into(), model: "claude-opus-5-5".into(), mode: "cli".into(), reasoning_effort: None },
        profile: "full".into(),
        writable: true,
        session_id: None,
        bus: grant,
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let outcome = adapter.run(request, Arc::new(Host), tx, CancellationToken::new()).await;
    let mut native = vec![];
    while let Ok(event) = rx.try_recv() {
        native.push(format!("{event:?}"));
    }
    assert!(outcome.is_ok(), "{:?}", outcome.err().map(|e| e.message));
    let init = native.iter().find(|e| e.contains(r#""subtype": String("init")"#)).cloned().unwrap_or_default();
    assert!(init.contains(r#""name": String("sa_bus"), "status": String("connected")"#), "bus not connected at init");
    assert!(init.contains(r#"String("mcp__sa_bus__send")"#), "bus tools must be loaded, not deferred");
    let notes = db.with(|q| q.notes(1, true)).unwrap();
    assert_eq!(notes.len(), 1, "the lead must hold the bus message");
    assert_eq!(notes[0]["body"], "hello from claude");
    assert_eq!(notes[0]["sender"], "claude (#2)");
}

/// OpenAI Responses stub: asks for the bus `send` function once, then ends.
fn responses() -> (u16, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
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
                let length: usize = head
                    .lines()
                    .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")))
                    .and_then(|(_, v)| v.trim().parse().ok())
                    .unwrap_or(0);
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let first = head.lines().next().unwrap_or_default().to_string();
                let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let names: Vec<String> = req["tools"].as_array().into_iter().flatten()
                    .filter_map(|t| t["name"].as_str().map(str::to_string)).collect();
                log.lock().unwrap().push(format!("{first} tools={names:?}"));
                let (kind, payload) = if !first.contains("/responses") {
                    ("application/json", json!({"object":"list","data":[{"id":"stub-model","object":"model"}],"models":[]}).to_string())
                } else {
                    // Codex 0.169 exposes an MCP server as one `namespace` tool.
                    let mounted = names.iter().any(|n| n == "mcp__sa_bus");
                    let answered = body.windows(20).any(|w| w == b"function_call_output");
                    let item = match (mounted, answered) {
                        (true, false) => json!({"type":"function_call","id":"fc_1","call_id":"call_1","namespace":"mcp__sa_bus","name":"send",
                            "arguments":"{\"to\":\"lead\",\"body\":\"hello from codex\"}","status":"completed"}),
                        _ => json!({"type":"message","id":"msg_1","role":"assistant","status":"completed",
                            "content":[{"type":"output_text","text":"done","annotations":[]}]}),
                    };
                    let usage = json!({"input_tokens":1,"input_tokens_details":{"cached_tokens":0},"output_tokens":1,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":2});
                    let events = [
                        json!({"type":"response.created","response":{"id":"resp_1","status":"in_progress","output":[]}}),
                        json!({"type":"response.output_item.added","output_index":0,"item":item}),
                        json!({"type":"response.output_item.done","output_index":0,"item":item}),
                        json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[item],"usage":usage}}),
                    ];
                    let sse: String = events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect();
                    ("text/event-stream", sse)
                };
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len());
            });
        }
    });
    (port, seen)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_full_access_turn_sends_through_the_bus() {
    if std::env::var("SA_LOCAL_GATEWAY_CLI").is_err() {
        eprintln!("set SA_LOCAL_GATEWAY_CLI=1 to run the installed Codex CLI against loopback stubs");
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let db = Db::open(data.path()).unwrap();
    db.with(|q| {
        q.conn().execute_batch(
            "INSERT INTO tickets(id,goal,cwd,writes,lead,created) VALUES(1,'g','c',0,'claude',0);
             INSERT INTO jobs(id,ticket,parent,agent,body,writing,depth,turn,state) VALUES
               (1,1,NULL,'claude','b',0,0,1,'waiting'),(2,1,1,'codex','b',0,1,1,'running');",
        )?;
        Ok(())
    })
    .unwrap();
    let bus = CollabBus::start(data.path()).await.unwrap();
    let grant = bus.grant(1, 2, 1, "codex");
    let (port, seen) = responses();
    let home = tempfile::tempdir().unwrap();
    let mut sys = sys(home.path());
    sys.set("CODEX_HOME", home.path().join(".codex").to_string_lossy());
    std::fs::create_dir_all(home.path().join(".codex")).unwrap();
    let provider = sa_store::providers::CustomProvider {
        id: "stub".into(),
        agent: "codex".into(),
        name: "stub".into(),
        base_url: format!("http://127.0.0.1:{port}/v1"),
        key_env: String::new(),
        models: vec!["stub-model".into()],
        enabled: true,
    };
    let adapter = sa_adapters::codex::Codex::with_registry(sys, sa_adapters::custom::registry(vec![provider]));
    let catalog = adapter.catalog().await.unwrap().value;
    let provider = catalog["providers"].as_array().unwrap().iter()
        .find(|p| p["label"].as_str().is_some_and(|l| l.contains("stub")))
        .expect("custom loopback source").clone();
    let request = RunRequest {
        ticket: 1,
        job: 2,
        turn: 1,
        agent: "codex".into(),
        prompt: "say hello to the lead".into(),
        images: vec![],
        cwd: home.path().into(),
        route: Route { provider: provider["id"].as_str().unwrap().into(), model: "stub-model".into(), mode: "cli".into(), reasoning_effort: None },
        profile: "full".into(),
        writable: true,
        session_id: None,
        bus: grant,
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let outcome = adapter.run(request, Arc::new(Host), tx, CancellationToken::new()).await;
    for line in seen.lock().unwrap().iter() {
        eprintln!("STUB {}", &line[..line.len().min(400)]);
    }
    assert!(outcome.is_ok(), "{:?}", outcome.err().map(|e| e.message));
    let notes = db.with(|q| q.notes(1, true)).unwrap();
    assert_eq!(notes.len(), 1, "the lead must hold the bus message");
    assert_eq!(notes[0]["body"], "hello from codex");
    assert_eq!(notes[0]["sender"], "codex (#2)");
}

/// Billable: two real Claude turns on one CC Switch source talk over the bus.
/// Opt-in: `SA_LIVE_BUS=<CC Switch source label>,<model>`, e.g.
/// `大核桃 · claude,gemini-4-hydrogen`. Prints the journal timeline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_two_claude_turns_exchange_a_code_word() {
    let Ok(spec) = std::env::var("SA_LIVE_BUS") else {
        eprintln!("set SA_LIVE_BUS=<source label>,<model> for a billable bus collaboration run");
        return;
    };
    let (label, model) = spec.split_once(',').expect("label,model");
    let data = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let db = Db::open(data.path()).unwrap();
    db.with(|q| {
        q.conn().execute_batch(
            "INSERT INTO tickets(id,goal,cwd,writes,lead,created) VALUES(1,'bus live test','c',0,'claude',0);
             INSERT INTO jobs(id,ticket,parent,agent,body,writing,depth,turn,state,role) VALUES
               (1,1,NULL,'claude','lead',0,0,1,'waiting','lead'),
               (2,1,1,'claude','worker',0,1,1,'running','worker'),
               (3,1,1,'claude','keeper',0,1,1,'running','keeper');",
        )?;
        Ok(())
    })
    .unwrap();
    let bus = CollabBus::start(data.path()).await.unwrap();
    let mut sys = Sys::from_process();
    // This test may run inside another Claude session; keep its session wiring out.
    sys.retain(|k| !k.to_ascii_uppercase().starts_with("CLAUDE_CODE_") && !k.to_ascii_uppercase().starts_with("ANTHROPIC_"));
    let adapter = Arc::new(Claude::new(sys));
    let catalog = adapter.catalog().await.unwrap().value;
    let provider = catalog["providers"].as_array().unwrap().iter()
        .find(|p| p["label"] == label)
        .unwrap_or_else(|| panic!("source {label} not found"))["id"].as_str().unwrap().to_string();
    let keeper_prompt = "You are task #3 in a multi-agent ticket and you hold the code word TANGERINE-42. \
        Another task will ask you for it over the collaboration bus (the sa_bus MCP tools). \
        Loop: call sa_bus `inbox`; if no request arrived, run the shell command `sleep 5` and check again, at most 12 times. \
        When a message asks for the code word, call sa_bus `send` with `to` set to the sender's task number and the body `CODE WORD: TANGERINE-42`, then stop and reply `delivered`.";
    let worker_prompt = "You are task #2 in a multi-agent ticket. Task #3 holds a code word you need. \
        Use the collaboration bus (the sa_bus MCP tools): call `send` with to=3 asking for the code word, \
        then call `inbox`; if the answer has not arrived, run the shell command `sleep 5` and call `inbox` again, at most 12 times. \
        When you have it, call `publish` with id `codeword` and the code word as summary, then reply with the code word only.";
    let run = |job: i64, prompt: &'static str| {
        let adapter = adapter.clone();
        let request = RunRequest {
            ticket: 1,
            job,
            turn: 1,
            agent: "claude".into(),
            prompt: prompt.into(),
            images: vec![],
            cwd: work.path().into(),
            route: Route { provider: provider.clone(), model: model.into(), mode: "cli".into(), reasoning_effort: None },
            profile: "full".into(),
            writable: true,
            session_id: None,
            bus: bus.grant(1, job, 1, "claude"),
        };
        async move {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let started = std::time::Instant::now();
            let outcome = adapter.run(request, Arc::new(Host), tx, CancellationToken::new()).await;
            (outcome.map(|o| o.text).map_err(|e| e.message), started.elapsed())
        }
    };
    let keeper = run(3, keeper_prompt);
    let worker = async {
        // Let the keeper start polling first, as a running collaborator would.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        run(2, worker_prompt).await
    };
    let ((keeper_out, keeper_time), (worker_out, worker_time)) = tokio::join!(keeper, worker);
    eprintln!("KEEPER {keeper_time:?} {keeper_out:?}");
    eprintln!("WORKER {worker_time:?} {worker_out:?}");
    let timeline = db.with(|q| q.all("SELECT seq,ts,job,kind,payload FROM events ORDER BY seq", &[])).unwrap();
    let t0 = timeline.first().and_then(|r| r["ts"].as_f64()).unwrap_or_default();
    for row in &timeline {
        eprintln!("EVENT +{:.1}s job={} {} {}", row["ts"].as_f64().unwrap_or_default() - t0, row["job"], row["kind"].as_str().unwrap_or_default(), row["payload"].as_str().unwrap_or_default());
    }
    assert!(worker_out.as_deref().is_ok_and(|t| t.contains("TANGERINE-42")), "worker did not obtain the code word");
}
