//! Read-only catalog discovery against the locally installed harnesses.
//! Opt-in: `SA_LIVE_CATALOG=1`. Never starts a model turn. Prints only
//! provider/model ids and availability, never credentials.

use sa_adapters::{
    antigravity::Antigravity, claude::Claude, codex::Codex, deepseek::DeepSeek, sys::Sys,
};
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
                assert!(
                    v["modes"].as_array().is_some_and(|m| m.len() == 2),
                    "{}: modes",
                    a.agent()
                );
                let cli = v["modes"][0]["available"].clone();
                let providers: Vec<String> = v["providers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| {
                        let models = p["models"].as_array().cloned().unwrap_or_default();
                        for model in &models {
                            if let Some(default) = model["defaultReasoningEffort"].as_str() {
                                assert!(sa_adapters::catalog::efforts(model).iter().any(|e| e == default), "{}: model effort default is outside advertised capabilities", a.agent());
                            }
                        }
                        let effort_count = models.iter().filter(|m| !sa_adapters::catalog::efforts(m).is_empty()).count();
                        let has_sol = models.iter().any(|m| m["id"].as_str().is_some_and(|s| s.contains("gpt-6.1-sol")));
                        let has_sonnet = models.iter().any(|m| m["id"].as_str().is_some_and(|s| s.contains("sonnet")));
                        format!(
                            "{}[{}]{} efforts={effort_count} sol={has_sol} sonnet={has_sonnet} complete={} source={}",
                            p["id"].as_str().unwrap_or("?"),
                            p["models"].as_array().map_or(0, Vec::len),
                            if p["available"] == Value::Bool(false) { "×" } else { "" },
                            p["catalog_complete"],
                            p["detail"].as_str().unwrap_or("原生 CLI")
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
                    assert!(
                        !text.contains(marker),
                        "{}: catalog leaks a credential-like value",
                        a.agent()
                    );
                }
            }
            Err(e) => eprintln!("{}: catalog error: {e}", a.agent()),
        }
    }
}
