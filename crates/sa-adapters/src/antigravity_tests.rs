use super::*;
use sa_engine::{
    adapter::{AdapterEvent, Route},
    images::ImageInput,
};
use std::sync::atomic::{AtomicUsize, Ordering};

struct DenyHost(Arc<AtomicUsize>);
impl TurnHost for DenyHost {
    fn approve(&self, _: Value) -> BoxFuture<'static, bool> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { false })
    }
    fn pending_notes(&self) -> Vec<Value> {
        vec![]
    }
    fn accept_notes(&self, _: &[i64]) {}
}

fn setup(scenario: &str) -> (tempfile::TempDir, Sys, Server, RunRequest) {
    let dir = tempfile::tempdir().unwrap();
    let sys = Sys::from_process();
    let node = sys
        .which(if cfg!(windows) { "node.exe" } else { "node" })
        .unwrap();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/antigravity-acp.cjs");
    let server = Server {
        id: "google".into(),
        label: "Antigravity".into(),
        command: node.to_string_lossy().into_owned(),
        args: vec![
            fixture.to_string_lossy().into_owned(),
            scenario.into(),
            dir.path().join("wire.jsonl").to_string_lossy().into_owned(),
        ],
        env: vec![],
        models: vec![],
        model_config_id: None,
        trusted_sandbox: true,
        permission_modes: [("readonly".into(), "default".into())].into(),
    };
    let png = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=").unwrap();
    let req = RunRequest {
        ticket: 1,
        job: 1,
        turn: 1,
        agent: "antigravity".into(),
        prompt: "审阅图片".into(),
        images: vec![ImageInput::from_bytes("图片.PNG".into(), png).unwrap()],
        cwd: dir.path().into(),
        route: Route {
            provider: "google".into(),
            model: "gemini-fixture-low".into(),
            mode: "cli".into(),
            reasoning_effort: Some("low".into()),
        },
        profile: "readonly".into(),
        writable: false,
        session_id: None,
    };
    (dir, sys, server, req)
}

async fn execute(
    sys: Sys,
    server: Server,
    req: RunRequest,
    cancel: CancellationToken,
) -> (Result<RunOutcome, AdapterError>, Vec<AdapterEvent>, usize) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run_server(
            sys,
            req,
            Arc::new(DenyHost(calls.clone())),
            Emit(tx),
            cancel,
            server,
            true,
        ),
    )
    .await
    .unwrap();
    let mut events = vec![];
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    (result, events, calls.load(Ordering::SeqCst))
}

#[tokio::test]
async fn antigravity_images_round_trip_bytes_and_resume_with_route_readback() {
    let (dir, sys, server, mut req) = setup("normal");
    for resumed in [false, true] {
        if resumed {
            req.session_id = Some("agy-fixture-session".into());
        }
        let (out, events, calls) = execute(
            sys.clone(),
            server.clone(),
            req.clone(),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(out.unwrap().text, "中途内容\n图片完成\n");
        assert_eq!(calls, 0);
        assert!(events.iter().any(|e| matches!(e, AdapterEvent::Journal{kind,data} if kind=="adapter.route" && data["agent"]=="antigravity")));
    }
    let wire = std::fs::read_to_string(dir.path().join("wire.jsonl")).unwrap();
    let messages: Vec<Value> = wire
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let prompts: Vec<_> = messages
        .iter()
        .filter(|m| m["method"] == "session/prompt")
        .collect();
    assert_eq!(prompts.len(), 2);
    for prompt in prompts {
        let image = &prompt["params"]["prompt"][1];
        assert_eq!(image["type"], "image");
        assert_eq!(image["mimeType"], "image/png");
        assert_eq!(
            STANDARD.decode(image["data"].as_str().unwrap()).unwrap(),
            req.images[0].data()
        );
        assert!(image["uri"].is_null());
    }
    assert!(wire.contains("\"optionId\":\"deny\""));
    assert!(wire.contains("session/set_model"));
    assert!(wire.contains("thought_level"));
}

#[tokio::test]
async fn antigravity_rejects_capability_route_and_effort_mismatches_before_prompt() {
    for scenario in [
        "no-image",
        "wrong-source",
        "wrong-model",
        "wrong-effort",
        "wrong-load",
        "no-load",
    ] {
        let (dir, sys, server, mut req) = setup(scenario);
        if matches!(scenario, "wrong-load" | "no-load") {
            req.session_id = Some("expected-session".into());
        }
        assert!(
            execute(sys, server, req, CancellationToken::new())
                .await
                .0
                .is_err(),
            "{scenario}"
        );
        assert!(
            !std::fs::read_to_string(dir.path().join("wire.jsonl"))
                .unwrap()
                .contains("session/prompt"),
            "{scenario}"
        );
    }
    let (dir, sys, server, mut req) = setup("normal");
    req.route.reasoning_effort = Some("max".into());
    assert!(
        execute(sys, server, req, CancellationToken::new())
            .await
            .0
            .is_err()
    );
    assert!(
        !std::fs::read_to_string(dir.path().join("wire.jsonl"))
            .unwrap()
            .contains("session/prompt")
    );
}

#[tokio::test]
async fn antigravity_partial_text_never_completes_failed_review() {
    for scenario in ["overloaded", "incomplete", "wrong-session"] {
        let (_dir, sys, server, req) = setup(scenario);
        let error = execute(sys, server, req, CancellationToken::new())
            .await
            .0
            .unwrap_err();
        if scenario == "overloaded" {
            assert_eq!(error.class, ErrorClass::Overloaded);
            assert_eq!(error.http_status, Some(503));
            assert_eq!(
                error.code.as_deref(),
                Some("native:retry_exhausted:agy:attempts:4")
            );
            assert!(!error.message.contains("secret-model"));
        }
    }
}

#[tokio::test]
async fn antigravity_image_cancellation_sends_native_cancel() {
    let (dir, sys, server, req) = setup("wait");
    let cancel = CancellationToken::new();
    let fire = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        fire.cancel();
    });
    assert_eq!(
        execute(sys, server, req, cancel).await.0.unwrap_err().class,
        ErrorClass::Cancelled
    );
    assert!(
        std::fs::read_to_string(dir.path().join("wire.jsonl"))
            .unwrap()
            .contains("session/cancel")
    );
}

#[test]
fn antigravity_retry_classification_does_not_change_generic_acp_errors() {
    let mut error = sdk::Error::internal_error();
    error.message = "API error (attempt 4): UNAVAILABLE (code 503)".into();
    let generic = sdk_failure(error.clone(), false);
    assert_eq!(generic.class, ErrorClass::Protocol);
    assert!(!generic.native_retry_exhausted());
    let agy = sdk_failure(error, true);
    assert_eq!(agy.class, ErrorClass::Overloaded);
    assert_eq!(agy.http_status, Some(503));
    assert!(agy.native_retry_exhausted());
}
