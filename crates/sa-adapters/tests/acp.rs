use futures::future::BoxFuture;
use sa_adapters::{acp::Acp, sys::Sys};
use sa_engine::adapter::{Adapter, AdapterEvent, ErrorClass, Route, RunRequest, TurnHost};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

struct Host {
    allow: bool,
    calls: Arc<AtomicUsize>,
}
impl TurnHost for Host {
    fn approve(&self, _: Value) -> BoxFuture<'static, bool> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let yes = self.allow;
        Box::pin(async move { yes })
    }
    fn pending_notes(&self) -> Vec<Value> {
        vec![]
    }
    fn accept_notes(&self, _: &[i64]) {}
}
fn setup(mode: &str, restricted: bool) -> (tempfile::TempDir, Sys, RunRequest) {
    let dir = tempfile::tempdir().unwrap();
    let mut sys = Sys::from_process();
    let node = sys
        .which(if cfg!(windows) { "node.exe" } else { "node" })
        .expect("Node required for ACP stdio fixture");
    let config = dir.path().join("acp.json");
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp-agent.cjs");
    std::fs::write(&config,serde_json::to_vec(&json!({"servers":[{"id":"fixture","command":node,"args":[fixture,mode,dir.path().join("wire.jsonl")],"models":["m1"],"model_config_id":"model","trusted_sandbox":restricted,"permission_modes":if restricted{json!({"ask":"safe","readonly":"safe"})}else{json!({})}}]})).unwrap()).unwrap();
    sys.set("SUPERAGENT_ACP_CONFIG", config.to_string_lossy());
    let req = RunRequest {
        ticket: 1,
        job: 1,
        turn: 1,
        agent: "acp".into(),
        prompt: "fixture".into(),
        images: vec![],
        cwd: dir.path().to_owned(),
        route: Route {
            provider: "fixture".into(),
            model: "m1".into(),
            mode: "cli".into(),
            reasoning_effort: None,
        },
        profile: "full".into(),
        writable: true,
        session_id: None,
    };
    (dir, sys, req)
}
async fn execute(
    sys: Sys,
    req: RunRequest,
    allow: bool,
    cancel: CancellationToken,
) -> (
    Result<sa_engine::adapter::RunOutcome, sa_engine::adapter::AdapterError>,
    Vec<AdapterEvent>,
    usize,
) {
    let adapter = Acp::new(sys);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        adapter.run(
            req,
            Arc::new(Host {
                allow,
                calls: calls.clone(),
            }),
            tx,
            cancel,
        ),
    )
    .await
    .expect("ACP fixture must finish");
    let mut events = vec![];
    while let Ok(e) = rx.try_recv() {
        events.push(e)
    }
    (out, events, calls.load(Ordering::SeqCst))
}
#[tokio::test]
async fn actual_stdio_new_load_stream_permission_and_model_selection() {
    let (dir, sys, req) = setup("normal", false);
    let cat = Acp::new(sys.clone()).catalog().await.unwrap();
    assert_eq!(cat.value["default"]["provider"], "fixture");
    let (out, events, calls) =
        execute(sys.clone(), req.clone(), false, CancellationToken::new()).await;
    let out = out.unwrap();
    assert_eq!(out.text, "精确结果\n");
    assert_eq!(calls, 0);
    assert!(
        events
            .iter()
            .any(|e| matches!(e,AdapterEvent::ToolStarted(id) if id=="tool-1"))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e,AdapterEvent::ToolFinished(id) if id=="tool-1"))
    );
    let mut resumed = req;
    resumed.session_id = out.session_id.clone();
    let (two, _, _) = execute(sys, resumed, false, CancellationToken::new()).await;
    let two = two.unwrap();
    assert_eq!(two.text, "精确结果\n");
    assert_eq!(two.session_id, out.session_id);
    let wire = std::fs::read_to_string(dir.path().join("wire.jsonl")).unwrap();
    assert!(wire.contains("session/load"));
    assert!(wire.contains("session/set_config_option"));
    assert!(wire.contains("\"optionId\":\"once\""));
    assert!(!wire.contains("\"optionId\":\"always\""));
}
#[tokio::test]
async fn restricted_runs_fail_closed_and_verified_ask_uses_host() {
    let (_dir, sys, mut req) = setup("normal", false);
    req.profile = "readonly".into();
    req.writable = false;
    assert_eq!(
        execute(sys, req, false, CancellationToken::new())
            .await
            .0
            .unwrap_err()
            .class,
        ErrorClass::Permission
    );
    let (dir, sys, mut req) = setup("normal", false);
    req.writable = false;
    assert_eq!(
        execute(sys, req, false, CancellationToken::new())
            .await
            .0
            .unwrap_err()
            .class,
        ErrorClass::Permission
    );
    assert!(
        !dir.path().join("wire.jsonl").exists(),
        "conflicting grant must fail before spawning"
    );
    let (dir, sys, mut req) = setup("normal", true);
    req.profile = "partial".into();
    req.writable = false;
    let (result, _, calls) = execute(sys, req, true, CancellationToken::new()).await;
    assert!(result.is_ok());
    assert_eq!(calls, 0);
    let wire = std::fs::read_to_string(dir.path().join("wire.jsonl")).unwrap();
    assert!(wire.contains("\"optionId\":\"deny\""));
    for allow in [false, true] {
        let (dir, sys, mut req) = setup("normal", true);
        req.profile = "ask".into();
        let (out, _, calls) = execute(sys, req, allow, CancellationToken::new()).await;
        assert!(out.is_ok());
        assert_eq!(calls, 1);
        let wire = std::fs::read_to_string(dir.path().join("wire.jsonl")).unwrap();
        assert!(wire.contains(if allow {
            "\"optionId\":\"once\""
        } else {
            "\"optionId\":\"deny\""
        }));
        assert!(wire.contains("session/set_mode"));
    }
}
#[tokio::test]
async fn no_resume_capability_and_incomplete_terminal_are_failures() {
    for mode in ["no-load", "incomplete", "wrong-session"] {
        let (_dir, sys, mut req) = setup(mode, false);
        if mode == "no-load" {
            req.session_id = Some("fixture-session".into());
        }
        assert!(
            execute(sys, req, false, CancellationToken::new())
                .await
                .0
                .is_err(),
            "{mode}"
        );
    }
}
#[tokio::test]
async fn cancellation_sends_native_notification_and_reaps_process() {
    let (dir, sys, req) = setup("wait", false);
    let cancel = CancellationToken::new();
    let fire = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        fire.cancel();
    });
    assert_eq!(
        execute(sys, req, false, cancel).await.0.unwrap_err().class,
        ErrorClass::Cancelled
    );
    let wire = std::fs::read_to_string(dir.path().join("wire.jsonl")).unwrap();
    assert!(wire.contains("session/cancel"));
}
