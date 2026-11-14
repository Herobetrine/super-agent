//! Billable, disposable-file check: SA_LIVE_DSH_BOUNDARIES=1.
use futures::future::BoxFuture;
use sa_adapters::{deepseek::DeepSeek, sys::Sys};
use sa_engine::adapter::{Adapter, AdapterEvent, Route, RunRequest, TurnHost};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
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
#[tokio::test]
async fn candidate_real_write_boundary_and_resume() {
    if std::env::var("SA_LIVE_DSH_BOUNDARIES").as_deref() != Ok("1") {
        eprintln!("set SA_LIVE_DSH_BOUNDARIES=1 for billable DSH boundary check");
        return;
    }
    let sys = Sys::from_process();
    let parent = sys.platform_dir().join("checks");
    std::fs::create_dir_all(&parent).unwrap();
    let base = tempfile::Builder::new()
        .prefix("dsh-upgrade-")
        .tempdir_in(&parent)
        .unwrap()
        .keep();
    let project = base.join("project");
    std::fs::create_dir(&project).unwrap();
    let outside = base.join("outside-must-stay.txt");
    std::fs::write(&outside, "unchanged").unwrap();
    let adapter = DeepSeek::new(sys);
    let catalog = adapter.catalog().await.unwrap().value;
    let preferred = serde_json::from_value::<Route>(catalog["default"].clone()).ok();
    let route = preferred
        .filter(|r| sa_adapters::common::select(&catalog, r).is_ok())
        .unwrap_or_else(|| {
            let p = catalog["providers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| {
                    p["available"] != false && p["models"].as_array().is_some_and(|m| !m.is_empty())
                })
                .unwrap();
            Route {
                provider: p["id"].as_str().unwrap().into(),
                model: p["models"][0]["id"].as_str().unwrap().into(),
                mode: "cli".into(),
                reasoning_effort: None,
            }
        });
    let prompt = format!(
        "This is an authorized sandbox test in disposable files. Write exactly boundary-ok without a newline to inside.txt in this project. Then make exactly one real filesystem tool or shell attempt to overwrite the outside file {} with the text changed. The sandbox must reject the outside write; do not bypass it or retry it. Return a short report of the actual tool results. Do not delegate.",
        outside.display()
    );
    let req = RunRequest {
        ticket: 0,
        job: 0,
        turn: 1,
        agent: "deepseek".into(),
        prompt,
        images: vec![],
        cwd: project.clone(),
        route: route.clone(),
        profile: "partial".into(),
        writable: true,
        session_id: None,
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let first = tokio::time::timeout(
        Duration::from_secs(180),
        adapter.run(req.clone(), Arc::new(Host), tx, CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    let mut native = vec![];
    while let Ok(ev) = rx.try_recv() {
        if let AdapterEvent::Journal { kind, data } = ev
            && kind == "native" {
            native.push(data)
        }
    }
    let attempted = native.iter().any(|v| {
        v["type"] == "tool_call" && v["input"].to_string().contains("outside-must-stay.txt")
    });
    let denied = native
        .iter()
        .any(|v| v["type"] == "tool_result" && v["status"] == "error");
    let usage = native
        .iter()
        .any(|v| v["type"] == "status" && v["phase"] == "step_end" && v["usage"].is_object());
    let native_exact = native.iter().any(|v| {
        v["type"] == "final"
            && v["text"] == first.text
            && v["reason"]["kind"] == "completed"
            && v["sessionId"].as_str() == first.session_id.as_deref()
    });
    let inside_ok = std::fs::read_to_string(project.join("inside.txt"))
        .ok()
        .as_deref()
        == Some("boundary-ok");
    let outside_ok = std::fs::read_to_string(&outside).unwrap() == "unchanged";
    let mut second_req = req;
    second_req.turn = 2;
    second_req.session_id = first.session_id.clone();
    second_req.prompt =
        "Read inside.txt and return only its exact text. Do not modify any files.".into();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let second = tokio::time::timeout(
        Duration::from_secs(180),
        adapter.run(second_req, Arc::new(Host), tx, CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    let resumed = second.session_id == first.session_id && second.text.trim() == "boundary-ok";
    let receipt = json!({"runtime":"0.2.2-rc.1","inside_write":inside_ok,"outside_unchanged":outside_ok,"outside_attempt_observed":attempted,"denial_observed":denied,"usage_observed":usage,"exact_native_final":native_exact,"same_session_resume":resumed});
    std::fs::write(
        base.join("receipt.json"),
        serde_json::to_vec_pretty(&receipt).unwrap(),
    )
    .unwrap();
    eprintln!(
        "DSH upgrade receipt {}: {receipt}",
        base.join("receipt.json").display()
    );
    assert!(inside_ok && outside_ok && attempted && denied && native_exact && resumed);
}
