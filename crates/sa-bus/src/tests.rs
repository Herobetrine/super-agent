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
    assert_eq!(names, ["roster", "send", "inbox", "tools", "call", "publish"]);
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
