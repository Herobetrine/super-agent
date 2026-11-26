use super::*;
use crate::{claude::Claude, codex::Codex};
use futures::future::BoxFuture;
use sa_engine::adapter::{
    Adapter, AdapterError, AdapterEvent, Route, RunOutcome, RunRequest, TurnHost,
};
use std::sync::OnceLock;

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
                model: "fixture-model".into(),
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
        "model = \"fixture-model\"\n"
    } else {
        r#"{"env":{"ANTHROPIC_MODEL":"fixture-model"}}"#
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
