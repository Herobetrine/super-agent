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
        let status_code = i32::from(status["loggedIn"] == false);
        let script = if cfg!(windows) {
            format!(
                "@echo off\r\nif \"%~4\"==\"auth\" goto status\r\n:init\r\nif defined FIXTURE_BLOCK_INIT goto init\r\necho initialized>>\"%FIXTURE_INITIALIZED%\"\r\necho {models}\r\nexit /b 0\r\n:status\r\nif \"%FIXTURE_WAIT%\"==\"\" goto ready\r\n:wait\r\nif not exist \"%FIXTURE_WAIT%\" goto wait\r\n:ready\r\necho {status}\r\nexit /b {status_code}\r\n"
            )
        } else {
            format!(
                "#!/bin/sh\nif [ \"$4\" = auth ]; then\nwhile [ -n \"$FIXTURE_WAIT\" ] && [ ! -e \"$FIXTURE_WAIT\" ]; do sleep 0.01; done\nprintf '%s\\n' '{status}'\nexit {status_code}\nelse\nwhile [ -n \"$FIXTURE_BLOCK_INIT\" ]; do sleep 0.01; done\nprintf initialized >> \"$FIXTURE_INITIALIZED\"\nprintf '%s\\n' '{models}'\nfi\n"
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
async fn first_party_timeouts_retain_good_rows_and_confirmed_logout_clears_them() {
    async fn exercise(block_status: bool) {
        let mut fixture = Fixture::new();
        // Exercise propagation through CC Switch's official-source alias too.
        fixture.row("official-alias", "claude", json!({}), "official");
        let login = json!({"loggedIn":true,"apiProvider":"firstParty"});
        let cancel = CancellationToken::new();
        let cli = fixture.cli(login.clone(), None);
        let good = discover(&fixture.sys, Some(cli), &cancel).await.unwrap().catalog;
        let source = good["default"]["provider"].clone();
        assert_eq!(good["default"]["model"], "official-model");
        let previous = sa_store::catalog_cache::Entry {value: good, fingerprint: String::new(), fetched_at: 0.0};
        let missing = fixture.data.path().join("unreleased");
        let cli = fixture.cli(login, block_status.then_some(missing.as_path()));
        if !block_status { fixture.sys.set("FIXTURE_BLOCK_INIT", "1"); }
        let mut timed_out = discover(&fixture.sys, Some(cli), &cancel).await.unwrap().catalog;
        let provider = |catalog: &Value| catalog["providers"].as_array().unwrap().iter()
            .find(|p| p["id"] == source).unwrap().clone();
        assert_eq!(provider(&timed_out)["catalogError"], true);
        previous.merge_failed_sources(&mut timed_out, true);
        assert_eq!(provider(&timed_out)["models"][0]["id"], "official-model");
        assert_eq!(timed_out["default"]["model"], "official-model");
        // The same source now explicitly reports loggedIn=false with exit 1.
        std::fs::write(&missing, b"released").unwrap();
        let cli = fixture.cli(json!({"loggedIn":false,"apiProvider":"firstParty"}), None);
        let mut logged_out = discover(&fixture.sys, Some(cli), &cancel).await.unwrap().catalog;
        previous.merge_failed_sources(&mut logged_out, true);
        assert_eq!(provider(&logged_out)["catalogError"], false);
        assert_eq!(provider(&logged_out)["available"], false);
        assert_eq!(provider(&logged_out)["models"], json!([]));
    }
    tokio::join!(exercise(true), exercise(false));
}

#[tokio::test]
async fn first_party_auth_timeout_after_credentials_change_drops_old_account_models() {
    let mut fixture = Fixture::new();
    let credentials = native_file(&fixture.sys).with_file_name(".credentials.json");
    std::fs::create_dir_all(credentials.parent().unwrap()).unwrap();
    std::fs::write(&credentials, br#"{"account":"first"}"#).unwrap();
    let login = json!({"loggedIn":true,"apiProvider":"firstParty"});
    let cli = fixture.cli(login, None);
    let cancel = CancellationToken::new();
    let good = discover(&fixture.sys, Some(cli.clone()), &cancel).await.unwrap().catalog;
    let revision = crate::catalog_revision(&fixture.sys, "claude");
    let id = good["default"]["provider"].clone();
    let source = |cat: &Value| cat["providers"].as_array().unwrap().iter()
        .find(|row| row["id"] == id).unwrap().clone();
    assert!(source(&good).get("sourceIdentity").is_none());
    assert_eq!(source(&good)["models"][0]["id"], "official-model");

    std::fs::write(credentials, br#"{"account":"second-account"}"#).unwrap();
    let current = crate::catalog_revision(&fixture.sys, "claude");
    assert_ne!(revision, current);
    fixture.sys.set("FIXTURE_WAIT", fixture.data.path().join("unreleased").to_string_lossy());
    let mut failed = discover(&fixture.sys, Some(cli), &cancel).await.unwrap().catalog;
    assert_eq!(source(&failed)["catalogError"], true);
    assert!(source(&failed).get("sourceIdentity").is_none());
    let previous = sa_store::catalog_cache::Entry { value: good, fingerprint: revision.clone(), fetched_at: 0.0 };
    previous.merge_failed_sources(&mut failed, revision == current);
    assert_eq!(source(&failed)["models"], json!([]));
    assert_eq!(source(&failed)["available"], false);
    assert_ne!(failed["default"]["model"], "official-model");
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

impl Fixture {
    fn proxy(&self, base: &str, state: Value) {
        let port: i64 = base.rsplit(':').next().unwrap().parse().unwrap();
        let db =
            rusqlite::Connection::open(self.data.path().join(".cc-switch/cc-switch.db")).unwrap();
        db.execute_batch("CREATE TABLE proxy_config(app_type TEXT,listen_address TEXT,listen_port INTEGER,enabled INTEGER)").unwrap();
        db.execute(
            "INSERT INTO proxy_config VALUES ('claude','127.0.0.1',?1,1)",
            [port],
        )
        .unwrap();
        std::fs::write(
            self.data.path().join(".cc-switch/live-state.json"),
            json!({"version":1,"apps":{"claude":state}}).to_string(),
        )
        .unwrap();
    }
}

#[tokio::test]
async fn aggregation_mode_routes_default_and_prefixed_member_models() {
    let fixture = Fixture::new();
    let members = HttpFixture::new(None, false);
    let listener = HttpFixture::new(None, false);
    members.release();
    listener.release();
    fixture.row("ferry", "claude", config(&members.base, "ANTHROPIC_API_KEY", "f"), "");
    fixture.row("other", "claude", config(&members.base, "ANTHROPIC_API_KEY", "o"), "");
    fixture.native(config(&listener.base, "ANTHROPIC_AUTH_TOKEN", "PROXY_MANAGED"));
    fixture.proxy(
        &listener.base,
        json!({"mode":"proxy","proxy_route":"ferry","stack":{"enabled":true,
            "members":["ferry","other"],"keys":{"api":"other","ferryman":"ferry","gone":"deleted"}}}),
    );
    let discovered = discover(&fixture.sys, None, &CancellationToken::new())
        .await
        .unwrap();
    let gateway = discovered
        .records
        .iter()
        .find(|r| r.app == "native")
        .unwrap();
    assert!(gateway.reason.is_empty());
    assert_eq!(gateway.name, "CC Switch 聚合");
    assert_eq!(
        gateway.models,
        ["bearer-PROXY_MANAGED", "key-f", "ccs-claude-api--key-o"]
    );
    assert_eq!(gateway.preferred, "key-f");
    let labels: Vec<&str> = gateway.model_info.iter().filter_map(|m| m["label"].as_str()).collect();
    assert!(labels.contains(&"key-f（默认路由 · ferry）"));
    assert!(labels.contains(&"key-o（other）"));
    let provider = discovered.catalog["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == json!(gateway.id()))
        .unwrap();
    assert_eq!(provider["available"], true);
}

#[tokio::test]
async fn routing_mode_without_native_pointer_adds_listener_source() {
    let fixture = Fixture::new();
    let members = HttpFixture::new(None, false);
    let listener = HttpFixture::new(None, false);
    members.release();
    listener.release();
    fixture.row("ferry", "claude", config(&members.base, "ANTHROPIC_API_KEY", "f"), "");
    fixture.proxy(&listener.base, json!({"mode":"proxy","proxy_route":"ferry"}));
    let discovered = discover(&fixture.sys, None, &CancellationToken::new())
        .await
        .unwrap();
    let gateway = discovered
        .records
        .iter()
        .find(|r| r.app == "cc-switch")
        .unwrap();
    assert!(gateway.reason.is_empty());
    assert_eq!(gateway.name, "CC Switch 路由");
    assert_eq!(gateway.token.as_deref(), Some("PROXY_MANAGED"));
    assert_eq!(gateway.models, ["bearer-PROXY_MANAGED", "key-f"]);
}

#[tokio::test]
async fn direct_mode_ignores_listener() {
    let fixture = Fixture::new();
    let listener = HttpFixture::new(None, false);
    listener.release();
    fixture.proxy(&listener.base, json!({"mode":"direct","proxy_route":"ferry"}));
    let discovered = discover(&fixture.sys, None, &CancellationToken::new())
        .await
        .unwrap();
    assert!(discovered.records.iter().all(|r| r.app != "cc-switch"));
    assert_eq!(listener.count(), 0);
}

#[test]
fn stack_ids_keep_the_one_million_marker_last() {
    assert_eq!(stack_id("api", "m[1M]"), "ccs-claude-api--m[1M]");
    assert_eq!(stack_id("api", "m[1m]"), "ccs-claude-api--m[1M]");
    assert_eq!(stack_id("api", "m"), "ccs-claude-api--m");
}

#[test]
fn routed_models_skip_one_million_twins() {
    let mut rec = Record {
        models: vec!["ccs-claude-api--m[1M]".into()],
        ..Default::default()
    };
    let source = Record {
        name: "api".into(),
        ..Default::default()
    };
    add_routed(&mut rec, stack_id("api", "m"), &source, "m", "api");
    add_routed(&mut rec, stack_id("api", "n"), &source, "n", "api");
    assert_eq!(rec.models, ["ccs-claude-api--m[1M]", "ccs-claude-api--n"]);
    assert_eq!(rec.model_info[0]["label"], "n（api）");
}
