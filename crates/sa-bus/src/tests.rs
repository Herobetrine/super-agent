use super::*;
use sa_engine::services::Bus;

struct Fx {
    dir: tempfile::TempDir,
    db: Db,
}

/// Ticket 1: lead #1 (claude, waiting), worker #2 (codex, running, turn 3),
/// reviewer #3 (antigravity, running). Ticket 2: lead #4.
fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).unwrap();
    db.with(|q| {
        q.conn().execute_batch(
            "INSERT INTO tickets(id,goal,cwd,writes,lead,created) VALUES(1,'g','c',0,'claude',0),(2,'g','c',0,'codex',0);
             INSERT INTO jobs(id,ticket,parent,agent,body,writing,depth,turn,state,role) VALUES
               (1,1,NULL,'claude','b',0,0,1,'waiting','lead'),
               (2,1,1,'codex','b',1,1,3,'running','writer'),
               (3,1,1,'antigravity','b',0,1,1,'running','reviewer'),
               (4,2,NULL,'codex','b',0,0,1,'running','');",
        )?;
        Ok(())
    })
    .unwrap();
    Fx { dir, db }
}

fn caller(job: i64, turn: i64, agent: &str, ticket: i64) -> Grantee {
    Grantee {
        ticket,
        job,
        turn,
        agent: agent.into(),
    }
}

fn run(fx: &Fx, who: &Grantee, tool: &str, args: Value) -> Result<Value> {
    fx.db.tx(|q| call(q, who, tool, &args))
}

#[test]
fn send_reaches_parent_and_peer_with_sender_identity() {
    let fx = fixture();
    let worker = caller(2, 3, "codex", 1);
    let reviewer = caller(3, 1, "antigravity", 1);
    let sent = run(&fx, &worker, "send", json!({"to":"lead","body":"commit abc ready"})).unwrap();
    assert_eq!(sent["to"], 1);
    run(&fx, &worker, "send", json!({"to":3,"body":"please check frame 12"})).unwrap();
    let inbox = run(&fx, &reviewer, "inbox", json!({})).unwrap();
    assert_eq!(inbox["messages"][0]["from"], "codex (#2)");
    assert_eq!(inbox["messages"][0]["body"], "please check frame 12");
    assert_eq!(inbox["unread"], 0);
    // The waiting lead's mail is durable and counted until it reads it.
    let lead_unread = fx.db.with(|q| q.int("SELECT count(*) FROM mailbox WHERE target=1 AND consumed_turn IS NULL", &[])).unwrap();
    assert_eq!(lead_unread, 1);
    assert_eq!(run(&fx, &worker, "roster", json!({})).unwrap()["unread"], 0);
}

#[test]
fn unread_count_rides_on_every_result() {
    let fx = fixture();
    let worker = caller(2, 3, "codex", 1);
    let reviewer = caller(3, 1, "antigravity", 1);
    run(&fx, &reviewer, "send", json!({"to":"2","body":"one"})).unwrap();
    run(&fx, &reviewer, "send", json!({"to":2,"body":"two"})).unwrap();
    assert_eq!(run(&fx, &worker, "roster", json!({})).unwrap()["unread"], 2);
    let inbox = run(&fx, &worker, "inbox", json!({})).unwrap();
    assert_eq!(inbox["messages"].as_array().unwrap().len(), 2);
    assert_eq!(run(&fx, &worker, "inbox", json!({})).unwrap()["messages"], json!([]));
}

#[test]
fn other_tickets_self_and_ended_turns_are_rejected() {
    let fx = fixture();
    let worker = caller(2, 3, "codex", 1);
    assert!(run(&fx, &worker, "send", json!({"to":4,"body":"x"})).is_err());
    assert!(run(&fx, &worker, "send", json!({"to":2,"body":"x"})).is_err());
    assert!(run(&fx, &worker, "send", json!({"to":"nobody","body":"x"})).is_err());
    assert!(run(&fx, &worker, "send", json!({"to":3,"body":""})).is_err());
    let stale = caller(2, 2, "codex", 1);
    assert!(run(&fx, &stale, "roster", json!({})).is_err(), "an older turn's token must not act");
    let lead = caller(1, 1, "claude", 1);
    assert!(run(&fx, &lead, "inbox", json!({})).is_err(), "a waiting job is not live");
}

#[test]
fn roster_lists_ticket_tasks_and_published_progress() {
    let fx = fixture();
    let worker = caller(2, 3, "codex", 1);
    run(&fx, &worker, "publish", json!({"id":"tests","summary":"412 passed"})).unwrap();
    assert_eq!(run(&fx, &worker, "publish", json!({"id":"tests","summary":"412 passed"})).unwrap()["duplicate"], true);
    assert!(run(&fx, &worker, "publish", json!({"id":"tests","summary":"changed"})).is_err());
    assert!(run(&fx, &worker, "publish", json!({"id":"x","summary":"two\nlines"})).is_err());
    let roster = run(&fx, &caller(3, 1, "antigravity", 1), "roster", json!({})).unwrap();
    let tasks = roster["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 3);
    assert_eq!(tasks[1]["latest_progress"], "412 passed");
    assert_eq!(tasks[1]["role"], "writer");
    assert_eq!(tasks[2]["you"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_round_trip_requires_a_live_grant() {
    use rmcp::ServiceExt;
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    let fx = fixture();
    let bus = CollabBus::start(fx.dir.path(), None).await.unwrap();
    let grant = bus.grant(1, 2, 3, "codex").unwrap();
    let connect = |token: String| {
        let config = StreamableHttpClientTransportConfig::with_uri(bus.url().to_string()).auth_header(token);
        ().serve(StreamableHttpClientTransport::from_config(config))
    };
    let client = connect(grant.token.clone()).await.unwrap();
    assert!(client.peer_info().unwrap().instructions.as_deref().unwrap().contains("collaboration bus"));
    let names: Vec<String> = client.list_all_tools().await.unwrap().into_iter().map(|t| t.name.to_string()).collect();
    assert_eq!(names, ["roster", "send", "advise", "ask", "reply", "inbox", "claim", "release", "tools", "call", "publish"]);
    let call = |name: &'static str, args: Value| {
        let mut params = rmcp::model::CallToolRequestParams::new(name);
        params.arguments = args.as_object().cloned();
        client.call_tool(params)
    };
    let sent = call("send", json!({"to":3,"body":"hello"})).await.unwrap();
    assert_eq!(sent.is_error, Some(false));
    let note = fx.db.with(|q| q.notes(3, true)).unwrap();
    assert_eq!(note[0]["body"], "hello");
    bus.revoke(&grant.token);
    let denied = call("roster", json!({})).await;
    assert!(denied.is_err(), "a revoked token must be refused");
    client.cancel().await.unwrap();
}

/// The bus adds tool definitions and instructions to every harness's prompt
/// prefix. They must be identical across turns, grants and tickets, or each
/// turn would miss the provider prompt cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_visible_definitions_are_byte_stable_across_grants() {
    use rmcp::ServiceExt;
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    let fx = fixture();
    let bus = CollabBus::start(fx.dir.path(), None).await.unwrap();
    let mut seen = vec![];
    for (ticket, job, turn, agent) in [(1, 2, 3, "codex"), (1, 3, 1, "antigravity"), (2, 4, 1, "codex")] {
        let grant = bus.grant(ticket, job, turn, agent).unwrap();
        let config = StreamableHttpClientTransportConfig::with_uri(bus.url().to_string()).auth_header(grant.token.clone());
        let client = ().serve(StreamableHttpClientTransport::from_config(config)).await.unwrap();
        let tools = serde_json::to_string(&client.list_all_tools().await.unwrap()).unwrap();
        let instructions = client.peer_info().unwrap().instructions.clone().unwrap();
        assert!(!tools.contains(&grant.token) && !instructions.contains(&grant.token));
        seen.push((tools, instructions));
        client.cancel().await.unwrap();
    }
    assert!(seen.windows(2).all(|w| w[0] == w[1]), "definitions must not vary per grant");
}

async fn client(bus: &CollabBus, token: &str) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    use rmcp::ServiceExt;
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    let config = StreamableHttpClientTransportConfig::with_uri(bus.url().to_string()).auth_header(token.to_string());
    ().serve(StreamableHttpClientTransport::from_config(config)).await.unwrap()
}

async fn tool(c: &rmcp::service::RunningService<rmcp::RoleClient, ()>, name: &'static str, args: Value) -> Value {
    let mut params = rmcp::model::CallToolRequestParams::new(name);
    params.arguments = args.as_object().cloned();
    let result = c.call_tool(params).await.unwrap();
    let text = result.content[0].as_text().map(|t| t.text.clone()).unwrap_or_default();
    json!({"error": result.is_error == Some(true), "value": serde_json::from_str::<Value>(&text).unwrap_or(json!(text))})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ask_waits_for_the_reply_and_refuses_a_reverse_wait() {
    let fx = fixture();
    let bus = CollabBus::start(fx.dir.path(), None).await.unwrap();
    let worker = client(&bus, &bus.grant(1, 2, 3, "codex").unwrap().token).await;
    let reviewer = client(&bus, &bus.grant(1, 3, 1, "antigravity").unwrap().token).await;
    let asking = tokio::spawn(async move {
        let answer = tool(&worker, "ask", json!({"to": 3, "question": "which frame flickers?", "wait_seconds": 20})).await;
        (worker, answer)
    });
    // The question arrives like any message, with its id.
    let mut question = None;
    for _ in 0..40 {
        let inbox = tool(&reviewer, "inbox", json!({})).await;
        if let Some(body) = inbox["value"]["messages"][0]["body"].as_str() {
            question = body.split_whitespace().nth(1).map(str::to_string);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let question = question.expect("question delivered");
    // A reverse wait while #2 waits on #3 would deadlock both turns.
    let reverse = tool(&reviewer, "ask", json!({"to": 2, "question": "and you?", "wait_seconds": 1})).await;
    assert!(reverse["error"] == true && reverse["value"].as_str().unwrap().contains("互相等待"));
    let replied = tool(&reviewer, "reply", json!({"question_id": question, "body": "frame 12"})).await;
    assert_eq!(replied["value"]["to"], 2);
    let (worker, answer) = asking.await.unwrap();
    assert_eq!(answer["value"]["answered"], true);
    assert_eq!(answer["value"]["answer"], "frame 12");
    assert_eq!(answer["value"]["from"], "antigravity (#3)");
    // Delivered through `ask`, so it is not repeated in the inbox.
    assert_eq!(tool(&worker, "inbox", json!({})).await["value"]["messages"], json!([]));
    assert!(tool(&reviewer, "reply", json!({"question_id": "q0", "body": "x"})).await["error"] == true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unanswered_ask_times_out_and_the_late_reply_reaches_the_inbox() {
    let fx = fixture();
    let bus = CollabBus::start(fx.dir.path(), None).await.unwrap();
    let worker = client(&bus, &bus.grant(1, 2, 3, "codex").unwrap().token).await;
    let reviewer = client(&bus, &bus.grant(1, 3, 1, "antigravity").unwrap().token).await;
    let answer = tool(&worker, "ask", json!({"to": 3, "question": "late?", "wait_seconds": 1})).await;
    assert_eq!(answer["value"]["answered"], false);
    let question = answer["value"]["question_id"].as_str().unwrap().to_string();
    tool(&reviewer, "reply", json!({"question_id": question, "body": "yes"})).await;
    let inbox = tool(&worker, "inbox", json!({})).await;
    assert_eq!(inbox["value"]["messages"][0]["body"], "yes");
    // After the timeout the asker no longer blocks a question in the other direction.
    let reverse = tool(&reviewer, "ask", json!({"to": 2, "question": "free now?", "wait_seconds": 1})).await;
    assert_eq!(reverse["value"]["answered"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claims_refuse_overlaps_notify_the_holder_and_end_with_the_turn() {
    let fx = fixture();
    let bus = CollabBus::start(fx.dir.path(), None).await.unwrap();
    let worker_grant = bus.grant(1, 2, 3, "codex").unwrap();
    let worker = client(&bus, &worker_grant.token).await;
    let reviewer = client(&bus, &bus.grant(1, 3, 1, "antigravity").unwrap().token).await;
    let taken = tool(&worker, "claim", json!({"paths": ["src\\fx", "docs/a.md"]})).await;
    assert_eq!(taken["value"]["claimed"], json!(["src/fx", "docs/a.md"]));
    // A file below a claimed directory overlaps; an unrelated path does not.
    let wanted = tool(&reviewer, "claim", json!({"paths": ["src/fx/Shader.java", "README.md"]})).await;
    assert_eq!(wanted["value"]["claimed"], json!(["README.md"]));
    assert_eq!(wanted["value"]["conflicts"][0]["held_by"], 2);
    let notified = tool(&worker, "inbox", json!({})).await;
    assert!(notified["value"]["messages"][0]["body"].as_str().unwrap().contains("src/fx/Shader.java"));
    let roster = tool(&reviewer, "roster", json!({})).await;
    assert_eq!(roster["value"]["tasks"][1]["claims"], json!(["src/fx", "docs/a.md"]));
    assert!(tool(&worker, "claim", json!({"paths": ["../outside"]})).await["error"] == true);
    // Releasing one path frees it for others.
    tool(&worker, "release", json!({"paths": ["src/fx"]})).await;
    let retry = tool(&reviewer, "claim", json!({"paths": ["src/fx/Shader.java"]})).await;
    assert_eq!(retry["value"]["conflicts"], json!([]));
    // The worker's remaining claim ends with its turn.
    bus.revoke(&worker_grant.token);
    let after = tool(&reviewer, "claim", json!({"paths": ["docs"]})).await;
    assert_eq!(after["value"]["conflicts"], json!([]));
}

struct EchoConsultant;

impl sa_engine::services::Consultant for EchoConsultant {
    fn consult(&self, ticket: i64, job: i64, _turn: i64, brief: String, _cancel: CancellationToken) -> futures_util_box::BoxFuture<Result<Value>> {
        Box::pin(async move { Ok(json!({"advisor": {"agent": "claude"}, "advice": format!("#{job}@{ticket}: {brief}")})) })
    }
}

mod futures_util_box {
    pub type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'static>>;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn advise_goes_to_the_consultant_with_the_callers_identity() {
    let fx = fixture();
    let bus = CollabBus::start(fx.dir.path(), None).await.unwrap();
    let worker = client(&bus, &bus.grant(1, 2, 3, "codex").unwrap().token).await;
    let missing = tool(&worker, "advise", json!({"brief": "help"})).await;
    assert!(missing["error"] == true, "no consultant yet");
    bus.set_consultant(Arc::new(EchoConsultant));
    let advice = tool(&worker, "advise", json!({"brief": "which approach?"})).await;
    assert_eq!(advice["value"]["advice"], "#2@1: which approach?");
    assert_eq!(advice["value"]["unread"], 0);
}
