//! Shared adapter plumbing (port of `tests-node/adapters.test.cjs`).

use futures::future::BoxFuture;
use sa_adapters::common::{fingerprint, scope, select};
use sa_adapters::proc::{Lines, start, stream};
use sa_adapters::sys::Sys;
use sa_engine::adapter::{ErrorClass, Route, RunRequest, TurnHost};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn node(sys: &Sys) -> Option<PathBuf> {
    sys.which(if cfg!(windows) { "node.exe" } else { "node" })
}

#[tokio::test]
async fn utf8_transport_preserves_split_code_points_and_rejects_invalid_bytes() {
    let text = "😀\t汉\r\n\u{0} \"尾\" \n";
    let mut wire = serde_json::to_vec(&json!({"text": text})).unwrap();
    wire.extend_from_slice(b"\r\n");
    // One byte per read, like the JS test's byte-by-byte Readable.
    let (mut tx, rx) = tokio::io::duplex(1);
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        for b in wire {
            tx.write_all(&[b]).await.unwrap();
        }
    });
    let mut lines = Lines::new(rx);
    assert_eq!(lines.next().await.unwrap(), Some(json!({"text": text})));
    assert_eq!(lines.next().await.unwrap(), None);

    let mut bad = b"{\"text\":\"".to_vec();
    bad.extend_from_slice(&[0xc0, 0xaf]);
    bad.extend_from_slice(b"\"}\n");
    let mut lines = Lines::new(&bad[..]);
    let err = lines.next().await.unwrap_err();
    assert!(err.message.contains("UTF-8"));
    assert_eq!(err.class, ErrorClass::Protocol);
}

#[test]
fn explicit_routes_fail_closed_for_stale_providers_models_and_desktops() {
    let cat = json!({
        "default": {"provider": "p", "model": "m", "mode": "cli"},
        "providers": [{"id": "p", "models": [{"id": "m"}, {"id": "hidden", "available": false}]}],
        "modes": [{"id": "cli", "available": true}, {"id": "desktop", "available": false}]
    });
    let route = |p: &str, m: &str, mode: &str| Route { provider: p.into(), model: m.into(), mode: mode.into() };
    assert_eq!(select(&cat, &route("p", "m", "cli")).unwrap(), route("p", "m", "cli"));
    for r in [route("p", "hidden", "cli"), route("other", "m", "cli"), route("p", "m", "desktop")] {
        let e = select(&cat, &r).unwrap_err();
        assert_eq!(e.class, ErrorClass::RouteUnavailable);
    }
}

struct Approver(bool);

impl TurnHost for Approver {
    fn approve(&self, _action: Value) -> BoxFuture<'static, bool> {
        let v = self.0;
        Box::pin(async move { v })
    }
    fn pending_notes(&self) -> Vec<Value> {
        vec![]
    }
    fn accept_notes(&self, _seqs: &[i64]) {}
}

fn request(profile: &str, writable: bool) -> RunRequest {
    RunRequest {
        ticket: 1,
        job: 1,
        turn: 1,
        agent: "deepseek".into(),
        prompt: "p".into(),
        images: vec![],
        cwd: std::env::current_dir().unwrap(),
        route: Route { provider: "p".into(), model: "m".into(), mode: "cli".into() },
        profile: profile.into(),
        writable,
        session_id: None,
    }
}

#[tokio::test]
async fn read_workspace_and_full_grants_are_enforced_before_dispatch() {
    let c = CancellationToken::new();
    let yes: Arc<dyn TurnHost> = Arc::new(Approver(true));
    let no: Arc<dyn TurnHost> = Arc::new(Approver(false));
    for agent in ["claude", "antigravity"] {
        let e = scope(agent, &request("partial", true), &yes, &c, false).await.unwrap_err();
        assert!(e.message.contains("未获"));
    }
    assert!(scope("deepseek", &request("readonly", true), &yes, &c, true).await.is_err());
    assert!(scope("deepseek", &request("ask", true), &no, &c, true).await.is_err());
    assert_eq!(scope("deepseek", &request("ask", true), &yes, &c, true).await.unwrap(), "workspace-write");
    assert!(scope("deepseek", &request("auto", false), &yes, &c, true).await.is_err());
    assert_eq!(scope("antigravity", &request("full", true), &yes, &c, false).await.unwrap(), "danger-full-access");
    assert_eq!(scope("claude", &request("readonly", false), &yes, &c, false).await.unwrap(), "read-only");
}

#[test]
fn cc_switch_public_ids_keep_the_python_encoding_across_unicode() {
    let digest = format!("{:x}", Sha256::digest("[\"\\u4e2d\", \"b\", \"\"]".as_bytes()));
    assert_eq!(fingerprint("ccs-", &[json!("中"), json!("b"), json!("")]), format!("ccs-{}", &digest[..24]));
}

#[tokio::test]
async fn cancel_stops_a_native_worker_and_its_own_descendant() {
    let sys = Sys::from_process();
    let Some(node) = node(&sys) else {
        eprintln!("node not on PATH; skipped");
        return;
    };
    let dir = tempfile::Builder::new().prefix("native-adapter-test-").tempdir().unwrap();
    let marker = dir.path().join("descendant-wrote");
    let grandchild = r#"setTimeout(()=>require("node:fs").writeFileSync(process.argv[1],"escaped"),900)"#;
    let code = format!(
        "const cp=require('node:child_process');const c=cp.spawn(process.execPath,['-e',{},process.argv[1]],{{windowsHide:true,stdio:'ignore'}});c.on('spawn',()=>console.log(JSON.stringify({{ready:true}})));setInterval(()=>{{}},999);",
        serde_json::to_string(grandchild).unwrap()
    );
    let args = ["-e".to_string(), code, marker.to_string_lossy().into_owned()];
    let mut p = start(&node, &args, dir.path(), &sys).unwrap();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let result = stream(&mut p, "", &cancel, |ev| {
        if ev["ready"] == json!(true) {
            trigger.cancel();
        }
        Ok(())
    })
    .await;
    assert_eq!(result.unwrap_err().class, ErrorClass::Cancelled);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(!marker.exists(), "descendant survived cancellation");
}

#[tokio::test]
async fn nonzero_exit_is_a_classified_crash_without_stderr_text() {
    let sys = Sys::from_process();
    let Some(node) = node(&sys) else {
        eprintln!("node not on PATH; skipped");
        return;
    };
    let code = "process.stderr.write('secret-token-123 ECONNRESET\\n');process.exit(3)";
    let mut p = start(&node, &["-e".to_string(), code.to_string()], &std::env::current_dir().unwrap(), &sys).unwrap();
    let err = stream(&mut p, "", &CancellationToken::new(), |_| Ok(())).await.unwrap_err();
    // The stderr tail refines the class but its text never leaves the adapter.
    assert_eq!(err.class, ErrorClass::Network);
    assert!(!err.message.contains("secret"));
    assert!(!err.code.unwrap_or_default().contains("secret"));
}
