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
    let bus = CollabBus::start(data.path(), None).await.unwrap();
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
    let bus = CollabBus::start(data.path(), None).await.unwrap();
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

/// Engine-like host: hands unread mailbox notes to adapters that can steer
/// them into a running turn (Codex `turn/steer`) and records acceptance.
struct DbHost {
    db: Arc<Db>,
    job: i64,
}

impl TurnHost for DbHost {
    fn approve(&self, _action: Value) -> BoxFuture<'static, bool> {
        Box::pin(async { true })
    }
    fn pending_notes(&self) -> Vec<Value> {
        self.db
            .with(|q| q.notes(self.job, true))
            .unwrap_or_default()
            .into_iter()
            .filter(|n| !n["id"].as_str().is_some_and(|id| id.starts_with("bus-r-")))
            .map(|n| json!({"seq": n["seq"], "sender": n["sender"], "body": n["body"]}))
            .collect()
    }
    fn accept_notes(&self, seqs: &[i64]) {
        let who = sa_engine::db::Who { ticket: 1, id: self.job, turn: 1 };
        let _ = self.db.tx(|q| q.accept(who, seqs));
    }
}

/// `<agent>|<catalog source label or id>|<model>` or
/// `<agent>|custom=<base_url>=<KEY_ENV>|<model>`.
struct Side {
    agent: String,
    adapter: Arc<dyn Adapter>,
    provider: String,
    model: String,
}

async fn side(spec: &str) -> Side {
    let mut parts = spec.splitn(3, '|');
    let (agent, source, model) = (
        parts.next().unwrap(),
        parts.next().expect("source"),
        parts.next().expect("model"),
    );
    let mut sys = Sys::from_process();
    // This test may run inside another Claude session; keep its session wiring out.
    sys.retain(|k| {
        let k = k.to_ascii_uppercase();
        !k.starts_with("CLAUDE_CODE_") && !k.starts_with("ANTHROPIC_")
    });
    // Small-context models cannot hold the operator's global Claude MCP
    // servers, plugins and skills; the bus test needs none of them.
    if agent == "claude" && std::env::var("SA_LIVE_BUS_CLEAN_CLAUDE").is_ok() {
        let dir = std::env::temp_dir().join(format!("sa-bus-claude-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        sys.set("CLAUDE_CONFIG_DIR", dir.to_string_lossy());
    }
    let (label, customs) = match source.strip_prefix("custom=") {
        Some(rest) => {
            let (base_url, key_env) = rest.rsplit_once('=').expect("custom=<base_url>=<KEY_ENV>");
            let provider = sa_store::providers::CustomProvider {
                id: "live-bus".into(),
                agent: agent.into(),
                name: "live-bus".into(),
                base_url: base_url.into(),
                key_env: key_env.into(),
                models: vec![model.into()],
                enabled: true,
            };
            ("live-bus".to_string(), vec![provider])
        }
        None => (source.to_string(), vec![]),
    };
    let registry = sa_adapters::custom::registry(customs);
    let adapter: Arc<dyn Adapter> = match agent {
        "claude" => Arc::new(Claude::with_registry(sys, registry)),
        "codex" => Arc::new(sa_adapters::codex::Codex::with_registry(sys, registry)),
        "deepseek" => Arc::new(sa_adapters::deepseek::DeepSeek::with_registry(sys, registry)),
        "antigravity" => Arc::new(sa_adapters::antigravity::Antigravity::new(sys)),
        other => panic!("unsupported agent {other}"),
    };
    let catalog = adapter.catalog().await.unwrap().value;
    let provider = catalog["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| {
            p["id"] == label.as_str()
                || p["label"] == label.as_str()
                || p["label"].as_str().is_some_and(|l| l.starts_with(&format!("{label} ")))
        })
        .unwrap_or_else(|| panic!("source {label} not found for {agent}"))["id"]
        .as_str()
        .unwrap()
        .to_string();
    Side {
        agent: agent.into(),
        adapter,
        provider,
        model: model.into(),
    }
}

/// Billable: two real harness turns exchange a code word over the bus.
/// Opt-in: `SA_LIVE_BUS_KEEPER` and `SA_LIVE_BUS_WORKER`, each in the
/// [`side`] format, e.g. `claude|大核桃 · claude|deepseek-v4.1-flash`.
/// Prints the journal timeline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_pair_exchanges_a_code_word() {
    let keeper_prompt = "You are task #3 in a multi-agent ticket and you hold the code word TANGERINE-42. \
        Another task will ask you for it over the collaboration bus (the sa_bus MCP tools). \
        Loop: call sa_bus `inbox`; if no request arrived, run the shell command `sleep 5` and check again, at most 12 times. \
        A message may also be delivered to you directly while you work. \
        When a message asks for the code word, call sa_bus `send` with `to` set to the sender's task number and the body `CODE WORD: TANGERINE-42`, then stop and reply `delivered`.";
    let worker_prompt = "You are task #2 in a multi-agent ticket. Task #3 holds a code word you need. \
        Use the collaboration bus (the sa_bus MCP tools): call `send` with to=3 asking for the code word, \
        then call `inbox`; if the answer has not arrived, run the shell command `sleep 5` and call `inbox` again, at most 12 times. \
        A reply may also be delivered to you directly while you work. \
        When you have it, call `publish` with id `codeword` and the code word as summary, then reply with the code word only.";
    live_pair(keeper_prompt, worker_prompt).await;
}

/// Same pair, but the worker waits inside one `ask` call for the `reply`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_pair_asks_and_replies() {
    let keeper_prompt = "You are task #3 in a multi-agent ticket and you hold the code word TANGERINE-42. \
        Another task will ask you for it over the collaboration bus (the sa_bus MCP tools). \
        Loop: call sa_bus `inbox`; if no question arrived, run the shell command `sleep 3` and check again, at most 15 times. \
        A question may also be delivered to you directly while you work. \
        A question message starts with `Question <id>`. Answer it with sa_bus `reply` using that id as `question_id` and the body `CODE WORD: TANGERINE-42`, then stop and reply `delivered`.";
    let worker_prompt = "You are task #2 in a multi-agent ticket. Task #3 holds a code word you need. \
        Call the collaboration bus tool sa_bus `ask` with to=3, question `What is the code word?` and wait_seconds 50. \
        If it is not answered, call `inbox` until the answer arrives (at most 5 times). \
        Then call `publish` with id `codeword` and the code word as summary, and reply with the code word only.";
    live_pair(keeper_prompt, worker_prompt).await;
}

async fn live_pair(keeper_prompt: &'static str, worker_prompt: &'static str) {
    let (Ok(keeper), Ok(worker)) = (
        std::env::var("SA_LIVE_BUS_KEEPER"),
        std::env::var("SA_LIVE_BUS_WORKER"),
    ) else {
        eprintln!("set SA_LIVE_BUS_KEEPER and SA_LIVE_BUS_WORKER for a billable bus collaboration run");
        return;
    };
    let (keeper, worker) = (side(&keeper).await, side(&worker).await);
    let data = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(data.path()).unwrap());
    db.with(|q| {
        q.conn().execute(
            "INSERT INTO tickets(id,goal,cwd,writes,lead,created) VALUES(1,'bus live test','c',0,'claude',0)",
            [],
        )?;
        q.conn().execute(
            "INSERT INTO jobs(id,ticket,parent,agent,body,writing,depth,turn,state,role) VALUES
               (1,1,NULL,'claude','lead',0,0,1,'waiting','lead'),
               (2,1,1,?1,'worker',0,1,1,'running','worker'),
               (3,1,1,?2,'keeper',0,1,1,'running','keeper')",
            [&worker.agent, &keeper.agent],
        )?;
        Ok(())
    })
    .unwrap();
    let bus = CollabBus::start(data.path(), None).await.unwrap();
    let run = |side: &Side, job: i64, prompt: &'static str| {
        let adapter = side.adapter.clone();
        let host = Arc::new(DbHost { db: db.clone(), job });
        let request = RunRequest {
            ticket: 1,
            job,
            turn: 1,
            agent: side.agent.clone(),
            prompt: prompt.into(),
            images: vec![],
            cwd: work.path().into(),
            route: Route {
                provider: side.provider.clone(),
                model: side.model.clone(),
                mode: "cli".into(),
                reasoning_effort: None,
            },
            profile: "full".into(),
            writable: true,
            session_id: None,
            bus: bus.grant(1, job, 1, &side.agent),
        };
        async move {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let started = std::time::Instant::now();
            let outcome = adapter.run(request, host, tx, CancellationToken::new()).await;
            (outcome.map(|o| o.text).map_err(|e| e.message), started.elapsed())
        }
    };
    let keeper_run = run(&keeper, 3, keeper_prompt);
    let worker_run = async {
        // Let the keeper start polling first, as a running collaborator would.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        run(&worker, 2, worker_prompt).await
    };
    let ((keeper_out, keeper_time), (worker_out, worker_time)) = tokio::join!(keeper_run, worker_run);
    eprintln!("KEEPER {} {keeper_time:?} {keeper_out:?}", keeper.agent);
    eprintln!("WORKER {} {worker_time:?} {worker_out:?}", worker.agent);
    let timeline = db
        .with(|q| q.all("SELECT seq,ts,job,kind,payload FROM events ORDER BY seq", &[]))
        .unwrap();
    let t0 = timeline.first().and_then(|r| r["ts"].as_f64()).unwrap_or_default();
    for row in &timeline {
        eprintln!(
            "EVENT +{:.1}s job={} {} {}",
            row["ts"].as_f64().unwrap_or_default() - t0,
            row["job"],
            row["kind"].as_str().unwrap_or_default(),
            row["payload"].as_str().unwrap_or_default()
        );
    }
    assert!(
        worker_out.as_deref().is_ok_and(|t| t.contains("TANGERINE-42")),
        "worker did not obtain the code word"
    );
}

/// Messages stub that runs one slow shell command, then records whether the
/// next request carries a steered note.
fn slow_tool_model(seen: Arc<std::sync::Mutex<Vec<String>>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let seen = seen.clone();
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
                let text = String::from_utf8_lossy(&body).to_string();
                let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let names: Vec<&str> = req["tools"].as_array().into_iter().flatten().filter_map(|t| t["name"].as_str()).collect();
                let shell = ["PowerShell", "Bash"].into_iter().find(|n| names.contains(n));
                let answered = text.contains("tool_result");
                if head.starts_with("POST") && shell.is_some() {
                    seen.lock().unwrap().push(text.clone());
                }
                let model = req["model"].as_str().unwrap_or("m").to_string();
                let (kind, payload) = if head.starts_with("GET") {
                    ("application/json", json!({"data":[{"id":"claude-opus-5-5"}],"has_more":false}).to_string())
                } else {
                    let (block, delta, stop) = match (shell, answered) {
                        (Some(shell), false) => {
                            let command = if shell == "PowerShell" { "Start-Sleep -Seconds 4" } else { "sleep 4" };
                            (json!({"type":"tool_use","id":"toolu_slow","name":shell,"input":{}}),
                             json!({"type":"input_json_delta","partial_json":json!({"command":command,"description":"wait"}).to_string()}),
                             "tool_use")
                        }
                        _ => (json!({"type":"text","text":""}), json!({"type":"text_delta","text":"done"}), "end_turn"),
                    };
                    let events = [
                        json!({"type":"message_start","message":{"id":"msg_local","type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"usage":{"input_tokens":1,"output_tokens":0}}}),
                        json!({"type":"content_block_start","index":0,"content_block":block}),
                        json!({"type":"content_block_delta","index":0,"delta":delta}),
                        json!({"type":"content_block_stop","index":0}),
                        json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":{"output_tokens":1}}),
                        json!({"type":"message_stop"}),
                    ];
                    let sse: String = events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect();
                    ("text/event-stream", sse)
                };
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len());
            });
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claude_receives_a_note_while_its_tool_runs() {
    if std::env::var("SA_LOCAL_GATEWAY_CLI").is_err() {
        eprintln!("set SA_LOCAL_GATEWAY_CLI=1 to run the installed Claude CLI against loopback stubs");
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(data.path()).unwrap());
    db.with(|q| {
        q.conn().execute_batch(
            "INSERT INTO tickets(id,goal,cwd,writes,lead,created) VALUES(1,'g','c',0,'claude',0);
             INSERT INTO jobs(id,ticket,parent,agent,body,writing,depth,turn,state) VALUES
               (1,1,NULL,'codex','b',0,0,1,'waiting'),(2,1,1,'claude','b',0,1,1,'running');",
        )?;
        Ok(())
    })
    .unwrap();
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let home = tempfile::tempdir().unwrap();
    let base = format!("http://127.0.0.1:{}", slow_tool_model(seen.clone()));
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
        prompt: "run the slow command".into(),
        images: vec![],
        cwd: home.path().into(),
        route: Route { provider: provider["id"].as_str().unwrap().into(), model: "claude-opus-5-5".into(), mode: "cli".into(), reasoning_effort: None },
        profile: "full".into(),
        writable: true,
        session_id: None,
        bus: None,
    };
    let note_db = db.clone();
    let writer = tokio::spawn(async move {
        // The slow command starts after the first model response.
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        note_db
            .tx(|q| q.record_note(1, Some(&json!(2)), Some(&json!("STEERED-NOTE from the lead")), None, None))
            .unwrap();
    });
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let outcome = adapter.run(request, Arc::new(DbHost { db: db.clone(), job: 2 }), tx, CancellationToken::new()).await;
    writer.await.unwrap();
    assert!(outcome.is_ok(), "{:?}", outcome.err().map(|e| e.message));
    let requests = seen.lock().unwrap().clone();
    let after_tool: Vec<&String> = requests.iter().filter(|r| r.contains("tool_result")).collect();
    assert_eq!(after_tool.len(), 1, "one turn: the note must not start another turn");
    assert!(after_tool[0].contains("STEERED-NOTE from the lead"), "the note must join the request after the tool result");
    let unread = db.with(|q| q.notes(2, true)).unwrap();
    assert!(unread.is_empty(), "the steered note is accepted");
}

/// Billable: a worker on one harness consults an advisor on another harness
/// through the bus. The answer is only in a project file, which the read-only
/// advisor must inspect. Opt-in: `SA_LIVE_BUS_WORKER` and `SA_LIVE_BUS_ADVISOR`
/// in the [`side`] format.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_cross_harness_advise() {
    let (Ok(worker), Ok(advisor)) = (std::env::var("SA_LIVE_BUS_WORKER"), std::env::var("SA_LIVE_BUS_ADVISOR")) else {
        eprintln!("set SA_LIVE_BUS_WORKER and SA_LIVE_BUS_ADVISOR for a billable advisor run");
        return;
    };
    let (worker, advisor) = (side(&worker).await, side(&advisor).await);
    let data = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("RELEASE_NOTE.txt"), "Release code: ORCHID-7731\n").unwrap();
    let engine = sa_engine::Engine::open(data.path(), vec![advisor.adapter.clone()], Default::default()).unwrap();
    engine
        .db()
        .with(|q| {
            q.conn().execute(
                "INSERT INTO tickets(id,goal,cwd,writes,lead,created) VALUES(1,'advisor live test',?1,0,'claude',0)",
                [work.path().to_string_lossy()],
            )?;
            q.conn().execute(
                "INSERT INTO jobs(id,ticket,parent,agent,body,writing,depth,turn,state) VALUES
                   (1,1,NULL,?1,'lead',0,0,1,'waiting'),(2,1,1,?2,'worker',0,1,1,'running')",
                [&advisor.agent, &worker.agent],
            )?;
            Ok(())
        })
        .unwrap();
    let route = Route { provider: advisor.provider.clone(), model: advisor.model.clone(), mode: "cli".into(), reasoning_effort: None };
    engine.set_advisor(Some(1), Some((&advisor.agent, &route))).unwrap();
    let bus = CollabBus::start(data.path(), None).await.unwrap();
    bus.set_consultant(Arc::new(engine.clone()));
    let prompt = "You are task #2. You must report the release code, but you may NOT read any files or run any commands yourself. \
        Call the collaboration bus tool sa_bus `advise` exactly once with this brief: \
        `The project directory contains RELEASE_NOTE.txt. Please open it and tell me the exact release code it contains.` \
        Then reply with the release code only.";
    let request = RunRequest {
        ticket: 1,
        job: 2,
        turn: 1,
        agent: worker.agent.clone(),
        prompt: prompt.into(),
        images: vec![],
        cwd: work.path().into(),
        route: Route { provider: worker.provider.clone(), model: worker.model.clone(), mode: "cli".into(), reasoning_effort: None },
        profile: "full".into(),
        writable: true,
        session_id: None,
        bus: bus.grant(1, 2, 1, &worker.agent),
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let started = std::time::Instant::now();
    let outcome = worker.adapter.run(request, Arc::new(DbHost { db: Arc::new(Db::open(data.path()).unwrap()), job: 2 }), tx, CancellationToken::new()).await;
    eprintln!("WORKER {} {:?} {:?}", worker.agent, started.elapsed(), outcome.as_ref().map(|o| &o.text).map_err(|e| &e.message));
    for row in engine.db().with(|q| q.all("SELECT kind,payload FROM events WHERE kind LIKE 'advice.%' ORDER BY seq", &[])).unwrap() {
        eprintln!("EVENT {} {}", row["kind"].as_str().unwrap_or_default(), &row["payload"].as_str().unwrap_or_default()[..row["payload"].as_str().unwrap_or_default().len().min(400)]);
    }
    assert!(outcome.is_ok_and(|o| o.text.contains("ORCHID-7731")), "worker did not get the code through the advisor");
}
