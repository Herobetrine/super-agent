use super::*;
use crate::antigravity::{Antigravity, SETTINGS_REL, executable, fixture_sys};
use crate::probe_cache::{FRESHNESS, assert_timing};
use sa_engine::{
    adapter::{AdapterEvent, Route},
    images::ImageInput,
    resilience::CODE_ROUTE_PROBE_FAILED,
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
    let mut req = request(dir.path(), "gemini-fixture-low", "low");
    req.images.push(ImageInput::from_bytes("图片.PNG".into(), png).unwrap());
    (dir, sys, server, req)
}

fn request(cwd: &std::path::Path, model: &str, effort: &str) -> RunRequest {
    RunRequest {
        ticket: 1,
        job: 1,
        turn: 1,
        agent: "antigravity".into(),
        prompt: "审阅图片".into(),
        images: vec![],
        cwd: cwd.into(),
        route: Route {
            provider: "google".into(),
            model: model.into(),
            mode: "cli".into(),
            reasoning_effort: Some(effort.into()),
        },
        profile: "readonly".into(),
        writable: false,
        session_id: None,
        bus: None,
    }
}

async fn execute(
    sys: Sys,
    server: Server,
    req: RunRequest,
    cancel: CancellationToken,
) -> (Result<RunOutcome, AdapterError>, Vec<AdapterEvent>, usize) {
    let timing = DiscoveryTiming::new(Instant::now(), true, false);
    execute_with(|host, events| run_server(sys, req, host, Emit(events), cancel, server, Some(timing))).await
}

async fn execute_with<F>(
    run: impl FnOnce(Arc<dyn TurnHost>, EventSink) -> F,
) -> (Result<RunOutcome, AdapterError>, Vec<AdapterEvent>, usize)
where
    F: Future<Output = Result<RunOutcome, AdapterError>>,
{
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run(Arc::new(DenyHost(calls.clone())), tx),
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
async fn antigravity_acp_only_reloads_after_a_model_or_mode_change() {
    for (scenario, model_changes, mode_changes) in [
        ("unchanged", 0, 0),
        ("normal", 1, 0),
        ("mode-only", 0, 1),
        ("both", 1, 1),
    ] {
        for resumed in [false, true] {
            let (dir, sys, server, mut req) = setup(scenario);
            req.session_id = resumed.then(|| "agy-fixture-session".into());
            let (result, events, _) = execute(sys, server, req, CancellationToken::new()).await;
            assert_eq!(result.unwrap().text, "中途内容\n图片完成\n");
            let wire = std::fs::read_to_string(dir.path().join("wire.jsonl")).unwrap();
            let messages: Vec<Value> = wire.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
            let count = |method: &str| messages.iter().filter(|msg| msg["method"] == method).count();
            assert_eq!(count("session/load"), usize::from(resumed) + usize::from(model_changes + mode_changes > 0));
            assert_eq!(count("session/set_model"), model_changes);
            assert_eq!(count("session/set_mode"), mode_changes);
            assert_eq!(count("session/prompt"), 1);
            assert_timing(&events, "hit");
        }
    }
    let (dir, sys, server, req) = setup("wrong-mode");
    assert!(execute(sys, server, req, CancellationToken::new()).await.0.is_err());
    let wire = std::fs::read_to_string(dir.path().join("wire.jsonl")).unwrap();
    assert!(!wire.contains("session/prompt"));
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

const MODELS: &str = "model-a\tModel A\nmodel-b\tModel B\n";
const EFFORT: &str =
    r#"{"command":{"data":{"adjustable":true,"available":["low","high"],"current":"low"}}}"#;

struct ProbeFixture {
    dir: tempfile::TempDir,
    adapter: Antigravity,
    sys: Sys,
    req: RunRequest,
}

impl ProbeFixture {
    fn setup() -> Self {
        Self::setup_with(|_| {})
    }

    fn setup_with(configure: impl FnOnce(&mut Sys)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut sys = fixture_sys(dir.path());
        configure(&mut sys);
        sys.set("FAKE_AGY_MODELS_FILE", "models.txt");
        sys.set("FAKE_AGY_EFFORT_FILE", "effort.json");
        sys.set("FAKE_AGY_LOG_ARGS", "1");
        let fixture = Self {
            adapter: Antigravity::new(sys.clone()),
            sys,
            req: request(dir.path(), "model-b", "high"),
            dir,
        };
        fixture.write("models.txt", MODELS);
        fixture.write("effort.json", EFFORT);
        fixture
    }

    fn write(&self, name: &str, content: &str) {
        std::fs::write(self.dir.path().join(name), content).unwrap();
    }

    async fn run(&self) -> Result<RunOutcome, AdapterError> {
        self.execute(self.req.clone(), CancellationToken::new()).await
    }

    async fn execute(
        &self,
        req: RunRequest,
        cancel: CancellationToken,
    ) -> Result<RunOutcome, AdapterError> {
        execute_with(|host, events| self.adapter.run(req, host, events, cancel))
            .await
            .0
    }

    fn args(&self, kind: &str) -> Vec<Vec<String>> {
        std::fs::read_to_string(self.dir.path().join("calls.tsv"))
            .unwrap()
            .lines()
            .filter_map(|line| {
                let mut fields = line.split('\t');
                assert_eq!(fields.next(), Some(self.req.route.provider.as_str()));
                let action = fields.next().unwrap();
                fields.next().unwrap(); // Model column retained for the existing fixtures.
                (action == kind).then(|| fields.map(str::to_owned).collect())
            })
            .collect()
    }

    fn assert_counts(&self, models: usize, efforts: usize, runs: usize) {
        assert_eq!(self.args("models").len(), models);
        assert_eq!(self.args("effort").len(), efforts);
        assert_eq!(self.args("run").len(), runs);
    }
}

#[tokio::test]
async fn three_turns_reuse_probes_and_another_model_only_probes_its_effort() {
    let mut fixture = ProbeFixture::setup();
    for turn in 1..=3 {
        fixture.req.turn = turn;
        assert_eq!(fixture.run().await.unwrap().text, "fixture-ok");
    }
    fixture.assert_counts(1, 1, 3);
    assert_eq!(
        fixture.args("models"),
        [["--settings", r#"{"modelProvider":""}"#, "models"]]
    );
    assert_eq!(
        fixture.args("effort"),
        [[
            "--settings",
            r#"{"modelProvider":""}"#,
            "-p",
            "/effort",
            "--output-format",
            "json",
            "--model",
            "model-b",
        ]]
    );

    fixture.req.route.model = "model-a".into();
    fixture.run().await.unwrap();
    fixture.assert_counts(1, 2, 4);
    assert_eq!(fixture.args("effort")[1].last().unwrap(), "model-a");
}

#[tokio::test]
async fn cached_model_miss_refreshes_once_and_preserves_probe_failed() {
    for restored in [true, false] {
        let mut fixture = ProbeFixture::setup();
        fixture.write("models.txt", "model-a\tModel A\n");
        fixture.req.route.model = "model-a".into();
        fixture.run().await.unwrap();
        fixture.assert_counts(1, 1, 1);

        fixture.req.route.model = "model-b".into();
        if restored {
            fixture.write("models.txt", MODELS);
            assert_eq!(fixture.run().await.unwrap().text, "fixture-ok");
            fixture.assert_counts(2, 2, 2);
            fixture.req.route.model = "model-a".into();
            fixture.run().await.unwrap();
            fixture.assert_counts(2, 2, 3);
        } else {
            let error = fixture.run().await.unwrap_err();
            assert_eq!(error.class, ErrorClass::RouteUnavailable);
            assert_eq!(error.code.as_deref(), Some(CODE_ROUTE_PROBE_FAILED));
            fixture.assert_counts(2, 1, 1);
        }
    }
}

#[tokio::test]
async fn failed_run_evicts_probes_but_cancelled_run_keeps_them() {
    let fixture = ProbeFixture::setup();
    fixture.run().await.unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = fixture
        .execute(fixture.req.clone(), cancel)
        .await
        .unwrap_err();
    assert_eq!(error.class, ErrorClass::Cancelled);
    fixture.run().await.unwrap();
    fixture.assert_counts(1, 1, 2);

    let mut req = fixture.req.clone();
    req.session_id = Some("another-session".into());
    let error = fixture
        .execute(req, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.class, ErrorClass::SessionMismatch);
    fixture.assert_counts(1, 1, 3);
    fixture.run().await.unwrap();
    fixture.assert_counts(2, 2, 4);
}

#[tokio::test]
async fn catalog_refreshes_always_probe_and_write_through() {
    let mut fixture = ProbeFixture::setup();
    for refresh in 1..=2 {
        fixture.adapter.catalog().await.unwrap();
        fixture.assert_counts(refresh, 3 * refresh, 0);
    }
    fixture.run().await.unwrap();
    fixture.assert_counts(2, 6, 1);

    fixture.write("models.txt", "model-c\tModel C\n");
    fixture.req.route.model = "model-c".into();
    for refresh in 1..=2 {
        fixture
            .adapter
            .catalog_for_route(fixture.req.route.clone())
            .unwrap()
            .await
            .unwrap();
        fixture.assert_counts(2 + refresh, 6 + refresh, 1);
    }
    fixture.run().await.unwrap();
    fixture.assert_counts(4, 8, 2);
}

#[tokio::test]
async fn failed_models_probe_is_not_cached() {
    let fixture = ProbeFixture::setup();
    std::fs::remove_file(fixture.dir.path().join("models.txt")).unwrap();
    let catalog = fixture
        .adapter
        .catalog_for_route(fixture.req.route.clone())
        .unwrap()
        .await
        .unwrap();
    assert_eq!(catalog.value["providers"][0]["catalogError"], true);
    fixture.assert_counts(1, 0, 0);
    let error = fixture.run().await.unwrap_err();
    assert_eq!(error.code.as_deref(), Some(CODE_ROUTE_PROBE_FAILED));
    fixture.assert_counts(2, 0, 0);
    fixture.write("models.txt", MODELS);
    fixture.run().await.unwrap();
    fixture.assert_counts(3, 1, 1);
}

#[tokio::test]
async fn malformed_models_and_effort_are_not_cached() {
    let mut fixture = ProbeFixture::setup();
    fixture.write("models.txt", "unrecognized format");
    fixture.req.route.model = "@native".into();
    fixture.run().await.unwrap();
    fixture.run().await.unwrap();
    fixture.assert_counts(2, 2, 2);

    fixture.write("models.txt", MODELS);
    fixture.write("effort.json", "invalid JSON");
    fixture.req.route.model = "model-b".into();
    fixture
        .adapter
        .catalog_for_route(fixture.req.route.clone())
        .unwrap()
        .await
        .unwrap();
    fixture.assert_counts(3, 3, 2);
    fixture.write("effort.json", EFFORT);
    fixture.run().await.unwrap();
    fixture.assert_counts(3, 4, 3);
}

#[tokio::test]
async fn effort_reply_without_command_data_is_not_cached() {
    let fixture = ProbeFixture::setup();
    fixture.write("effort.json", r#"{"status":"ERROR","response":"not signed in"}"#);
    fixture
        .adapter
        .catalog_for_route(fixture.req.route.clone())
        .unwrap()
        .await
        .unwrap();
    fixture.assert_counts(1, 1, 0);
    fixture.write("effort.json", EFFORT);
    fixture.run().await.unwrap();
    // The error reply was not cached as a fixed effort, so the run probes the
    // effort again and needs no fresh catalog retry.
    fixture.assert_counts(1, 2, 1);
}

#[tokio::test]
async fn incomplete_effort_capabilities_are_not_cached() {
    for reply in [
        r#"{"command":{"data":{}}}"#,
        r#"{"command":{"data":{"adjustable":"false"}}}"#,
        r#"{"command":{"data":{"adjustable":true}}}"#,
        r#"{"command":{"data":{"adjustable":true,"available":[]}}}"#,
        r#"{"command":{"data":{"adjustable":true,"available":[],"current":"low"}}}"#,
        r#"{"command":{"data":{"adjustable":true,"available":["low",1],"current":"low"}}}"#,
        r#"{"command":{"data":{"adjustable":true,"available":["low"],"current":1}}}"#,
    ] {
        let fixture = ProbeFixture::setup();
        fixture.write("effort.json", reply);
        let catalog = fixture
            .adapter
            .catalog_for_route(fixture.req.route.clone())
            .unwrap()
            .await
            .unwrap();
        let model = &catalog.value["providers"][0]["models"][2];
        assert_eq!(model["id"], "model-b");
        assert!(model.get("reasoningEfforts").is_none(), "{reply}");
        assert!(model.get("defaultReasoningEffort").is_none(), "{reply}");
        fixture.assert_counts(1, 1, 0);

        fixture.write("effort.json", EFFORT);
        fixture.run().await.unwrap();
        fixture.assert_counts(1, 2, 1);
    }
}

#[tokio::test]
async fn native_settings_change_refreshes_probes_for_the_same_route() {
    let mut fixture = ProbeFixture::setup();
    fixture.req.route.model = "@native".into();
    let settings = fixture.dir.path().join(SETTINGS_REL);
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, r#"{"model":"model-a"}"#).unwrap();
    fixture.run().await.unwrap();
    fixture.assert_counts(1, 1, 1);

    std::fs::write(settings, r#"{"model":"model-b"}"#).unwrap();
    fixture.run().await.unwrap();
    fixture.assert_counts(2, 2, 2);
    fixture.run().await.unwrap();
    fixture.assert_counts(2, 2, 3);
}

#[tokio::test]
async fn in_place_executable_stamp_change_refreshes_probes_for_the_same_route() {
    let fixture = ProbeFixture::setup_with(|sys| {
        let shared = executable(sys).unwrap();
        std::fs::copy(&shared, sys.home.join(shared.file_name().unwrap())).unwrap();
        sys.set("PATH", sys.home.to_string_lossy().into_owned());
    });
    let exe = fixture.dir.path().join(if cfg!(windows) { "agy.exe" } else { "agy" });
    fixture.run().await.unwrap();
    fixture.assert_counts(1, 1, 1);

    let before = std::fs::metadata(&exe).unwrap().modified().unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&exe)
        .unwrap()
        .set_modified(before - Duration::from_secs(60))
        .unwrap();
    assert_ne!(std::fs::metadata(exe).unwrap().modified().unwrap(), before);
    fixture.run().await.unwrap();
    fixture.assert_counts(2, 2, 2);
    fixture.run().await.unwrap();
    fixture.assert_counts(2, 2, 3);
}

#[tokio::test]
async fn adc_credential_stamp_change_refreshes_probes_for_the_same_route() {
    for explicit in [false, true] {
        let mut fixture = ProbeFixture::setup_with(|sys| {
            sys.set("AGY_ADC_AUTH", "true");
            let home = sys.home.to_string_lossy().into_owned();
            sys.set("APPDATA", home.clone());
            sys.set("HOME", home);
            if explicit {
                sys.set("GOOGLE_APPLICATION_CREDENTIALS", "adc.json");
            }
        });
        fixture.req.route.provider = "adc".into();
        let credentials = fixture.dir.path().join(if explicit {
            "adc.json"
        } else if cfg!(windows) {
            "gcloud/application_default_credentials.json"
        } else {
            ".config/gcloud/application_default_credentials.json"
        });
        std::fs::create_dir_all(credentials.parent().unwrap()).unwrap();
        std::fs::write(&credentials, "{}").unwrap();
        fixture.run().await.unwrap();
        fixture.assert_counts(1, 1, 1);

        std::fs::write(&credentials, r#"{"fixture":2}"#).unwrap();
        fixture.run().await.unwrap();
        fixture.assert_counts(2, 2, 2);
        fixture.run().await.unwrap();
        fixture.assert_counts(2, 2, 3);
    }
}

#[tokio::test]
async fn google_account_file_change_refreshes_probes_for_the_same_route() {
    let fixture = ProbeFixture::setup();
    let account = fixture.dir.path().join(".gemini").join("google_accounts.json");
    std::fs::create_dir_all(account.parent().unwrap()).unwrap();
    std::fs::write(&account, r#"{"active":"a@example.com"}"#).unwrap();
    fixture.run().await.unwrap();
    fixture.assert_counts(1, 1, 1);

    std::fs::write(&account, r#"{"active":"someone.else@example.com"}"#).unwrap();
    fixture.run().await.unwrap();
    fixture.assert_counts(2, 2, 2);
    fixture.run().await.unwrap();
    fixture.assert_counts(2, 2, 3);
}

#[tokio::test]
async fn refresh_during_a_failed_run_survives_its_cache_eviction() {
    let fixture = ProbeFixture::setup_with(|sys| sys.set("FAKE_AGY_RUN_GATE", "release-run"));
    let refresh = || {
        fixture
            .adapter
            .catalog_for_route(fixture.req.route.clone())
            .unwrap()
    };
    refresh().await.unwrap();
    fixture.assert_counts(1, 1, 0);

    let mut request = fixture.req.clone();
    request.session_id = Some("another-session".into());
    let (events, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let mut run = fixture.adapter.run(
        request,
        Arc::new(DenyHost(Arc::new(AtomicUsize::new(0)))),
        events,
        CancellationToken::new(),
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! {
            () = async {
                while let Some(event) = receive.recv().await {
                    if matches!(event, AdapterEvent::Journal {kind, data}
                        if kind == "native" && data["event"] == "init")
                    {
                        return;
                    }
                }
                panic!("native run did not start");
            } => {},
            _ = &mut run => panic!("native run ended before the concurrent refresh"),
        }
        refresh().await.unwrap();
        fixture.assert_counts(2, 2, 1);
        fixture.write("release-run", "");
        assert_eq!(run.await.unwrap_err().class, ErrorClass::SessionMismatch);
    })
    .await
    .unwrap();

    fixture.run().await.unwrap();
    fixture.assert_counts(2, 2, 2);
}

#[tokio::test]
async fn expired_probes_are_fetched_again() {
    let fixture = ProbeFixture::setup();
    fixture.run().await.unwrap();
    fixture.assert_counts(1, 1, 1);
    fixture.adapter.set_probe_freshness(Duration::ZERO);
    fixture.run().await.unwrap();
    fixture.assert_counts(2, 2, 2);
}

#[tokio::test]
async fn persisted_probes_survive_restart_and_recover_from_bad_files() {
    let fixture = ProbeFixture::setup();
    let sys = fixture.sys.clone().with_data_dir(fixture.dir.path().join("app-data"));
    let data_dir = sys.data_dir.as_ref().unwrap();
    let path = data_dir.join("cache/probes-v1.json");
    for (index, state) in ["new", "restart", "corrupt", "expired", "unknown-version"].into_iter().enumerate() {
        match state {
            "corrupt" => fixture.write("app-data/cache/probes-v1.json", "{"),
            "expired" | "unknown-version" => {
                let mut disk: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                if state == "expired" {
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
                    let old = (now - FRESHNESS - Duration::from_secs(60)).as_millis();
                    for entry in disk["entries"].as_array_mut().unwrap() {
                        entry["started_unix_ms"] = json!(old);
                    }
                } else {
                    disk["version"] = json!(2);
                }
                std::fs::write(&path, disk.to_string()).unwrap();
            }
            _ => {}
        }
        let (out, events, _) = execute_with(|host, events| {
            Antigravity::new(sys.clone()).run(fixture.req.clone(), host, events, CancellationToken::new())
        }).await;
        assert_eq!(out.unwrap().text, "fixture-ok");
        let count = index + usize::from(index == 0);
        fixture.assert_counts(count, count, index + 1);
        assert_timing(&events, if state == "restart" { "hit" } else { "miss" });
        let disk: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(disk["version"], 1);
    }
}
