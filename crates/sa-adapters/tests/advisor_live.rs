//! Opt-in, billable advisor audit in a fresh disposable ledger. The caller is
//! a synthetic running job; only Engine::consult invokes a real model.
use anyhow::{Context, Result, ensure};
use sa_adapters::{claude::Claude, sys::Sys};
use sa_engine::{
    CreateParams, Engine, EngineOptions,
    adapter::{Adapter, Route},
};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn advisor_returns_advice_and_accounted_usage() -> Result<()> {
    let Ok(path) = std::env::var("SA_LIVE_ADVISOR") else {
        return Ok(());
    };
    let scenario: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let text = |key: &str| -> Result<&str> {
        scenario[key]
            .as_str()
            .with_context(|| format!("missing {key}"))
    };
    let project = PathBuf::from(text("project")?).canonicalize()?;
    let ledger = PathBuf::from(text("ledger")?);
    ensure!(
        ledger.is_absolute() && !ledger.exists() && !ledger.starts_with(&project),
        "fresh ledger must be outside project"
    );
    std::fs::create_dir(&ledger)?;
    let adapter: Arc<dyn Adapter> = Arc::new(Claude::new(Sys::from_process()));
    let catalog = adapter.catalog().await?.value;
    let model = text("model")?;
    let provider = catalog["providers"]
        .as_array()
        .context("catalog missing providers")?
        .iter()
        .filter(|p| p["available"] != false)
        .filter(|p| scenario["provider"].as_str().is_none_or(|id| p["id"] == id))
        .filter(|p| {
            p["models"]
                .as_array()
                .is_some_and(|models| models.iter().any(|m| m["id"] == model))
        })
        .filter_map(|p| p["id"].as_str())
        .min_by_key(|id| catalog["default"]["provider"] != *id)
        .context("selected advisor model is unavailable")?;
    let route = Route {
        provider: provider.into(),
        model: model.into(),
        mode: "cli".into(),
        reasoning_effort: None,
    };
    sa_adapters::common::select(&catalog, &route)?;
    let engine = Engine::open(&ledger, vec![adapter], EngineOptions::default())?;
    let ticket = engine
        .create(CreateParams {
            goal: "Synthetic caller for an authorized read-only advisor review".into(),
            cwd: project,
            lead: Some("claude".into()),
            permission_profile: Some("readonly".into()),
            selections: Some(json!({"claude":route})),
            ..Default::default()
        })
        .await?;
    let job = engine.root(ticket)?["id"]
        .as_i64()
        .context("root missing")?;
    engine.set_advisor(Some(ticket), Some(("claude", &route)))?;
    engine.db().with(|q| {
        q.run("UPDATE jobs SET state='running',turn=1 WHERE id=?", &[&job])
            .map(|_| ())
    })?;
    let result = engine
        .consult(ticket, job, 1, text("brief")?, CancellationToken::new())
        .await;
    engine.db().with(|q| {
        q.run(
            "UPDATE jobs SET state='done',result='synthetic caller ended' WHERE id=?",
            &[&job],
        )
        .map(|_| ())
    })?;
    engine.close().await;
    let result = result?;
    ensure!(
        !result["advice"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .is_empty(),
        "advisor returned no advice"
    );
    std::fs::write(
        ledger.join("advice.json"),
        serde_json::to_vec_pretty(&result)?,
    )?;
    let mut store = sa_store::Store::open_read_only(&ledger)?;
    let mut snapshot = store.snapshot(ticket, 0)?;
    while snapshot.usage.pending {
        snapshot = store.snapshot(ticket, 0)?;
    }
    ensure!(
        snapshot.usage.advisor_calls.len() == 1,
        "expected one independent advisor attempt"
    );
    let call = &snapshot.usage.advisor_calls[0];
    ensure!(
        call.reported && call.tokens.total_tokens.is_some_and(|n| n > 0),
        "advisor usage missing"
    );
    ensure!(
        snapshot.usage.reported_advisor_calls == 1 && snapshot.usage.unreported_advisor_calls == 0,
        "advisor coverage mismatch"
    );
    ensure!(
        snapshot.usage.totals == call.tokens,
        "synthetic caller must not duplicate advisor cost"
    );
    std::fs::write(
        ledger.join("usage-receipt.json"),
        serde_json::to_vec_pretty(&json!({
            "ticket":ticket,"requested_model":model,"reported_model":result["advisor"]["model"],
            "attempt_id":call.attempt_id,"tokens":call.tokens,"scope":call.scope,
            "advisor_reported":call.reported,"advisor_partial":call.partial,"fixture_caller":true
        }))?,
    )?;
    println!(
        "Advisor returned substantive advice with independent, nonduplicated usage; ledger retained."
    );
    Ok(())
}
