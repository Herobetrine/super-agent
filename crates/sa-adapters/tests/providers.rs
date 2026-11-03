//! Custom provider plumbing against an in-test loopback HTTP server: model
//! probing, catalog requests and endpoint rules. Offline; no model calls.

use sa_adapters::common::get_json;
use sa_adapters::custom::{CustomProvider, LOCAL_PLACEHOLDER, probe_models};
use sa_adapters::sys::Sys;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use tokio_util::sync::CancellationToken;

/// One-shot HTTP/1.1 server; returns its base URL and a receiver for the
/// raw request head.
fn serve(status: &str, extra: &str, body: &str) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
        body.len()
    );
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut head = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                break;
            }
            head.push_str(&line);
        }
        // The client may hang up early (oversized body); that is expected.
        let mut out = stream;
        let _ = out.write_all(response.as_bytes());
        let _ = out.flush();
        let _ = reader.read(&mut [0u8; 1]);
        let _ = tx.send(head);
    });
    (format!("http://127.0.0.1:{port}"), rx)
}

fn sys(vars: &[(&str, &str)]) -> Sys {
    Sys::new(
        vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        PathBuf::from("/nonexistent-home"),
        std::env::temp_dir(),
    )
}

fn provider(agent: &str, base: &str, key_env: &str) -> CustomProvider {
    CustomProvider {
        id: "gw".into(),
        agent: agent.into(),
        name: "本机网关".into(),
        base_url: base.into(),
        key_env: key_env.into(),
        models: vec![],
        enabled: true,
    }
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

#[tokio::test]
async fn openai_probe_sends_bearer_and_parses_models() {
    let (base, rx) = serve("200 OK", "", r#"{"object":"list","data":[{"id":"gpt-x"},{"id":"gpt-x"},{"id":"o-mini"}]}"#);
    let p = provider("codex", &format!("{base}/v1"), "GW_KEY");
    let models = probe_models(&p, &sys(&[("GW_KEY", "sk-test")]), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(models, vec!["gpt-x", "o-mini"]);
    let head = rx.recv().unwrap();
    assert!(head.starts_with("GET /v1/models HTTP/1.1"), "{head}");
    assert_eq!(header(&head, "authorization"), Some("Bearer sk-test"));
    assert_eq!(header(&head, "x-api-key"), None);
}

#[tokio::test]
async fn anthropic_probe_uses_v1_models_and_both_auth_headers() {
    let (base, rx) = serve("200 OK", "", r#"{"data":[{"id":"claude-x","type":"model"}],"has_more":false}"#);
    let p = provider("claude", &format!("{base}/v1/"), "GW_KEY");
    let models = probe_models(&p, &sys(&[("GW_KEY", "sk-ant")]), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(models, vec!["claude-x"]);
    let head = rx.recv().unwrap();
    assert!(head.starts_with("GET /v1/models?limit=1000 HTTP/1.1"), "{head}");
    assert_eq!(header(&head, "x-api-key"), Some("sk-ant"));
    assert_eq!(header(&head, "anthropic-version"), Some("2023-06-01"));
}

#[tokio::test]
async fn keyless_loopback_probe_sends_placeholder() {
    let (base, rx) = serve("200 OK", "", r#"{"data":[{"id":"local-m"}]}"#);
    let p = provider("deepseek", &format!("{base}/v1"), "");
    assert_eq!(probe_models(&p, &sys(&[]), &CancellationToken::new()).await.unwrap(), vec!["local-m"]);
    let head = rx.recv().unwrap();
    assert_eq!(header(&head, "authorization"), Some(format!("Bearer {LOCAL_PLACEHOLDER}").as_str()));
}

#[tokio::test]
async fn probe_failures_are_reported_without_the_key() {
    let p = provider("codex", "https://gw.example/v1", "GW_KEY");
    let e = probe_models(&p, &sys(&[]), &CancellationToken::new()).await.unwrap_err();
    assert!(e.to_string().contains("GW_KEY"), "{e}");

    let (base, _rx) = serve("401 Unauthorized", "", r#"{"error":"sk-secret is invalid"}"#);
    let p = provider("codex", &format!("{base}/v1"), "GW_KEY");
    let e = probe_models(&p, &sys(&[("GW_KEY", "sk-secret")]), &CancellationToken::new())
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("401") && !e.contains("sk-secret"), "{e}");

    let (base, _rx) = serve("200 OK", "", r#"{"data":[]}"#);
    let p = provider("codex", &format!("{base}/v1"), "");
    assert!(probe_models(&p, &sys(&[]), &CancellationToken::new()).await.is_err());
}

#[tokio::test]
async fn catalog_requests_do_not_follow_redirects() {
    let (base, _rx) = serve("302 Found", "Location: https://elsewhere.example/models\r\n", "{}");
    let e = get_json(&format!("{base}/models"), &[], &CancellationToken::new()).await.unwrap_err();
    assert!(e.to_string().contains("302"), "{e}");
}

#[tokio::test]
async fn catalog_requests_enforce_scheme_and_size() {
    let cancel = CancellationToken::new();
    for bad in ["http://example.com/models", "https://u:p@example.com/m", "file:///etc/passwd"] {
        let e = get_json(bad, &[], &cancel).await.unwrap_err();
        assert!(e.to_string().contains("HTTPS"), "{bad}: {e}");
    }
    let big = format!("{{\"data\":\"{}\"}}", "x".repeat(1024 * 1024 + 10));
    let (base, _rx) = serve("200 OK", "", &big);
    let e = get_json(&format!("{base}/models"), &[], &cancel).await.unwrap_err();
    assert!(e.to_string().contains("KiB"), "{e}");
    let bad_header = [("X\r\nInjected".to_string(), "v".to_string())];
    assert!(get_json("http://127.0.0.1:1/", &bad_header, &cancel).await.is_err());
}

#[tokio::test]
async fn cancellation_stops_a_pending_probe() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hold = thread::spawn(move || {
        let conn = listener.accept();
        thread::sleep(std::time::Duration::from_millis(1500));
        drop(conn);
    });
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        c.cancel();
    });
    let started = std::time::Instant::now();
    let e = get_json(&format!("http://127.0.0.1:{port}/models"), &[], &cancel).await.unwrap_err();
    assert!(e.to_string().contains("取消"), "{e}");
    assert!(started.elapsed() < std::time::Duration::from_millis(1000));
    hold.join().unwrap();
}
