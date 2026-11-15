use super::*;
use crate::adapter::{Catalog, EventSink, RunOutcome};

#[derive(Default)]
struct WaitingAdapter {
    calls: Arc<Mutex<Vec<i64>>>,
}

impl Adapter for WaitingAdapter {
    fn agent(&self) -> &'static str { "codex" }

    fn catalog(&self) -> BoxFuture<'static, Result<Catalog>> {
        Box::pin(async {
            Ok(Catalog { value: json!({
                "default": {"provider":"local","model":"fixture","mode":"cli"},
                "modes":[{"id":"cli","available":true}],
                "providers":[{"id":"local","models":[{"id":"fixture"}]}]
            }) })
        })
    }

    fn run(&self, request: RunRequest, _: Arc<dyn TurnHost>, _: EventSink, cancel: CancellationToken)
        -> BoxFuture<'static, Result<RunOutcome, AdapterError>>
    {
        let calls = self.calls.clone();
        Box::pin(async move {
            lock(&calls).push(request.job);
            cancel.cancelled().await;
            Err(AdapterError::new(ErrorClass::Other, "fixture cancelled"))
        })
    }
}

struct PanicRunner;

impl TurnRunner for PanicRunner {
    fn run_turn(&self, _: Arc<dyn Adapter>, _: RunRequest, _: Arc<dyn TurnHost>, _: EventSink, _: CancellationToken)
        -> BoxFuture<'static, TurnResult>
    {
        panic!("fixture should fail before admission");
    }

    fn health(&self) -> Vec<(String, Route, Health)> {
        panic!("fixture scheduler panic");
    }
}

fn fixture(runner: Arc<dyn TurnRunner>) -> (tempfile::TempDir, Engine, Arc<WaitingAdapter>) {
    let dir = tempfile::tempdir().unwrap();
    let adapter = Arc::new(WaitingAdapter::default());
    let engine = Engine::open(dir.path(), vec![adapter.clone()], EngineOptions {
        runner,
        tick: Duration::from_millis(10),
        ..Default::default()
    }).unwrap();
    engine.db().with(|q| {
        q.run("INSERT INTO tickets(id,goal,cwd,writes,lead,created,permission_profile) VALUES(1,'fixture',?,0,'codex',0,'readonly')",
            &[&dir.path().to_string_lossy().as_ref()])?;
        q.conn().execute_batch(
            "INSERT INTO jobs(id,ticket,parent,agent,body,writing,depth,turn,state) VALUES
             (1,1,NULL,'codex','root',0,0,1,'waiting'),
             (2,1,1,'codex','child',0,1,1,'waiting'),
             (3,1,2,'codex','grandchild',0,2,1,'done');"
        )?;
        Ok(())
    }).unwrap();
    (dir, engine, adapter)
}

async fn until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.expect("scheduler condition timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timer_panic_is_observed_and_logged_without_restarting_work() {
    let (_dir, engine, adapter) = fixture(Arc::new(PanicRunner));
    engine.start().unwrap();
    until(|| engine.scheduler_health().state == SchedulerState::Failed).await;
    until(|| engine.db().with(|q| q.exists("SELECT 1 FROM events WHERE kind='scheduler.failed'", &[])).unwrap()).await;
    assert!(engine.fault().unwrap().contains("fixture scheduler panic"));
    let diagnostic: Value = serde_json::from_slice(&std::fs::read(engine.data_dir().join("scheduler-failure.json")).unwrap()).unwrap();
    assert_eq!(diagnostic["reason"], "panic");
    assert_eq!(diagnostic.as_object().unwrap().len(), 2);
    assert!(engine.start().is_err());
    assert!(lock(&adapter.calls).is_empty());
    let payload = engine.db().with(|q| q.one("SELECT payload FROM events WHERE kind='scheduler.failed'", &[])).unwrap().unwrap();
    assert_eq!(str_of(&payload, "payload"), "{\"reason\":\"panic\"}");
    engine.close().await;
    assert_eq!(engine.scheduler_health().state, SchedulerState::Stopped);
}

#[tokio::test]
async fn abort_before_first_poll_is_observed() {
    let (_dir, engine, _) = fixture(Arc::new(crate::runner::PassThroughRunner::default()));
    engine.start().unwrap();
    lock(&engine.inner.sched).timer.as_ref().unwrap().abort();
    until(|| engine.scheduler_health().state == SchedulerState::Failed).await;
    assert!(engine.fault().is_some());
    engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_child_wakes_waiting_parent_once() {
    let (_dir, engine, adapter) = fixture(Arc::new(crate::runner::PassThroughRunner::default()));
    engine.start().unwrap();
    until(|| lock(&adapter.calls).contains(&2)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(*lock(&adapter.calls), vec![2]);
    assert_eq!(engine.scheduler_health().state, SchedulerState::Running);
    engine.close().await;
    assert_eq!(engine.scheduler_health().state, SchedulerState::Stopped);
    assert!(engine.start().is_err());
    assert_eq!(*lock(&adapter.calls), vec![2]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unread_mail_wakes_parent_while_child_remains_waiting() {
    let (_dir, engine, adapter) = fixture(Arc::new(crate::runner::PassThroughRunner::default()));
    engine.db().with(|q| {
        q.run("UPDATE jobs SET state='interrupted' WHERE id=3", &[])?;
        q.record_note(1, Some(&json!(1)), Some(&json!("fixture note")), None, None)?;
        Ok(())
    }).unwrap();
    engine.start().unwrap();
    until(|| lock(&adapter.calls).contains(&1)).await;
    assert_eq!(*lock(&adapter.calls), vec![1]);
    engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_blocks_woken_work_and_normal_shutdown_is_final() {
    let (_dir, engine, adapter) = fixture(Arc::new(crate::runner::PassThroughRunner::default()));
    engine.db().with(|q| q.run("INSERT INTO ticket_pause(ticket,state) VALUES(1,'paused')", &[]).map(|_| ())).unwrap();
    engine.start().unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(lock(&adapter.calls).is_empty());
    assert_eq!(engine.scheduler_health().state, SchedulerState::Running);
    engine.quiesce().await;
    tokio::task::yield_now().await;
    assert_eq!(engine.scheduler_health().state, SchedulerState::Stopped);
    assert!(engine.fault().is_none());
    assert!(engine.start().is_err());
    engine.close().await;
}

#[test]
fn stale_progress_is_reported_without_taking_scheduler_or_journal_locks() {
    let (_dir, engine, _) = fixture(Arc::new(crate::runner::PassThroughRunner::default()));
    // A monotonic origin in the past makes the test deterministic and instant.
    let mut inner = Arc::try_unwrap(engine.inner).ok().unwrap();
    inner.scheduler_origin = Instant::now() - Duration::from_secs(60);
    inner.scheduler_state.store(SCHEDULER_RUNNING, Ordering::SeqCst);
    inner.scheduler_progress_ms.store(1, Ordering::SeqCst);
    inner.scheduler_database_busy.store(true, Ordering::SeqCst);
    inner.scheduler_sqlite_code.store(rusqlite::ffi::SQLITE_BUSY as u64, Ordering::SeqCst);
    let engine = Engine { inner: Arc::new(inner) };
    let _sched = lock(&engine.inner.sched);
    assert_eq!(engine.try_active_count(), None);
    engine.db().with(|_| {
        assert!(engine.db().try_with(|_| Ok(())).unwrap().is_none());
        assert_eq!(engine.scheduler_health().state, SchedulerState::Unresponsive);
        Ok(())
    }).unwrap();
}

struct AdmissionPanicRunner;

impl TurnRunner for AdmissionPanicRunner {
    fn run_turn(&self, _: Arc<dyn Adapter>, _: RunRequest, _: Arc<dyn TurnHost>, _: EventSink, _: CancellationToken)
        -> BoxFuture<'static, TurnResult>
    {
        panic!("fixture should fail inside admission");
    }

    fn admission_blocked_until(&self, _: &str, _: &Route, _: f64) -> Option<f64> {
        panic!("fixture panic inside admission transaction");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_panic_rolls_back_wake_and_keeps_journal_writable() {
    let (_dir, engine, adapter) = fixture(Arc::new(AdmissionPanicRunner));
    engine.db().with(|q| {
        q.run("INSERT INTO native_routes(job,route) VALUES(2,?)",
            &[&r#"{"provider":"local","model":"fixture","mode":"cli"}"#])?;
        Ok(())
    }).unwrap();
    engine.start().unwrap();
    until(|| engine.scheduler_health().state == SchedulerState::Failed).await;
    until(|| engine.db().with(|q| q.exists("SELECT 1 FROM events WHERE kind='scheduler.failed'", &[])).unwrap()).await;
    engine.db().with(|q| {
        assert!(q.conn().is_autocommit(), "unwinding must release the write transaction");
        let child = q.one("SELECT state,turn FROM jobs WHERE id=2", &[])?.unwrap();
        assert_eq!(str_of(&child, "state"), "waiting");
        assert_eq!(int_of(&child, "turn"), 1);
        assert!(!q.exists("SELECT 1 FROM events WHERE kind='turn.admitted'", &[])?);
        Ok(())
    }).unwrap();
    assert!(engine.db().try_with(|q| Ok(q.conn().is_autocommit())).unwrap().unwrap());
    engine.db().tx(|q| {
        q.run("UPDATE tickets SET goal='verified after panic' WHERE id=1", &[])?;
        Ok(())
    }).unwrap();
    assert!(lock(&adapter.calls).is_empty());
    assert!(engine.start().is_err());
    engine.close().await;
    assert_eq!(engine.scheduler_health().state, SchedulerState::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn blocked_admission_does_not_block_executor_timers() {
    let (_dir, engine, _) = fixture(Arc::new(crate::runner::PassThroughRunner::default()));
    engine.db().with(|q| q.run("INSERT INTO ticket_pause(ticket,state) VALUES(1,'paused')", &[]).map(|_| ())).unwrap();
    engine.start().unwrap();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let blocked = engine.clone();
    let holder = std::thread::spawn(move || {
        blocked.db().with(|_| {
            held_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(500));
            Ok(())
        }).unwrap();
    });
    held_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let started = Instant::now();
    let mut interval = tokio::time::interval(Duration::from_millis(10));
    for _ in 0..8 {
        interval.tick().await;
    }
    assert!(started.elapsed() < Duration::from_millis(300), "executor timers waited behind SQLite");
    assert_eq!(engine.try_active_count(), None, "the admission pass should still be waiting for SQLite");
    holder.join().unwrap();
    engine.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pass_waiting_for_database_does_not_admit_after_shutdown() {
    let (_dir, engine, adapter) = fixture(Arc::new(crate::runner::PassThroughRunner::default()));
    // Hold the database after start's recovery and before admission's first tick.
    engine.inner.started.store(true, Ordering::SeqCst);
    engine.inner.scheduler_state.store(SCHEDULER_RUNNING, Ordering::SeqCst);
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocked = engine.clone();
    let holder = std::thread::spawn(move || {
        blocked.db().with(|_| {
            held_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            Ok(())
        }).unwrap();
    });
    held_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let pass = engine.clone();
    let task = tokio::task::spawn_blocking(move || pass.step());
    until(|| engine.try_active_count().is_none()).await;
    let closing = engine.clone();
    let close = tokio::spawn(async move { closing.quiesce().await; });
    until(|| engine.scheduler_health().state == SchedulerState::Stopped).await;
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    assert_eq!(task.await.unwrap(), 0);
    close.await.unwrap();
    assert!(lock(&adapter.calls).is_empty());
    engine.db().with(|q| {
        let child = q.one("SELECT state,turn FROM jobs WHERE id=2", &[])?.unwrap();
        assert_eq!(str_of(&child, "state"), "waiting");
        assert_eq!(int_of(&child, "turn"), 1);
        Ok(())
    }).unwrap();
    engine.close().await;
}

struct AdmissionBarrierRunner {
    entered: AtomicBool,
    release: AtomicBool,
}

impl TurnRunner for AdmissionBarrierRunner {
    fn run_turn(&self, _: Arc<dyn Adapter>, _: RunRequest, _: Arc<dyn TurnHost>, _: EventSink, _: CancellationToken)
        -> BoxFuture<'static, TurnResult>
    {
        panic!("cancelled admission must not start a model");
    }

    fn admission_blocked_until(&self, _: &str, _: &Route, _: f64) -> Option<f64> {
        self.entered.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.release.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        None
    }
}

#[tokio::test]
async fn abort_during_admission_rolls_back_before_commit_and_starts_no_model() {
    let runner = Arc::new(AdmissionBarrierRunner {
        entered: AtomicBool::new(false),
        release: AtomicBool::new(false),
    });
    let (_dir, engine, adapter) = fixture(runner.clone());
    engine.db().with(|q| {
        q.run("INSERT INTO native_routes(job,route) VALUES(2,?)",
            &[&r#"{"provider":"local","model":"fixture","mode":"cli"}"#])?;
        Ok(())
    }).unwrap();
    engine.start().unwrap();
    let abort = lock(&engine.inner.sched).timer.as_ref().unwrap().abort_handle();
    until(|| runner.entered.load(Ordering::SeqCst)).await;
    abort.abort();
    until(|| engine.scheduler_health().state == SchedulerState::Failed).await;
    runner.release.store(true, Ordering::SeqCst);
    until(|| engine.try_active_count().is_some()).await;
    engine.db().with(|q| {
        let child = q.one("SELECT state,turn FROM jobs WHERE id=2", &[])?.unwrap();
        assert_eq!(str_of(&child, "state"), "waiting");
        assert_eq!(int_of(&child, "turn"), 1);
        assert!(!q.exists("SELECT 1 FROM events WHERE kind='turn.admitted'", &[])?);
        assert!(q.conn().is_autocommit());
        Ok(())
    }).unwrap();
    assert!(lock(&adapter.calls).is_empty());
    engine.close().await;
}

#[tokio::test]
async fn sqlite_writer_contention_retries_only_before_begin_and_recovers_once() {
    let (_dir, engine, adapter) = fixture(Arc::new(crate::runner::PassThroughRunner::default()));
    engine.db().with(|q| {
        q.conn().busy_timeout(Duration::from_millis(2))?;
        Ok(())
    }).unwrap();
    engine.start().unwrap();
    let other = rusqlite::Connection::open(engine.data_dir().join("hub.sqlite3")).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    until(|| engine.scheduler_health().state == SchedulerState::DatabaseBusy).await;
    let before = engine.inner.scheduler_progress_ms.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let health = engine.scheduler_health();
    assert_eq!(health.state, SchedulerState::DatabaseBusy);
    assert_eq!(health.last_sqlite_code, Some(rusqlite::ffi::SQLITE_BUSY));
    assert_eq!(health.last_sqlite_extended_code, Some(rusqlite::ffi::SQLITE_BUSY));
    assert_eq!(engine.inner.scheduler_progress_ms.load(Ordering::SeqCst), before);
    assert!(engine.fault().is_none());
    assert!(lock(&adapter.calls).is_empty());
    engine.db().with(|q| {
        assert!(q.conn().is_autocommit());
        assert!(!q.exists("SELECT 1 FROM events WHERE kind='turn.admitted'", &[])?);
        Ok(())
    }).unwrap();
    other.execute_batch("ROLLBACK").unwrap();
    until(|| lock(&adapter.calls).contains(&2)).await;
    tokio::time::sleep(Duration::from_millis(70)).await;
    assert_eq!(engine.scheduler_health().state, SchedulerState::Running);
    assert_eq!(*lock(&adapter.calls), vec![2]);
    assert!(engine.fault().is_none());
    let diagnostic: Value = serde_json::from_slice(&std::fs::read(engine.data_dir().join("scheduler-busy.json")).unwrap()).unwrap();
    assert_eq!(diagnostic["sqlite_code"], 5);
    assert_eq!(diagnostic.as_object().unwrap().len(), 4);
    engine.close().await;
}

#[test]
fn only_begin_busy_has_retry_proof_and_body_errors_roll_back() {
    let (_dir, engine, _) = fixture(Arc::new(crate::runner::PassThroughRunner::default()));
    // A BUSY generated after BEGIN must never be mistaken for an unstarted
    // admission, even when the database changes themselves roll back.
    let error = engine.db().admission_tx::<()>(|q| {
        q.run("UPDATE tickets SET goal='must roll back' WHERE id=1", &[])?;
        Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("fixture transaction-body BUSY".into()),
        ).into())
    }).unwrap_err();
    assert!(error.downcast_ref::<BeginBusy>().is_none());
    assert_eq!(sqlite_error_codes(&error), Some((5, 5)));
    engine.db().with(|q| {
        let row = q.one("SELECT goal FROM tickets WHERE id=1", &[])?.unwrap();
        assert_eq!(str_of(&row, "goal"), "fixture");
        assert!(q.conn().is_autocommit());
        q.conn().execute_batch("CREATE TABLE lock_fixture(id INTEGER); INSERT INTO lock_fixture VALUES(1)")?;
        Ok(())
    }).unwrap();
    // An active statement on this same connection makes DROP fail LOCKED.
    // This is a different fault from another connection owning the writer.
    let error = engine.db().admission_tx::<()>(|q| {
        let mut statement = q.conn().prepare("SELECT id FROM lock_fixture")?;
        let mut rows = statement.query([])?;
        assert!(rows.next()?.is_some());
        q.conn().execute_batch("DROP TABLE lock_fixture")?;
        Ok(())
    }).unwrap_err();
    assert_eq!(sqlite_error_codes(&error).unwrap().0, rusqlite::ffi::SQLITE_LOCKED);
    assert!(error.downcast_ref::<BeginBusy>().is_none());
    engine.db().with(|q| {
        assert!(q.conn().is_autocommit());
        Ok(())
    }).unwrap();
}

#[tokio::test]
async fn permanent_sqlite_admission_error_still_stops_and_records_numeric_code() {
    let (_dir, engine, adapter) = fixture(Arc::new(crate::runner::PassThroughRunner::default()));
    engine.db().with(|q| {
        q.conn().execute_batch("DROP TABLE ticket_pause")?;
        Ok(())
    }).unwrap();
    engine.start().unwrap();
    until(|| engine.scheduler_health().state == SchedulerState::Failed).await;
    let health = engine.scheduler_health();
    assert_eq!(health.last_sqlite_code, Some(rusqlite::ffi::SQLITE_ERROR));
    assert_eq!(health.last_sqlite_extended_code, Some(rusqlite::ffi::SQLITE_ERROR));
    tokio::time::sleep(Duration::from_millis(70)).await;
    assert!(lock(&adapter.calls).is_empty());
    until(|| engine.db().with(|q| q.exists("SELECT 1 FROM events WHERE kind='scheduler.failed'", &[])).unwrap()).await;
    engine.db().with(|q| {
        let row = q.one("SELECT payload FROM events WHERE kind='scheduler.failed'", &[])?.unwrap();
        let payload: Value = serde_json::from_str(str_of(&row, "payload"))?;
        assert_eq!(payload, json!({"reason":"admission_error","sqlite_code":1,"sqlite_extended_code":1}));
        Ok(())
    }).unwrap();
    engine.close().await;
}

#[tokio::test]
async fn database_busy_retry_respects_pause_and_shutdown() {
    let (_dir, engine, adapter) = fixture(Arc::new(crate::runner::PassThroughRunner::default()));
    engine.db().with(|q| {
        q.conn().busy_timeout(Duration::from_millis(2))?;
        q.run("INSERT INTO ticket_pause(ticket,state) VALUES(1,'paused')", &[])?;
        Ok(())
    }).unwrap();
    engine.start().unwrap();
    let other = rusqlite::Connection::open(engine.data_dir().join("hub.sqlite3")).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    until(|| engine.scheduler_health().state == SchedulerState::DatabaseBusy).await;
    other.execute_batch("ROLLBACK").unwrap();
    until(|| engine.scheduler_health().state == SchedulerState::Running).await;
    assert!(lock(&adapter.calls).is_empty());
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    until(|| engine.scheduler_health().state == SchedulerState::DatabaseBusy).await;
    engine.quiesce().await;
    other.execute_batch("ROLLBACK").unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(engine.scheduler_health().state, SchedulerState::Stopped);
    assert!(engine.fault().is_none());
    assert!(lock(&adapter.calls).is_empty());
    engine.close().await;
}

#[test]
fn real_commit_busy_rolls_back_without_begin_retry_proof() {
    let (_dir, engine, _) = fixture(Arc::new(crate::runner::PassThroughRunner::default()));
    engine.db().with(|q| {
        q.conn().busy_timeout(Duration::from_millis(2))?;
        let mode: String = q.conn().query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))?;
        assert_eq!(mode, "delete");
        Ok(())
    }).unwrap();
    let reader = rusqlite::Connection::open(engine.data_dir().join("hub.sqlite3")).unwrap();
    // The explicit transaction keeps its shared lock after SELECT finishes.
    reader.execute_batch("BEGIN; SELECT goal FROM tickets WHERE id=1;").unwrap();
    let bodies = AtomicUsize::new(0);
    let error = engine.db().admission_tx(|q| {
        bodies.fetch_add(1, Ordering::SeqCst);
        q.run("UPDATE tickets SET goal='uncommitted' WHERE id=1", &[])?;
        Ok(())
    }).unwrap_err();
    assert_eq!(sqlite_error_codes(&error).unwrap().0, rusqlite::ffi::SQLITE_BUSY);
    assert!(error.downcast_ref::<BeginBusy>().is_none());
    assert_eq!(bodies.load(Ordering::SeqCst), 1);
    engine.db().with(|q| {
        assert!(q.conn().is_autocommit());
        let row = q.one("SELECT goal FROM tickets WHERE id=1", &[])?.unwrap();
        assert_eq!(str_of(&row, "goal"), "fixture");
        Ok(())
    }).unwrap();
    reader.execute_batch("ROLLBACK").unwrap();
    engine.db().tx(|q| {
        q.run("UPDATE tickets SET goal='committed after reader left' WHERE id=1", &[])?;
        Ok(())
    }).unwrap();
    assert_eq!(bodies.load(Ordering::SeqCst), 1);
}
