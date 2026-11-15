//! Explicit offline replay of a frozen SQLite backup. Never runs a CLI or model.
use futures::future::BoxFuture;
use sa_engine::adapter::{Adapter, AdapterError, Catalog, EventSink, RunOutcome, RunRequest, TurnHost};
use sa_engine::{Engine, EngineOptions};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

struct NoExecution(&'static str, Arc<tokio::sync::Semaphore>);
impl Adapter for NoExecution {
    fn agent(&self) -> &'static str { self.0 }
    fn catalog(&self) -> BoxFuture<'static, anyhow::Result<Catalog>> {
        let gate = self.1.clone();
        Box::pin(async move {
            let _ = gate.acquire().await;
            anyhow::bail!("offline replay catalog closed without model execution")
        })
    }
    fn run(&self, _: RunRequest, _: Arc<dyn TurnHost>, _: EventSink, _: CancellationToken)
        -> BoxFuture<'static, Result<RunOutcome, AdapterError>> {
        panic!("offline replay must never execute a model")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires SA_SCHEDULER_SNAPSHOT: a frozen SQLite backup, never the live journal"]
async fn frozen_pending_jobs_are_admitted_without_model_execution() {
    let source = PathBuf::from(std::env::var_os("SA_SCHEDULER_SNAPSHOT").expect("set frozen snapshot path"));
    let source = std::fs::canonicalize(source).unwrap();
    assert_ne!(source.file_name().unwrap(), "hub.sqlite3", "use an explicitly named offline backup");
    let wal = PathBuf::from(format!("{}-wal", source.display()));
    assert!(wal.metadata().map_or(true, |m| m.len() == 0), "backup must have no pending WAL writes");
    let data = tempfile::tempdir().unwrap();
    std::fs::copy(&source, data.path().join("hub.sqlite3")).unwrap();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let adapters = sa_engine::adapter::AGENTS.iter()
        .map(|name| Arc::new(NoExecution(name, gate.clone())) as Arc<dyn Adapter>).collect();
    let engine = Engine::open(data.path(), adapters, EngineOptions {
        tick: Duration::from_secs(3600), ..Default::default()
    }).unwrap();
    let eligible = engine.db().with(|q| q.all(
        "SELECT p.id,p.turn,p.state FROM jobs p
         WHERE NOT EXISTS(SELECT 1 FROM ticket_pause gate WHERE gate.ticket=p.ticket)
         AND ((p.state='queued' AND p.ready_after<=?) OR (p.state='waiting'
          AND (NOT EXISTS(SELECT 1 FROM jobs c WHERE c.parent=p.id AND c.state NOT IN ('done','partial','failed','cancelled'))
           OR EXISTS(SELECT 1 FROM mailbox m WHERE m.target=p.id AND m.consumed_turn IS NULL))))",
        &[&sa_engine::db::now()])).unwrap();
    assert!(!eligible.is_empty(), "snapshot has no eligible pending jobs");
    engine.start().unwrap();
    engine.step();
    let mut running = 0;
    for before in &eligible {
        let id = before["id"].as_i64().unwrap();
        let after = engine.db().with(|q| q.one("SELECT state,turn FROM jobs WHERE id=?", &[&id])).unwrap().unwrap();
        assert!(matches!(after["state"].as_str(), Some("queued" | "running")), "job {id}: {after:?}");
        let next_turn = before["turn"].as_i64().unwrap() + i64::from(before["state"] == "waiting");
        assert_eq!(after["turn"].as_i64().unwrap(), next_turn);
        running += usize::from(after["state"] == "running");
    }
    assert!(running > 0, "at least one pending job must actually be admitted");
    assert!(engine.fault().is_none());
    eprintln!("offline replay checked {} eligible pending jobs, {running} admitted; model execution disabled", eligible.len());
    gate.close();
    engine.close().await;
}
