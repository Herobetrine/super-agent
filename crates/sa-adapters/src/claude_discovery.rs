//! Discovery regressions use disposable CC Switch data, a fake CLI and loopback HTTP.
use super::*;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

struct HttpFixture {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    release: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl HttpFixture {
    fn new(marker: Option<PathBuf>, failed: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let release = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (seen, gate, stopped) = (requests.clone(), release.clone(), stop.clone());
        let worker = std::thread::spawn(move || {
            let mut handlers = Vec::new();
            while !stopped.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                };
                let (seen, gate, stopped, marker) =
                    (seen.clone(), gate.clone(), stopped.clone(), marker.clone());
                handlers.push(std::thread::spawn(move || {
                    stream.set_nonblocking(false).unwrap();
                    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut head = String::new();
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                            break;
                        }
                        head.push_str(&line);
                    }
                    let model = head.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        match name.to_ascii_lowercase().as_str() {
                            "x-api-key" => Some(format!("key-{}", value.trim())),
                            "authorization" => Some(format!("bearer-{}", value.trim().strip_prefix("Bearer ").unwrap())),
                            _ => None,
                        }
                    }).unwrap();
                    seen.lock().unwrap().push(head);
                    if let Some(marker) = marker {
                        std::fs::write(marker, b"HTTP reached").unwrap();
                    }
                    while !gate.load(Ordering::SeqCst) && !stopped.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    let body = json!({"data":[{"id":model,"reasoningEfforts":[]}]}).to_string();
                    let status = if failed { "503 Service Unavailable" } else { "200 OK" };
                    let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                }));
            }
            for handler in handlers {
                handler.join().unwrap();
            }
        });
        Self {
            base,
            requests,
            release,
            stop,
            worker: Some(worker),
        }
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    async fn wait_for(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.count() < count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("independent source probes must reach the HTTP barrier");
    }

    fn release(&self) {
        self.release.store(true, Ordering::SeqCst);
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.release();
        let result = self.worker.take().unwrap().join();
        if !std::thread::panicking() {
            result.unwrap();
        }
    }
}

struct Fixture {
    data: tempfile::TempDir,
    sys: Sys,
}

impl Fixture {
    fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let mut vars = Vec::new();
        for name in ["PATH", "SystemRoot", "COMSPEC"] {
            if let Ok(value) = std::env::var(name) {
                vars.push((name.into(), value));
            }
        }
        let sys = Sys::new(vars, data.path().into(), data.path().into());
        std::fs::create_dir_all(data.path().join(".cc-switch")).unwrap();
        let db = rusqlite::Connection::open(data.path().join(".cc-switch/cc-switch.db")).unwrap();
        db.execute_batch("CREATE TABLE providers(app_type TEXT,id TEXT,name TEXT,settings_config TEXT,is_current INTEGER,meta TEXT,category TEXT)").unwrap();
        Self { data, sys }
    }

    fn row(&self, id: &str, app: &str, cfg: Value, category: &str) {
        let db =
            rusqlite::Connection::open(self.data.path().join(".cc-switch/cc-switch.db")).unwrap();
        db.execute(
            "INSERT INTO providers VALUES (?1,?2,?2,?3,0,'{}',?4)",
            rusqlite::params![app, id, cfg.to_string(), category],
        )
        .unwrap();
    }

    fn native(&self, cfg: Value) {
        std::fs::create_dir_all(self.data.path().join(".claude")).unwrap();
        std::fs::write(
            self.data.path().join(".claude/settings.json"),
            cfg.to_string(),
        )
        .unwrap();
    }

    fn cli(&mut self, status: Value, wait: Option<&Path>) -> PathBuf {
        let initialized = self.data.path().join("initialized");
        self.sys
            .set("FIXTURE_INITIALIZED", initialized.to_string_lossy());
        if let Some(wait) = wait {
            self.sys.set("FIXTURE_WAIT", wait.to_string_lossy());
        }
        let models = json!({"response":{"request_id":"catalog","subtype":"success","response":{"models":[{"value":"official-model","reasoningEfforts":["high"]}]}}});
        let script = if cfg!(windows) {
            format!(
                "@echo off\r\nif \"%~4\"==\"auth\" goto status\r\necho initialized>>\"%FIXTURE_INITIALIZED%\"\r\necho {models}\r\nexit /b 0\r\n:status\r\nif \"%FIXTURE_WAIT%\"==\"\" goto ready\r\n:wait\r\nif not exist \"%FIXTURE_WAIT%\" goto wait\r\n:ready\r\necho {status}\r\nexit /b 0\r\n"
            )
        } else {
            format!(
                "#!/bin/sh\nif [ \"$4\" = auth ]; then\nwhile [ -n \"$FIXTURE_WAIT\" ] && [ ! -e \"$FIXTURE_WAIT\" ]; do sleep 0.01; done\nprintf '%s\\n' '{status}'\nelse\nprintf initialized >> \"$FIXTURE_INITIALIZED\"\nprintf '%s\\n' '{models}'\nfi\n"
            )
        };
        let path = self.data.path().join(if cfg!(windows) {
            "fake-claude.cmd"
        } else {
            "fake-claude"
        });
        std::fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        path
    }
}

fn config(base: &str, auth: &str, token: &str) -> Value {
    json!({"env":{"ANTHROPIC_BASE_URL":base,auth:token}})
}

#[tokio::test]
async fn explicitly_logged_out_skips_initialize_and_keeps_custom_sources() {
    let mut fixture = Fixture::new();
    let cli = fixture.cli(json!({"loggedIn":false,"apiProvider":"firstParty"}), None);
    let http = HttpFixture::new(None, false);
    http.release();
    fixture.row(
        "gateway",
        "claude",
        config(&http.base, "ANTHROPIC_API_KEY", "a"),
        "",
    );
    let discovered = discover(&fixture.sys, Some(cli), &CancellationToken::new())
        .await
        .unwrap();
    assert!(!fixture.data.path().join("initialized").exists());
    let official = discovered
        .records
        .iter()
        .find(|r| r.row == "first-party")
        .unwrap();
    assert_eq!(official.reason, "未验证官方登录或模型目录");
    assert!(official.models.is_empty());
    let gateway = discovered
        .records
        .iter()
        .find(|r| r.row == "gateway")
        .unwrap();
    assert!(gateway.reason.is_empty());
    assert_eq!(gateway.models, ["key-a"]);
    assert_eq!(http.count(), 1);
}

#[tokio::test]
async fn unconfirmed_auth_keeps_initialize_fallback_and_unavailable_reason() {
    for status in [json!({}), json!({"loggedIn":true,"apiProvider":"other"})] {
        let mut fixture = Fixture::new();
        let cli = fixture.cli(status, None);
        let official = first_party(&fixture.sys, Some(&cli), &CancellationToken::new()).await;
        assert!(fixture.data.path().join("initialized").exists());
        assert_eq!(official.models, ["official-model"]);
        assert_eq!(official.reason, "未验证官方登录或模型目录");
    }
}

#[tokio::test]
async fn same_credentials_share_success_and_failure_only_within_one_discovery() {
    for failed in [false, true] {
        let fixture = Fixture::new();
        let http = HttpFixture::new(None, failed);
        let cfg = config(&http.base, "ANTHROPIC_API_KEY", "a");
        for index in 0..6 {
            fixture.row(&format!("gateway-{index}"), "claude", cfg.clone(), "");
        }
        fixture.native(cfg);
        let cancel = CancellationToken::new();
        let probe = discover(&fixture.sys, None, &cancel);
        let release = async {
            http.wait_for(1).await;
            http.release();
        };
        let (discovered, ()) = tokio::join!(probe, release);
        let discovered = discovered.unwrap();
        assert_eq!(
            http.count(),
            1,
            "all overlapping CC/native misses must share one probe"
        );
        for record in discovered.records.iter().filter(|r| !r.base.is_empty()) {
            assert_eq!(record.catalog_error.is_some(), failed);
            if !failed {
                assert_eq!(record.models, ["key-a"]);
            }
        }
        discover(&fixture.sys, None, &cancel).await.unwrap();
        assert_eq!(http.count(), 2, "a later refresh must probe again");
    }
}

#[tokio::test]
async fn endpoint_auth_and_token_keys_progress_independently_without_mixing() {
    let fixture = Fixture::new();
    let first = HttpFixture::new(None, false);
    let second = HttpFixture::new(None, false);
    for (id, base, auth, token) in [
        ("key-a", &first.base, "ANTHROPIC_API_KEY", "a"),
        ("key-b", &first.base, "ANTHROPIC_API_KEY", "b"),
        ("bearer", &first.base, "ANTHROPIC_AUTH_TOKEN", "a"),
        ("other-endpoint", &second.base, "ANTHROPIC_API_KEY", "a"),
    ] {
        fixture.row(id, "claude", config(base, auth, token), "");
    }
    let cancel = CancellationToken::new();
    let (discovered, ()) = tokio::join!(discover(&fixture.sys, None, &cancel), async {
        tokio::join!(first.wait_for(3), second.wait_for(1));
        first.release();
        second.release();
    });
    let discovered = discovered.unwrap();
    assert_eq!((first.count(), second.count()), (3, 1));
    for (id, model) in [
        ("key-a", "key-a"),
        ("key-b", "key-b"),
        ("bearer", "bearer-a"),
        ("other-endpoint", "key-a"),
    ] {
        let record = discovered.records.iter().find(|r| r.row == id).unwrap();
        assert_eq!(record.models, [model]);
        assert_eq!(record.model_info[0]["reasoningEfforts"], json!([]));
    }
}

#[tokio::test]
async fn official_and_gateway_probes_overlap_then_merge_their_own_models() {
    let mut fixture = Fixture::new();
    let marker = fixture.data.path().join("http-reached");
    let cli = fixture.cli(
        json!({"loggedIn":true,"apiProvider":"firstParty"}),
        Some(&marker),
    );
    let http = HttpFixture::new(Some(marker), false);
    http.release();
    fixture.row("official-alias", "claude", json!({}), "official");
    fixture.row(
        "gateway",
        "claude-desktop",
        config(&http.base, "ANTHROPIC_API_KEY", "a"),
        "official",
    );
    let cancel = CancellationToken::new();
    let discovered = tokio::time::timeout(
        Duration::from_secs(3),
        discover(&fixture.sys, Some(cli), &cancel),
    )
    .await
    .expect("HTTP must start while official auth is waiting for it")
    .unwrap();
    assert!(fixture.data.path().join("initialized").exists());
    let official = discovered
        .records
        .iter()
        .find(|r| r.row == "official-alias")
        .unwrap();
    assert!(official.reason.is_empty());
    assert_eq!(official.auth, "sdk-login");
    assert_eq!(official.models, ["official-model"]);
    let gateway = discovered
        .records
        .iter()
        .find(|r| r.row == "gateway")
        .unwrap();
    assert_eq!(gateway.auth, "ANTHROPIC_API_KEY");
    assert_eq!(gateway.models, ["key-a"]);
    assert_eq!(gateway.model_info[0]["reasoningEfforts"], json!([]));
    assert_eq!(http.count(), 1);
}
