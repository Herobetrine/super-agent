use super::*;
use crate::{claude::Claude, codex::Codex};
use futures::future::BoxFuture;
use sa_engine::adapter::{
    Adapter, AdapterError, AdapterEvent, Route, RunOutcome, RunRequest, TurnHost,
};
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

const MODEL: &str = "fixture-model@20250101";

struct Host;

impl TurnHost for Host {
    fn approve(&self, _: Value) -> BoxFuture<'static, bool> {
        Box::pin(async { false })
    }

    fn pending_notes(&self) -> Vec<Value> {
        vec![]
    }

    fn accept_notes(&self, _: &[i64]) {}
}

fn fake_cli() -> &'static Path {
    static FIXTURE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    &FIXTURE
        .get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            let exe = dir.path().join(if cfg!(windows) {
                "fixture.exe"
            } else {
                "fixture"
            });
            let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/startup_cli.c");
            let compiler = std::env::var_os("CC_x86_64_pc_windows_gnu")
                .or_else(|| std::env::var_os("CC"))
                .unwrap_or_else(|| if cfg!(windows) { "clang" } else { "cc" }.into());
            let output = std::process::Command::new(compiler)
                .arg(source)
                .arg("-o")
                .arg(&exe)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            (dir, exe)
        })
        .1
}

struct Fixture {
    dir: tempfile::TempDir,
    sys: Sys,
    request: RunRequest,
    config: PathBuf,
    credential: PathBuf,
}

impl Fixture {
    fn new(agent: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let home = root.join("home");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let name = if cfg!(windows) {
            format!("{agent}.exe")
        } else {
            agent.into()
        };
        std::fs::copy(fake_cli(), bin.join(name)).unwrap();
        let config_dir = home.join(format!(".{agent}"));
        std::fs::create_dir_all(&config_dir).unwrap();
        let (config, credential, provider) = if agent == "codex" {
            (
                config_dir.join("config.toml"),
                config_dir.join("auth.json"),
                "openai".into(),
            )
        } else {
            (
                config_dir.join("settings.json"),
                config_dir.join(".credentials.json"),
                crate::common::fingerprint(
                    "ccs-",
                    &[
                        json!("native"),
                        json!("first-party"),
                        json!(""),
                        json!("sdk-login"),
                    ],
                ),
            )
        };
        let mut sys = Sys::new(vec![], home, root.to_path_buf()).with_data_dir(root.join("data"));
        sys.set("PATH", bin.to_string_lossy());
        sys.set(
            "SA_STARTUP_FIXTURE_LOG",
            root.join("calls").to_string_lossy(),
        );
        sys.set("FIXTURE_SECRET", "private-env");
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            sys.set("SystemRoot", system_root.to_string_lossy());
        }
        std::fs::write(
            &credential,
            r#"{"token":"private-file","email":"private@example.com"}"#,
        )
        .unwrap();
        let request = RunRequest {
            ticket: 1,
            job: 1,
            turn: 1,
            agent: agent.into(),
            prompt: "fixture".into(),
            images: vec![],
            cwd: root.to_path_buf(),
            route: Route {
                provider,
                model: MODEL.into(),
                mode: "cli".into(),
                reasoning_effort: Some("low".into()),
            },
            profile: "full".into(),
            writable: true,
            session_id: None,
            bus: None,
        };
        Self {
            dir,
            sys,
            request,
            config,
            credential,
        }
    }

    fn adapter(&self) -> Box<dyn Adapter> {
        if self.request.agent == "codex" {
            Box::new(Codex::new(self.sys.clone()))
        } else {
            Box::new(Claude::new(self.sys.clone()))
        }
    }

    fn configure_endpoints(&self, providers: &[(&str, &ModelEndpoint)]) {
        if self.request.agent == "codex" {
            let config: String = providers
                .iter()
                .map(|(name, endpoint)| {
                    format!("[model_providers.{name}]\nbase_url = {:?}\n", endpoint.base)
                })
                .collect();
            std::fs::write(&self.config, config).unwrap();
        } else {
            let dir = self.sys.home.join(".cc-switch");
            std::fs::create_dir_all(&dir).unwrap();
            let db = rusqlite::Connection::open(dir.join("cc-switch.db")).unwrap();
            db.execute_batch("CREATE TABLE providers(app_type TEXT,id TEXT,name TEXT,settings_config TEXT,is_current INTEGER)").unwrap();
            for (name, endpoint) in providers {
                let config = json!({"env":{
                    "ANTHROPIC_BASE_URL":endpoint.base, "ANTHROPIC_API_KEY":"loopback-fixture"
                }});
                db.execute(
                    "INSERT INTO providers VALUES ('claude',?1,?1,?2,0)",
                    rusqlite::params![name, config.to_string()],
                )
                .unwrap();
            }
        }
    }

    fn select_endpoint(&mut self, name: &str, endpoint: &ModelEndpoint) {
        self.request.route.provider = if self.request.agent == "codex" {
            name.into()
        } else {
            crate::common::fingerprint(
                "ccs-",
                &[
                    json!("claude"),
                    json!(name),
                    json!(endpoint.base),
                    json!("ANTHROPIC_API_KEY"),
                ],
            )
        };
    }

    fn assert_discoveries(&self, count: usize) {
        let log = std::fs::read_to_string(self.dir.path().join("calls")).unwrap();
        for probe in if self.request.agent == "codex" {
            &["codex-models"][..]
        } else {
            &["claude-auth", "claude-models"][..]
        } {
            assert_eq!(
                log.lines().filter(|line| line == probe).count(),
                count,
                "{probe}"
            );
        }
    }

    async fn run(
        &self,
        adapter: &dyn Adapter,
    ) -> (Result<RunOutcome, AdapterError>, Vec<AdapterEvent>) {
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            adapter.run(
                self.request.clone(),
                Arc::new(Host),
                events,
                CancellationToken::new(),
            ),
        )
        .await
        .expect("turn fixture finished");
        let mut timings = Vec::new();
        while let Ok(event) = received.try_recv() {
            timings.push(event);
        }
        (result, timings)
    }

    async fn assert_run(&self, adapter: &dyn Adapter, cache: &str) {
        let (result, events) = self.run(adapter).await;
        assert_eq!(result.unwrap().text, "fixture-ok");
        assert_timing(&events, cache);
    }
}

async fn exercise(agent: &str) {
    let fixture = Fixture::new(agent);
    {
        let adapter = fixture.adapter();
        for cache in ["miss", "hit", "hit"] {
            fixture.assert_run(adapter.as_ref(), cache).await;
        }
        fixture.assert_discoveries(1);
    }
    let adapter = fixture.adapter();
    fixture.assert_run(adapter.as_ref(), "hit").await;
    fixture.assert_discoveries(1);
    for count in 2..=3 {
        adapter.catalog().await.unwrap();
        fixture.assert_discoveries(count);
    }
    fixture.assert_run(adapter.as_ref(), "hit").await;
    fixture.assert_discoveries(3);

    let changed = if agent == "codex" {
        format!("model = \"{MODEL}\"\n")
    } else {
        json!({"env":{"ANTHROPIC_MODEL": MODEL}}).to_string()
    };
    std::fs::write(&fixture.config, changed).unwrap();
    fixture.assert_run(adapter.as_ref(), "miss").await;
    fixture.assert_discoveries(4);

    std::fs::write(&fixture.credential, r#"{"token":"private-file-rotated"}"#).unwrap();
    fixture.assert_run(adapter.as_ref(), "miss").await;
    fixture.assert_discoveries(5);

    let marker = fixture.dir.path().join("fail-turn");
    std::fs::write(&marker, "").unwrap();
    let (out, events) = fixture.run(adapter.as_ref()).await;
    assert!(out.is_err());
    assert_timing(&events, "hit");
    fixture.assert_discoveries(5);
    std::fs::remove_file(marker).unwrap();
    drop(adapter);
    fixture.assert_run(fixture.adapter().as_ref(), "miss").await;
    fixture.assert_discoveries(6);

    let persisted =
        std::fs::read_to_string(fixture.dir.path().join("data/cache/probes-v1.json")).unwrap();
    for secret in [
        "fixture-secret",
        "private-env",
        "private-file",
        "fixture@example.com",
        "private@example.com",
    ] {
        assert!(!persisted.contains(secret), "{secret}");
    }
}

#[tokio::test]
async fn codex_discovery_is_reused_and_persisted() {
    exercise("codex").await;
}

#[tokio::test]
async fn claude_discovery_is_reused_and_persisted() {
    exercise("claude").await;
}

struct ModelEndpoint {
    base: String,
    failed: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
    worker: tokio::task::JoinHandle<()>,
}

impl ModelEndpoint {
    async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let failed = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let (failure, count) = (failed.clone(), requests.clone());
        let worker = tokio::spawn(async move {
            let body = json!({"data":[{"id":MODEL,"reasoningEfforts":["low"]}]}).to_string();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                loop {
                    let mut line = String::new();
                    if stream.read_line(&mut line).await.unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                count.fetch_add(1, Relaxed);
                let status = if failure.load(Relaxed) {
                    "503 Service Unavailable"
                } else {
                    "200 OK"
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                stream
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
            }
        });
        Self {
            base,
            failed,
            requests,
            worker,
        }
    }

    fn count(&self) -> usize {
        self.requests.load(Relaxed)
    }
}

impl Drop for ModelEndpoint {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

#[tokio::test]
async fn http_fallback_remains_usable_without_renewing_persistent_probe() {
    for agent in ["codex", "claude"] {
        let mut fixture = Fixture::new(agent);
        let endpoint = ModelEndpoint::new().await;
        fixture.configure_endpoints(&[("provider-a", &endpoint)]);
        fixture.select_endpoint("provider-a", &endpoint);
        let adapter = fixture.adapter();
        fixture.assert_run(adapter.as_ref(), "miss").await;
        let before = endpoint.count();
        assert!(before > 0);
        endpoint.failed.store(true, Relaxed);

        let refreshed = adapter.catalog().await.unwrap().value;
        let row = refreshed["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == fixture.request.route.provider)
            .unwrap();
        assert_eq!(row["models"][0]["id"], MODEL, "{agent}");
        assert_eq!(row["catalog_complete"], false, "{agent}");
        assert_eq!(row["catalogError"], true, "{agent}");
        let during_failure = endpoint.count();
        assert!(during_failure > before, "{agent}");
        drop(adapter);

        // Restart from disk while LAST_GOOD can still serve this process.
        fixture.assert_run(fixture.adapter().as_ref(), "miss").await;
        assert!(endpoint.count() > during_failure, "{agent}");
    }
}

#[tokio::test]
async fn provider_failure_keeps_the_other_provider_warm() {
    for agent in ["codex", "claude"] {
        let mut fixture = Fixture::new(agent);
        let a = ModelEndpoint::new().await;
        let b = ModelEndpoint::new().await;
        fixture.configure_endpoints(&[("provider-a", &a), ("provider-b", &b)]);
        fixture.select_endpoint("provider-a", &a);
        let adapter = fixture.adapter();
        fixture.assert_run(adapter.as_ref(), "miss").await;
        let before = (a.count(), b.count());
        assert_eq!(before, (1, 1), "{agent}");

        let marker = fixture.dir.path().join("fail-turn");
        std::fs::write(&marker, "").unwrap();
        let (failure, events) = fixture.run(adapter.as_ref()).await;
        assert!(failure.is_err(), "{agent}");
        assert_timing(&events, "hit");
        std::fs::remove_file(marker).unwrap();
        drop(adapter);

        fixture.select_endpoint("provider-b", &b);
        let adapter = fixture.adapter();
        fixture.assert_run(adapter.as_ref(), "miss").await;
        assert_eq!((a.count(), b.count()), (2, 1), "{agent}");
        fixture.assert_run(adapter.as_ref(), "hit").await;
        assert_eq!((a.count(), b.count()), (2, 1), "{agent}");
    }
}
