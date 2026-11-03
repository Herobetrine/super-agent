//! Read-only catalog discovery against the locally installed harnesses.
//! Opt-in: `SA_LIVE_CATALOG=1`. Never starts a model turn. Prints only
//! provider/model ids and availability, never credentials.

use sa_adapters::{antigravity::Antigravity, claude::Claude, codex::Codex, deepseek::DeepSeek, sys::Sys};
use sa_engine::adapter::Adapter;
use serde_json::Value;
use std::sync::Arc;

#[tokio::test]
async fn installed_harness_catalogs_have_the_spec_shape() {
    if std::env::var("SA_LIVE_CATALOG").is_err() {
        eprintln!("set SA_LIVE_CATALOG=1 to probe installed harnesses");
        return;
    }
    let sys = Sys::from_process();
    let adapters: Vec<Arc<dyn Adapter>> = vec![
        Arc::new(Codex::new(sys.clone())),
        Arc::new(Claude::new(sys.clone())),
        Arc::new(DeepSeek::new(sys.clone())),
        Arc::new(Antigravity::new(sys)),
    ];
    for a in adapters {
        let started = std::time::Instant::now();
        match a.catalog().await {
            Ok(c) => {
                let v = &c.value;
                assert!(v["providers"].is_array(), "{}: providers", a.agent());
                assert!(v["modes"].as_array().is_some_and(|m| m.len() == 2), "{}: modes", a.agent());
                let cli = v["modes"][0]["available"].clone();
                let providers: Vec<String> = v["providers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| {
                        format!(
                            "{}[{}]{}",
                            p["id"].as_str().unwrap_or("?"),
                            p["models"].as_array().map_or(0, Vec::len),
                            if p["available"] == Value::Bool(false) { "×" } else { "" }
                        )
                    })
                    .collect();
                eprintln!(
                    "{}: cli={cli} default={} providers={providers:?} ({:?})",
                    a.agent(),
                    v["default"],
                    started.elapsed()
                );
                let text = v.to_string();
                for marker in ["sk-", "Bearer ", "api_key\":\"s"] {
                    assert!(!text.contains(marker), "{}: catalog leaks a credential-like value", a.agent());
                }
            }
            Err(e) => eprintln!("{}: catalog error: {e}", a.agent()),
        }
    }
}
