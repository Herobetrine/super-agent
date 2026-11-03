//! Real MCP servers through `ProjectExtensions` (port of
//! `tests-node/extensions.test.cjs`) and one full engine turn that emits
//! `SA_TOOL`. The server is the `sa-ext-fixture-mcp` binary of this crate;
//! `SA_NODE_MCP=1` adds the official TypeScript SDK fixture
//! (`tests-node/fixtures/challenge-mcp.cjs`, needs `node` and the repo's
//! `node_modules`).

use futures::FutureExt;
use futures::future::BoxFuture;
use sa_engine::adapter::{AGENTS, Adapter, AdapterError, Catalog, EventSink, RunOutcome, RunRequest, TurnHost};
use sa_engine::services::{Extensions, InvokeArgs};
use sa_engine::{CreateParams, Engine, EngineOptions};
use sa_ext::skills::SkillHomes;
use sa_ext::{ExtOptions, ProjectExtensions};
use serde_json::{Value, json};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const FIXTURE: &str = env!("CARGO_BIN_EXE_sa-ext-fixture-mcp");
const SECRET: &str = "test-🧪-value";
/// Set in this test process only; stdio servers must not inherit it.
const TOKEN_VAR: &str = "MCP_SA_EXT_TEST_TOKEN";

type Script = Arc<dyn Fn(Value) -> String + Send + Sync>;

/// A harness that answers each turn with `script(packet)`.
struct Fake {
    name: &'static str,
    script: Script,
    packets: Arc<Mutex<Vec<Value>>>,
}

impl Adapter for Fake {
    fn agent(&self) -> &'static str {
        self.name
    }
    fn catalog(&self) -> BoxFuture<'static, anyhow::Result<Catalog>> {
        let route = json!({"provider": "test-local", "model": "default", "mode": "cli"});
        let value = json!({
            "default": route,
            "modes": [{"id": "cli", "available": true}],
            "providers": [{"id": "test-local", "models": [{"id": "default"}]}]
        });
        async move { Ok(Catalog { value }) }.boxed()
    }
    fn run(
        &self,
        req: RunRequest,
        _host: Arc<dyn TurnHost>,
        _events: EventSink,
        _cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<RunOutcome, AdapterError>> {
        let packet: Value = serde_json::from_str(req.prompt.rsplit('\n').next().unwrap()).unwrap();
        self.packets.lock().unwrap().push(packet.clone());
        let text = (self.script)(packet);
        async move {
            Ok(RunOutcome {
                text,
                session_id: Some("session".into()),
            })
        }
        .boxed()
    }
}

struct Fx {
    dir: TempDir,
    state: PathBuf,
    secret: PathBuf,
    ext: ProjectExtensions,
    engine: Engine,
    packets: Arc<Mutex<Vec<Value>>>,
}

fn fx(script: Script) -> Fx {
    let dir = tempfile::Builder::new().prefix("sa-ext-mcp-").tempdir().unwrap();
    let state = dir.path().join("state");
    let secret = dir.path().join("secret");
    std::fs::write(&secret, SECRET).unwrap();
    let opts = ExtOptions {
        homes: SkillHomes {
            home: dir.path().join("home"),
            codex_home: dir.path().join("codex"),
        },
        path: std::env::var_os("PATH"),
    };
    let ext = ProjectExtensions::open_with(&state, opts).unwrap();
    let packets: Arc<Mutex<Vec<Value>>> = Arc::default();
    let adapters = AGENTS
        .iter()
        .map(|name| {
            Arc::new(Fake {
                name,
                script: script.clone(),
                packets: packets.clone(),
            }) as Arc<dyn Adapter>
        })
        .collect();
    let opts = EngineOptions {
        extensions: Some(Arc::new(ext.clone())),
        tick: Duration::from_secs(3600),
        ..Default::default()
    };
    let engine = Engine::open(&state, adapters, opts).unwrap();
    Fx {
        dir,
        state,
        secret,
        ext,
        engine,
        packets,
    }
}

impl Fx {
    fn stdio_server(&self) -> String {
        self.ext
            .save_server(&json!({"name": "proof", "command": FIXTURE, "args": [self.secret], "enabled": true}))
            .unwrap()
    }

    fn sql(&self, sql: &str) -> Vec<serde_json::Map<String, Value>> {
        self.engine.db().with(|q| q.all(sql, &[])).unwrap()
    }

    async fn ticket(&self, profile: &str, extensions: Value) -> (i64, i64) {
        let mut params = CreateParams::new("demo", self.dir.path())
            .lead("deepseek")
            .profile(profile);
        params.extensions = Some(extensions);
        let ticket = self.engine.create(params).await.unwrap();
        let job = self.engine.root(ticket).unwrap()["id"].as_i64().unwrap();
        (ticket, job)
    }

    fn run_job(&self, job: i64) {
        self.engine
            .db()
            .with(|q| q.run("UPDATE jobs SET state='running' WHERE id=?", &[&job]))
            .unwrap();
    }

    fn calls(&self, suffix: &str) -> Option<String> {
        std::fs::read_to_string(format!("{}{suffix}", self.secret.display())).ok()
    }
}

fn args(ticket: i64, job: i64, profile: &str, approve: bool, command: Value) -> InvokeArgs {
    InvokeArgs {
        ticket,
        job,
        turn: 1,
        admission: 0,
        logical_turn: 1,
        profile: profile.into(),
        cwd: String::new(),
        cancel: CancellationToken::new(),
        approve: Arc::new(move |_| async move { approve }.boxed()),
        command,
    }
}

fn names(tools: &Value) -> Vec<&str> {
    tools
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect()
}

/// Start the HTTP fixture; returns the child and its `/mcp` URL.
fn http_fixture(secret: &Path, token: Option<&str>) -> (Child, String) {
    let mut cmd = Command::new(FIXTURE);
    cmd.arg(secret).arg("http").stdout(Stdio::piped()).stderr(Stdio::null());
    if let Some(t) = token {
        cmd.env("FIXTURE_TOKEN", t);
    }
    let mut child = cmd.spawn().unwrap();
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let port = serde_json::from_str::<Value>(&line).unwrap()["port"].as_u64().unwrap();
    (child, format!("http://127.0.0.1:{port}/mcp"))
}

fn alive(pid: u32) -> bool {
    if cfg!(windows) {
        let out = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
    } else {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn skills_stdio_and_http_transports_consent_and_revocation() {
    let f = fx(Arc::new(|_| "unused".into()));
    // SAFETY: only this test reads or writes the variable.
    unsafe { std::env::set_var(TOKEN_VAR, "sa-ext-secret-token") };
    let folder = f.dir.path().join("selected-skill");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("SKILL.md"),
        "---\nname: selected-skill\ndescription: Example\n---\nKeep exact bytes.\n",
    )
    .unwrap();
    let skill = f.ext.import_skill(&folder).unwrap();
    let server = f.stdio_server();
    // Two `tools/list` pages.
    assert_eq!(
        names(&f.ext.probe(&server).await.unwrap()),
        ["challenge", "env", "fail", "big", "sleep"]
    );
    let (ticket, job) = f.ticket("ask", json!({"skills": [skill], "servers": [server]})).await;
    f.run_job(job);
    std::fs::write(folder.join("SKILL.md"), "changed").unwrap();
    let ctx = f.ext.context(ticket, "ask");
    assert!(ctx["skills"][0]["body"].as_str().unwrap().contains("Keep exact bytes"));
    assert_eq!(ctx["mcp"][0]["server"], json!(server));
    assert_eq!(ctx["mcp"][0]["tools"].as_array().unwrap().len(), 5);
    assert_eq!(f.ext.context(ticket, "readonly")["mcp"], json!([]));
    assert_eq!(
        f.ext.summary(ticket).unwrap(),
        json!({"skills": [{"id": skill, "name": "selected-skill"}], "servers": [{"id": server, "name": "proof"}]})
    );

    let call = |id: &str, tool: &str, approve: bool, arguments: Value| {
        args(
            ticket,
            job,
            "ask",
            approve,
            json!({"id": id, "server": server, "tool": tool, "arguments": arguments}),
        )
    };
    // Denied: an `error` receipt, the tool never ran.
    assert!(
        f.ext
            .invoke(call("denied", "challenge", false, json!({})))
            .await
            .unwrap()
            .new_receipt
    );
    assert_eq!(f.ext.results(job)[0]["state"], json!("error"));
    assert_eq!(f.ext.results(job)[0]["result"], json!({"error": "用户未批准调用"}));
    assert_eq!(f.calls(".calls"), None);
    // Approved once; an identical replay is a no-op.
    assert!(
        f.ext
            .invoke(call("approved", "challenge", true, json!({})))
            .await
            .unwrap()
            .new_receipt
    );
    assert!(
        !f.ext
            .invoke(call("approved", "challenge", true, json!({})))
            .await
            .unwrap()
            .new_receipt
    );
    let err = f
        .ext
        .invoke(call("approved", "challenge", true, json!({"x": 1})))
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "MCP 幂等标识已用于不同参数");
    assert_eq!(f.calls(".calls").as_deref(), Some("1\n"));
    let last = f.ext.results(job).last().cloned().unwrap();
    assert_eq!(last["state"], json!("done"));
    assert_eq!(last["result"]["content"][0]["text"], json!(SECRET));

    // stdio servers get the SDK's safe environment only.
    f.ext
        .invoke(call("env-token", "env", true, json!({"name": TOKEN_VAR})))
        .await
        .unwrap();
    f.ext
        .invoke(call("env-path", "env", true, json!({"name": "PATH"})))
        .await
        .unwrap();
    let results = f.ext.results(job);
    let text = |id: &str| {
        results.iter().find(|r| r["id"] == json!(id)).unwrap()["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(text("env-token"), "<unset>");
    assert_ne!(text("env-path"), "<unset>");

    // isError results and oversized results.
    f.ext.invoke(call("fail", "fail", true, json!({}))).await.unwrap();
    f.ext.invoke(call("big", "big", true, json!({}))).await.unwrap();
    let results = f.ext.results(job);
    let by = |id: &str| results.iter().find(|r| r["id"] == json!(id)).unwrap().clone();
    assert_eq!(by("fail")["state"], json!("error"));
    assert_eq!(by("fail")["result"]["isError"], json!(true));
    let big = by("big");
    assert_eq!(big["state"], json!("done"));
    let object = PathBuf::from(big["result"]["object_path"].as_str().unwrap());
    assert!(object.starts_with(f.state.join("context-objects")));
    let raw = std::fs::read_to_string(&object).unwrap();
    assert_eq!(big["result"]["bytes"], json!(raw.len()));
    assert_eq!(big["result"]["sha256"], json!(sa_engine::protocol::digest(&raw)));
    assert!(raw.len() > 512 * 1024);

    // Unknown tools and readonly profiles never get a receipt.
    let err = f
        .ext
        .invoke(call("nope", "missing", true, json!({})))
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "此工具未获工单授权");
    let mut ro = call("ro", "challenge", true, json!({}));
    ro.profile = "readonly".into();
    assert_eq!(
        f.ext.invoke(ro).await.unwrap_err().to_string(),
        "当前权限不允许 MCP 调用"
    );

    // Every receipt journaled `mcp.result`; issued calls also `mcp.started`.
    let events = f.sql("SELECT kind,payload FROM events WHERE kind LIKE 'mcp.%' ORDER BY seq");
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds.iter().filter(|k| **k == "mcp.result").count(), 6);
    assert_eq!(kinds.iter().filter(|k| **k == "mcp.started").count(), 5);
    let journal: String = events.iter().map(|e| e["payload"].as_str().unwrap()).collect();
    assert!(!journal.contains("sa-ext-secret-token"));

    // Disabling and re-enabling issues a new revision: the binding is revoked.
    let config = json!({"id": server, "name": "proof", "command": FIXTURE, "args": [f.secret], "enabled": false});
    f.ext.save_server(&config).unwrap();
    let mut on = config.clone();
    on["enabled"] = json!(true);
    f.ext.save_server(&on).unwrap();
    assert_eq!(f.ext.context(ticket, "ask")["mcp"], json!([]));
    assert_eq!(
        f.ext.results(job).len(),
        6,
        "receipts of the latest logical turn are all kept"
    );

    // Streamable HTTP on loopback.
    let (mut plain, url) = http_fixture(&f.secret, None);
    let remote = f
        .ext
        .save_server(&json!({"name": "HTTP local", "transport": "http", "url": url, "enabled": true}))
        .unwrap();
    assert_eq!(names(&f.ext.probe(&remote).await.unwrap())[0], "challenge");
    let (t2, j2) = f.ticket("full", json!({"skills": [], "servers": [remote]})).await;
    f.run_job(j2);
    let http_call = args(
        t2,
        j2,
        "full",
        false,
        json!({"id": "h", "server": remote, "tool": "challenge", "arguments": {}}),
    );
    assert!(f.ext.invoke(http_call).await.unwrap().new_receipt);
    assert_eq!(f.ext.results(j2)[0]["result"]["content"][0]["text"], json!(SECRET));
    plain.kill().unwrap();

    // Bearer token from the configured `MCP_*` variable only.
    let (mut guarded, url) = http_fixture(&f.secret, Some("sa-ext-secret-token"));
    let unset = f
        .ext
        .save_server(
            &json!({"name": "unset", "transport": "http", "url": url, "tokenEnv": "MCP_SA_EXT_UNSET", "enabled": true}),
        )
        .unwrap();
    assert_eq!(
        f.ext.probe(&unset).await.unwrap_err().to_string(),
        "未设置此 MCP 服务的令牌环境变量"
    );
    let anonymous = f
        .ext
        .save_server(&json!({"name": "anonymous", "transport": "http", "url": url, "enabled": true}))
        .unwrap();
    assert!(f.ext.probe(&anonymous).await.is_err());
    let token = f
        .ext
        .save_server(&json!({"name": "token", "transport": "http", "url": url, "tokenEnv": TOKEN_VAR, "enabled": true}))
        .unwrap();
    assert_eq!(names(&f.ext.probe(&token).await.unwrap()).len(), 5);
    guarded.kill().unwrap();
    let listed = f.ext.list(None).unwrap();
    assert!(!listed.to_string().contains("sa-ext-secret-token"));

    let err = f
        .ext
        .save_server(&json!({"name": "bad", "transport": "http", "url": "http://remote.invalid/mcp", "enabled": true}))
        .unwrap_err();
    assert_eq!(err.to_string(), sa_ext::validate::BAD_ENDPOINT);
    f.engine.close().await;
    f.ext.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sa_tool_from_a_turn_writes_a_receipt_and_takes_another_turn() {
    let call = r#"SA_TOOL {"id":"calc","server":"SERVER","tool":"challenge","arguments":{}}"#;
    let server_id: Arc<Mutex<String>> = Arc::default();
    let sid = server_id.clone();
    let f = fx(Arc::new(move |packet| {
        if packet["turn"] == json!(1) {
            let server = sid.lock().unwrap().clone();
            assert_eq!(packet["project_extensions"]["mcp"][0]["server"], json!(server));
            assert_eq!(packet["mcp_results"], json!([]));
            return format!("checking\n{}", call.replace("SERVER", &server));
        }
        "the code is known".into()
    }));
    let server = f.stdio_server();
    *server_id.lock().unwrap() = server.clone();
    f.ext.probe(&server).await.unwrap();
    let (ticket, job) = f.ticket("full", json!({"skills": [], "servers": [server]})).await;
    f.engine.start().unwrap();
    let end = Instant::now() + Duration::from_secs(30);
    while f.engine.root(ticket).unwrap()["state"] != json!("done") {
        assert!(
            Instant::now() < end,
            "timed out: {:?}",
            f.sql("SELECT id,turn,state,result FROM jobs")
        );
        assert!(f.engine.fault().is_none());
        f.engine.step();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let root = f.engine.root(ticket).unwrap();
    assert_eq!(root["turn"], json!(2));
    let packets = f.packets.lock().unwrap().clone();
    assert_eq!(packets.len(), 2);
    let expected = json!([{"id": "calc", "state": "done", "result": {"content": [{"type": "text", "text": SECRET}]}}]);
    assert_eq!(packets[1]["mcp_results"], expected);
    let receipts = f.sql("SELECT job,logical,key,request,state,result FROM studio_tool_receipts");
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0]["job"], json!(job));
    assert_eq!(receipts[0]["logical"], json!(1));
    assert_eq!(receipts[0]["state"], json!("done"));
    assert_eq!(
        receipts[0]["request"],
        json!(format!(
            r#"{{"server":"{server}","tool":"challenge","arguments":{{}}}}"#
        ))
    );
    let events = f.sql("SELECT kind,turn,payload FROM events WHERE kind LIKE 'mcp.%' ORDER BY seq");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["kind"], json!("mcp.started"));
    assert_eq!(events[1]["kind"], json!("mcp.result"));
    assert_eq!(events[1]["turn"], json!(1));
    let payload: Value = serde_json::from_str(events[1]["payload"].as_str().unwrap()).unwrap();
    assert_eq!(
        payload,
        json!({"id": "calc", "server": server, "tool": "challenge", "state": "done",
            "result": {"content": [{"type": "text", "text": SECRET}]}})
    );
    assert_eq!(f.calls(".calls").as_deref(), Some("1\n"));
    f.engine.close().await;
    f.ext.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_an_issued_call_is_uncertain_and_stops_the_server() {
    let f = fx(Arc::new(|_| "unused".into()));
    let server = f.stdio_server();
    f.ext.probe(&server).await.unwrap();
    let (ticket, job) = f.ticket("full", json!({"skills": [], "servers": [server]})).await;
    f.run_job(job);
    let a = args(
        ticket,
        job,
        "full",
        false,
        json!({"id": "slow", "server": server, "tool": "sleep", "arguments": {}}),
    );
    let cancel = a.cancel.clone();
    let ext = f.ext.clone();
    let task = tokio::spawn(async move { ext.invoke(a).await });
    let end = Instant::now() + Duration::from_secs(20);
    while f.calls(".sleeps").is_none() {
        assert!(Instant::now() < end, "tool never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let pid: u32 = std::fs::read_to_string(format!("{}.pid", f.secret.display()))
        .unwrap()
        .parse()
        .unwrap();
    assert!(alive(pid));
    let started = Instant::now();
    cancel.cancel();
    assert!(task.await.unwrap().unwrap().new_receipt);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "cancel must not wait for the tool"
    );
    let r = &f.ext.results(job)[0];
    assert_eq!(r["state"], json!("uncertain"));
    assert_eq!(r["result"], json!({"error": "操作已取消"}));
    let end = Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(Instant::now() < end, "MCP server {pid} survived cancellation");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // A stale turn cannot start a new call.
    f.engine
        .db()
        .with(|q| q.run("UPDATE jobs SET state='cancelled' WHERE id=?", &[&job]))
        .unwrap();
    let stale = args(
        ticket,
        job,
        "full",
        false,
        json!({"id": "late", "server": server, "tool": "challenge", "arguments": {}}),
    );
    assert_eq!(f.ext.invoke(stale).await.unwrap_err().to_string(), "旧轮次已停止");
    f.engine.close().await;
    f.ext.close();
}

/// Interop with the official TypeScript SDK server (opt-in).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typescript_sdk_stdio_server() {
    if std::env::var("SA_NODE_MCP").as_deref() != Ok("1") {
        eprintln!("skipped: set SA_NODE_MCP=1 to run against tests-node/fixtures/challenge-mcp.cjs");
        return;
    }
    let f = fx(Arc::new(|_| "unused".into()));
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests-node/fixtures/challenge-mcp.cjs");
    let script = sa_engine::services::realpath(&script).unwrap();
    let server = f
        .ext
        .save_server(&json!({"name": "node", "command": "node", "args": [script, f.secret], "enabled": true}))
        .unwrap();
    assert_eq!(names(&f.ext.probe(&server).await.unwrap()), ["challenge"]);
    let (ticket, job) = f.ticket("full", json!({"skills": [], "servers": [server]})).await;
    f.run_job(job);
    let a = args(
        ticket,
        job,
        "full",
        false,
        json!({"id": "ts", "server": server, "tool": "challenge", "arguments": {}}),
    );
    assert!(f.ext.invoke(a).await.unwrap().new_receipt);
    let r = &f.ext.results(job)[0];
    assert_eq!(r["state"], json!("done"), "{r}");
    assert_eq!(r["result"]["content"][0]["text"], json!(SECRET));
    assert_eq!(f.calls(".calls").as_deref(), Some("1\n"));
    f.engine.close().await;
    f.ext.close();
}
