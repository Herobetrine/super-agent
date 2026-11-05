//! Complete model listings and credential-origin boundaries, using local fixtures.
use sa_adapters::catalog;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn server(pages: Vec<Value>) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for page in pages {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            let _ = tx.send(request);
            let body = page.to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    (base, rx)
}

#[tokio::test]
async fn follows_all_anthropic_pages_preserving_auth_and_capabilities() {
    let (base, rx) = server(vec![
        json!({"data":[{"id":"opus"}],"has_more":true,"last_id":"opus"}),
        json!({"data":[{"id":"sonnet","capabilities":{"effort":{"low":{"supported":true}}}}],"has_more":false}),
    ]);
    let rows = catalog::fetch(
        &format!("{base}/v1/models?limit=1000"),
        &[("x-api-key".into(), "fixture".into())],
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["reasoningEfforts"], json!(["low"]));
    let first = rx.recv().unwrap();
    let second = rx.recv().unwrap();
    assert!(first.contains("x-api-key: fixture"));
    assert!(second.contains("after_id=opus"));
    assert!(second.contains("x-api-key: fixture"));
}

#[tokio::test]
async fn does_not_silently_truncate_after_256_or_follow_another_origin() {
    let data: Vec<Value> = (0..300).map(|n| json!({"id":format!("m-{n}")})).collect();
    let (base, _) = server(vec![json!({"data":data})]);
    assert_eq!(
        catalog::fetch(&base, &[], &CancellationToken::new())
            .await
            .unwrap()
            .len(),
        300
    );
    let (base, _) = server(vec![
        json!({"data":[{"id":"m"}],"next":"http://localhost:1/models"}),
    ]);
    let error = catalog::fetch(
        &base,
        &[("Authorization".into(), "fixture-secret".into())],
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("认证来源"));
    assert!(!error.to_string().contains("fixture-secret"));
}

#[tokio::test]
async fn repeated_cursor_fails_instead_of_reporting_partial_success() {
    let page = json!({"data":[{"id":"m"}],"has_more":true,"last_id":"m"});
    let (base, _) = server(vec![page.clone(), page]);
    let error = catalog::fetch(&base, &[], &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("重复分页"));
}
