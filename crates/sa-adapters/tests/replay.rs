//! Adapter::run against local stdio peers. The normal inbound shapes come from
//! M0's neon2 journal (Codex and agy) and opus journal (Claude). Setup replies,
//! permissions, failures and cancellation exchanges are synthetic.
use futures::future::BoxFuture;
use sa_adapters::{antigravity::Antigravity, claude::Claude, codex::Codex, sys::Sys};
use sa_engine::{
    adapter::{
        Adapter, AdapterError, AdapterEvent, BusGrant, ErrorClass, Route, RunOutcome, RunRequest,
        TurnHost,
    },
    protocol::tools_proof,
};
use serde_json::{Value, json};
use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{io::AsyncReadExt, net::TcpListener, sync::mpsc, time::timeout};
use tokio_util::sync::CancellationToken;

const PROMPT: &str = "Inspect fixture.txt.";
const IN_PROGRESS: &str = "Replay in progress.";
const LIMIT: Duration = Duration::from_secs(20);

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

#[derive(Clone, Copy)]
enum Harness {
    Codex,
    Claude,
    Antigravity,
}

impl Harness {
    fn agent(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Antigravity => "antigravity",
        }
    }

    fn executable(self) -> &'static str {
        match self {
            Self::Antigravity => "agy",
            _ => self.agent(),
        }
    }

    fn session(self) -> &'static str {
        match self {
            Self::Codex => "00000000-0000-4000-8000-000000000001",
            Self::Claude => "00000000-0000-4000-8000-000000000003",
            Self::Antigravity => "00000000-0000-4000-8000-000000000004",
        }
    }

    fn adapter(self, sys: Sys) -> Box<dyn Adapter> {
        match self {
            Self::Codex => Box::new(Codex::new(sys)),
            Self::Claude => Box::new(Claude::new(sys)),
            Self::Antigravity => Box::new(Antigravity::new(sys)),
        }
    }

    fn route(self) -> Route {
        let (provider, model) = match self {
            Self::Codex => ("openai".into(), "codex-fixture"),
            Self::Claude => (
                sa_adapters::common::fingerprint(
                    "ccs-",
                    &[
                        json!("native"),
                        json!("first-party"),
                        json!(""),
                        json!("sdk-login"),
                    ],
                ),
                "claude-fixture",
            ),
            Self::Antigravity => ("google".into(), "gemini-fixture"),
        };
        Route {
            provider,
            model: model.into(),
            mode: "cli".into(),
            reasoning_effort: None,
        }
    }
}

fn transcripts() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/transcripts")
}

#[derive(Debug)]
struct Replay {
    result: Result<RunOutcome, AdapterError>,
    events: Vec<AdapterEvent>,
}

impl Replay {
    fn native(&self) -> impl Iterator<Item = &Value> {
        self.events.iter().filter_map(|event| match event {
            AdapterEvent::Journal { kind, data } if kind == "native" => Some(data),
            _ => None,
        })
    }

    fn assert_success(&self, harness: Harness, text: &str, tool_id: &str, proof: &str) {
        let result = self.result.as_ref().unwrap();
        assert_eq!(result.text, text);
        assert_eq!(result.session_id.as_deref(), Some(harness.session()));
        self.assert_session(harness);

        let lifecycle: Vec<_> = self
            .events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::ToolStarted(id) => Some(("started", id.as_str())),
                AdapterEvent::ToolFinished(id) => Some(("finished", id.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(lifecycle, [("started", tool_id), ("finished", tool_id)]);
        assert_eq!(
            self.native().filter_map(tools_proof).collect::<Vec<_>>(),
            [proof]
        );
        assert_eq!(
            self.events
                .iter()
                .filter_map(|event| match event {
                    AdapterEvent::Journal { kind, data } if kind == "delta" => data.as_str(),
                    _ => None,
                })
                .collect::<String>(),
            text,
        );
    }

    fn assert_session(&self, harness: Harness) {
        let ids: Vec<_> = self
            .events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::Session(id) => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, [harness.session()]);
    }

    fn assert_failure(&self, harness: Harness, class: ErrorClass) {
        let error = self.result.as_ref().unwrap_err();
        assert_eq!(error.class, class, "{error:?}");
        assert_eq!(error.session_id.as_deref(), Some(harness.session()));
        self.assert_session(harness);
    }
}

async fn replay(harness: Harness, scenario: &str) -> Replay {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let exe = root.join(format!(
        "{}{}",
        harness.executable(),
        std::env::consts::EXE_SUFFIX
    ));
    fs::copy(env!("CARGO_BIN_EXE_sa-replay-cli"), exe).unwrap();
    let script = transcripts()
        .join(harness.agent())
        .join(format!("{scenario}.jsonl"));
    let receipt = root.join("replay-receipt");
    let mut sys = Sys::new(vec![], root.to_owned(), root.to_owned());
    for key in ["SystemRoot", "WINDIR"] {
        if let Ok(value) = std::env::var(key) {
            sys.set(key, value);
        }
    }
    let path = root.to_string_lossy();
    for key in ["HOME", "USERPROFILE", "TMP", "TEMP"] {
        sys.set(key, path.as_ref());
    }
    sys.set(
        "PATH",
        std::env::join_paths([root]).unwrap().to_string_lossy(),
    );
    sys.set("SA_REPLAY_SCRIPT", script.to_string_lossy());
    sys.set("SA_REPLAY_RECEIPT", receipt.to_string_lossy());
    let listener = if scenario == "cancel" {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        sys.set(
            "SA_REPLAY_LIFETIME",
            listener.local_addr().unwrap().to_string(),
        );
        Some(listener)
    } else {
        None
    };

    let req = RunRequest {
        ticket: 1,
        job: 1,
        turn: 1,
        agent: harness.agent().into(),
        prompt: PROMPT.into(),
        images: vec![],
        cwd: root.to_owned(),
        route: harness.route(),
        profile: "full".into(),
        writable: true,
        session_id: (scenario == "resume").then(|| harness.session().into()),
        // A full-access bus grant selects agy's native ACP transport for text.
        bus: matches!(harness, Harness::Antigravity).then(|| BusGrant {
            url: "http://127.0.0.1:1/mcp".into(),
            token: "replay-fixture".into(),
        }),
    };
    let cancel = CancellationToken::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let run = harness
        .adapter(sys)
        .run(req, Arc::new(Host), tx, cancel.clone());
    let mut events = Vec::new();
    let result = timeout(LIMIT, async {
        tokio::pin!(run);
        loop {
            tokio::select! {
                result = &mut run => break result,
                Some(event) = rx.recv() => {
                    if listener.is_some()
                        && matches!(&event, AdapterEvent::Journal { kind, data }
                            if kind == "delta" && data.as_str() == Some(IN_PROGRESS))
                    {
                        cancel.cancel();
                    }
                    events.push(event);
                }
            }
        }
    })
    .await;
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }

    // The adapter can redact stderr or translate a peer failure into its own
    // error. Independently require every expected outbound frame to match.
    let expected_receipt: String = fs::read_to_string(&script)
        .unwrap()
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let step: Value = serde_json::from_str(line).unwrap();
            if step.get("expect").is_some() || step.get("expect_response").is_some() {
                Some(format!("{}\n", index + 1))
            } else {
                None
            }
        })
        .collect();
    let actual_receipt = fs::read_to_string(receipt)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    assert_eq!(
        actual_receipt,
        expected_receipt,
        "{} outbound transcript diverged. Adapter result: {result:?}",
        script.display(),
    );
    let result = result.unwrap_or_else(|_| panic!("{} timed out", script.display()));

    if let Some(listener) = listener {
        assert!(
            cancel.is_cancelled(),
            "the mid-turn delta never triggered cancellation"
        );
        // Only the turn peer opens this socket and it holds it until exit.
        timeout(LIMIT, async {
            let (mut peer, _) = listener.accept().await.unwrap();
            match peer.read(&mut [0]).await {
                Ok(0) => {}
                Err(error) if error.kind() == ErrorKind::ConnectionReset => {}
                other => panic!("unexpected replay peer lifetime socket result: {other:?}"),
            }
        })
        .await
        .expect("the cancelled replay peer must exit");
    }

    Replay { result, events }
}

#[tokio::test]
async fn codex_normal() {
    let harness = Harness::Codex;
    let replay = replay(harness, "normal").await;
    replay.assert_success(harness, "Codex fixture result.\n", "command-1", "ok");
    let startup: Vec<_> = replay
        .native()
        .filter_map(|frame| match frame["method"].as_str() {
            method
            @ Some("thread/started" | "mcpServer/startupStatus/updated" | "turn/started") => method,
            _ => None,
        })
        .collect();
    assert_eq!(
        startup,
        [
            "thread/started",
            "mcpServer/startupStatus/updated",
            "turn/started"
        ]
    );
}

#[tokio::test]
async fn codex_failed() {
    let harness = Harness::Codex;
    replay(harness, "failed")
        .await
        .assert_failure(harness, ErrorClass::Auth);
}

#[tokio::test]
async fn codex_cancel() {
    let harness = Harness::Codex;
    let replay = replay(harness, "cancel").await;
    assert_eq!(
        replay.result.as_ref().unwrap_err().class,
        ErrorClass::Cancelled
    );
    replay.assert_session(harness);
}

#[tokio::test]
async fn claude_normal() {
    let harness = Harness::Claude;
    replay(harness, "normal").await.assert_success(
        harness,
        "Claude fixture result.\n",
        "tool-1",
        "ok",
    );
}

#[tokio::test]
async fn claude_failed() {
    let harness = Harness::Claude;
    let replay = replay(harness, "failed").await;
    replay.assert_failure(harness, ErrorClass::Protocol);
    assert_eq!(replay.result.unwrap_err().http_status, Some(400));
}

#[tokio::test]
async fn claude_cancel() {
    let harness = Harness::Claude;
    replay(harness, "cancel")
        .await
        .assert_failure(harness, ErrorClass::Cancelled);
}

#[tokio::test]
async fn antigravity_normal() {
    let harness = Harness::Antigravity;
    replay(harness, "normal").await.assert_success(
        harness,
        "Gemini fixture result.\n",
        "tool-1",
        "ok",
    );
}

#[tokio::test]
async fn antigravity_failed() {
    let harness = Harness::Antigravity;
    let replay = replay(harness, "failed").await;
    replay.assert_failure(harness, ErrorClass::Overloaded);
    assert_eq!(replay.result.unwrap_err().http_status, Some(503));
}

#[tokio::test]
async fn antigravity_cancel() {
    let harness = Harness::Antigravity;
    replay(harness, "cancel")
        .await
        .assert_failure(harness, ErrorClass::Cancelled);
}

#[tokio::test]
async fn antigravity_resume() {
    let harness = Harness::Antigravity;
    replay(harness, "resume").await.assert_success(
        harness,
        "Gemini resumed result.\n",
        "tool-1",
        "attempt",
    );
}

#[test]
fn transcripts_are_scrubbed() {
    let private = regex::Regex::new(
        r"(?i)hjy03|[a-z]:\\+|[cde]:/|[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}|sk-|ghp_|sab-[a-z0-9_-]{8,}",
    ).unwrap();
    let mut directories = vec![transcripts()];
    let mut count = 0;
    while let Some(directory) = directories.pop() {
        for child in fs::read_dir(directory).unwrap() {
            let path = child.unwrap().path();
            if path.is_dir() {
                directories.push(path);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                count += 1;
                for (index, line) in fs::read_to_string(&path).unwrap().lines().enumerate() {
                    let value: Value = serde_json::from_str(line).unwrap();
                    assert!(
                        !private.is_match(line) && !private.is_match(&value.to_string()),
                        "private data pattern in {}:{}",
                        path.display(),
                        index + 1,
                    );
                }
            }
        }
    }
    assert!(count > 0);
}
