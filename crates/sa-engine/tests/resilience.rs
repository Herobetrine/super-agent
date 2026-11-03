//! Resilience layer tests (spec §9.2 I cases 1-6 and 8, plus provider
//! rotation). Fake adapters; no engine, no database, no network.

use futures::FutureExt;
use futures::future::BoxFuture;
use sa_engine::adapter::{
    Adapter, AdapterError, AdapterEvent, Catalog, ErrorClass, EventSink, Route, RunOutcome,
    RunRequest, TurnHost,
};
use sa_engine::resilience::classify::{self, classified};
use sa_engine::resilience::{
    CODE_ROUTE_NOT_CONFIGURED, CODE_ROUTE_PROBE_FAILED, Health, Recovery, Resilience,
    ResilienceConfig, ResilienceOptions, RetryLimits, STALLED_KIND, Supervisor, TurnLedger,
    TurnResult, backoff,
};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------- fakes

type RunFn = dyn Fn(Arc<dyn TurnHost>, EventSink, CancellationToken) -> BoxFuture<'static, Result<RunOutcome, AdapterError>>
    + Send
    + Sync;

struct FakeAdapter(Box<RunFn>);

impl FakeAdapter {
    fn adapter<F, Fut>(f: F) -> Arc<dyn Adapter>
    where
        F: Fn(Arc<dyn TurnHost>, EventSink, CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<RunOutcome, AdapterError>> + Send + 'static,
    {
        Arc::new(FakeAdapter(Box::new(move |h, e, c| f(h, e, c).boxed())))
    }
}

impl Adapter for FakeAdapter {
    fn agent(&self) -> &'static str {
        "codex"
    }
    fn catalog(&self) -> BoxFuture<'static, anyhow::Result<Catalog>> {
        async { Ok(Catalog::default()) }.boxed()
    }
    fn run(
        &self,
        _request: RunRequest,
        host: Arc<dyn TurnHost>,
        events: EventSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<RunOutcome, AdapterError>> {
        (self.0)(host, events, cancel)
    }
}

struct FakeHost {
    approve_after: Duration,
}

impl TurnHost for FakeHost {
    fn approve(&self, _action: Value) -> BoxFuture<'static, bool> {
        let d = self.approve_after;
        async move {
            sleep(d).await;
            true
        }
        .boxed()
    }
    fn pending_notes(&self) -> Vec<Value> {
        Vec::new()
    }
    fn accept_notes(&self, _seqs: &[i64]) {}
}

fn host() -> Arc<dyn TurnHost> {
    Arc::new(FakeHost {
        approve_after: Duration::from_millis(600),
    })
}

fn route(p: &str, m: &str) -> Route {
    Route {
        provider: p.into(),
        model: m.into(),
        mode: "cli".into(),
    }
}

fn request() -> RunRequest {
    RunRequest {
        ticket: 1,
        job: 1,
        turn: 1,
        agent: "codex".into(),
        prompt: "p".into(),
        cwd: PathBuf::from("."),
        route: route("openai", "m"),
        profile: "full".into(),
        writable: true,
        session_id: None,
    }
}

fn fast() -> ResilienceConfig {
    ResilienceConfig {
        first_frame: Duration::from_millis(300),
        model_idle: Duration::from_millis(200),
        tool_idle: Duration::from_millis(1500),
        turn_hard_cap: Duration::from_secs(10),
        interrupt_grace: Duration::from_millis(100),
        ..Default::default()
    }
}

fn frame() -> AdapterEvent {
    AdapterEvent::Journal {
        kind: "native".into(),
        data: json!({"x": 1}),
    }
}

fn ok(text: &str) -> Result<RunOutcome, AdapterError> {
    Ok(RunOutcome {
        text: text.into(),
        session_id: Some("s".into()),
    })
}

/// Run a turn and collect forwarded events.
async fn supervise(
    cfg: &ResilienceConfig,
    adapter: Arc<dyn Adapter>,
    cancel: CancellationToken,
) -> (TurnResult, Vec<AdapterEvent>, Duration) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let t0 = Instant::now();
    let res = Supervisor::run_turn(cfg, adapter, request(), host(), tx, cancel).await;
    let took = t0.elapsed();
    let mut evs = Vec::new();
    while let Ok(e) = rx.try_recv() {
        evs.push(e);
    }
    (res, evs, took)
}

fn failed(res: TurnResult) -> AdapterError {
    match res {
        TurnResult::Failed(e) => e,
        TurnResult::Done(o) => panic!("expected failure, got Done({:?})", o.text),
    }
}

/// Adapter that cooperatively stops when cancelled.
async fn until_cancelled(cancel: CancellationToken) -> Result<RunOutcome, AdapterError> {
    cancel.cancelled().await;
    Err(AdapterError::new(ErrorClass::Cancelled, "interrupted"))
}

// ------------------------------------------------------- supervisor (§9.2 I.1, I.2)

#[tokio::test(flavor = "multi_thread")]
async fn i1_stalled_stream_is_killed_at_model_idle_and_session_resumes() {
    let cfg = fast();
    let saw_cancel = Arc::new(AtomicUsize::new(0));
    let flag = saw_cancel.clone();
    let adapter = FakeAdapter::adapter(move |_h, ev, cancel| {
        let flag = flag.clone();
        async move {
            ev.send(AdapterEvent::Session("thread-1".into())).unwrap();
            ev.send(frame()).unwrap();
            let r = until_cancelled(cancel).await;
            flag.fetch_add(1, Ordering::SeqCst);
            r
        }
    });
    let (res, evs, took) = supervise(&cfg, adapter, CancellationToken::new()).await;
    let e = failed(res);
    assert_eq!(e.class, ErrorClass::Stalled);
    assert_eq!(e.code.as_deref(), Some("stall:model_idle"));
    assert_eq!(e.session_id.as_deref(), Some("thread-1"));
    assert!(e.message.contains("已中断"));
    assert_eq!(saw_cancel.load(Ordering::SeqCst), 1, "adapter token was cancelled");
    assert!(took >= Duration::from_millis(200) && took < Duration::from_millis(1500), "{took:?}");
    let stalled: Vec<_> = evs
        .iter()
        .filter(|e| matches!(e, AdapterEvent::Journal { kind, .. } if kind == STALLED_KIND))
        .collect();
    assert_eq!(stalled.len(), 1);
    assert_eq!(evs.len(), 3, "session + native + turn.stalled");

    // The policy retries the same route and resumes the early session.
    let r = Resilience::with_options(ResilienceConfig::default(), fixed(1.0));
    match r.after_failure("codex", &route("openai", "m"), &e, 1, &[], 0.0) {
        Recovery::Retry {
            attempt: 1,
            max: 3,
            resume_session,
            ..
        } => assert_eq!(resume_session.as_deref(), Some("thread-1")),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn first_frame_timeout() {
    let cfg = fast();
    let adapter = FakeAdapter::adapter(|_h, _ev, cancel| until_cancelled(cancel));
    let (res, _, took) = supervise(&cfg, adapter, CancellationToken::new()).await;
    let e = failed(res);
    assert_eq!(e.code.as_deref(), Some("stall:first_frame"));
    assert!(took >= Duration::from_millis(300), "{took:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn i2_open_tool_extends_to_tool_idle() {
    let cfg = fast();
    let adapter = FakeAdapter::adapter(|_h, ev, _c| async move {
        ev.send(frame()).unwrap();
        ev.send(AdapterEvent::ToolStarted("cmd-1".into())).unwrap();
        sleep(Duration::from_millis(500)).await; // > model_idle, < tool_idle (1.5 s)
        ev.send(AdapterEvent::ToolFinished("cmd-1".into())).unwrap();
        ev.send(frame()).unwrap();
        ok("done")
    });
    let (res, evs, _) = supervise(&cfg, adapter, CancellationToken::new()).await;
    assert!(matches!(res, TurnResult::Done(ref o) if o.text == "done"));
    assert_eq!(evs.len(), 4);

    // A tool that never finishes is cut at tool_idle.
    let adapter = FakeAdapter::adapter(|_h, ev, cancel| async move {
        ev.send(AdapterEvent::ToolStarted("cmd-2".into())).unwrap();
        until_cancelled(cancel).await
    });
    let (res, evs, took) = supervise(&cfg, adapter, CancellationToken::new()).await;
    let e = failed(res);
    assert_eq!(e.code.as_deref(), Some("stall:tool_idle"));
    assert!(took >= Duration::from_millis(1500), "{took:?}");
    let AdapterEvent::Journal { data, .. } = evs.last().unwrap() else {
        panic!()
    };
    assert_eq!(data["phase"], "tool_idle");
    assert_eq!(data["open_tools"], json!(["cmd-2"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn approval_pauses_idle_timer() {
    let cfg = fast();
    // In-flight TurnHost::approve (600 ms > model_idle).
    let adapter = FakeAdapter::adapter(|host, ev, _c| async move {
        ev.send(frame()).unwrap();
        assert!(host.approve(json!({"kind": "exec"})).await);
        sleep(Duration::from_millis(20)).await;
        ev.send(frame()).unwrap();
        ok("approved")
    });
    let (res, _, _) = supervise(&cfg, adapter, CancellationToken::new()).await;
    assert!(matches!(res, TurnResult::Done(_)), "{res:?}");

    // Announced AwaitingApproval pauses until the next frame.
    let adapter = FakeAdapter::adapter(|_h, ev, _c| async move {
        ev.send(AdapterEvent::AwaitingApproval).unwrap();
        sleep(Duration::from_millis(500)).await;
        ev.send(frame()).unwrap();
        ok("x")
    });
    let (res, _, _) = supervise(&cfg, adapter, CancellationToken::new()).await;
    assert!(matches!(res, TurnResult::Done(_)), "{res:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn native_retry_extends_idle_once() {
    let cfg = fast();
    let adapter = FakeAdapter::adapter(|_h, ev, _c| async move {
        ev.send(frame()).unwrap();
        ev.send(AdapterEvent::NativeRetry {
            after: Some(Duration::from_millis(1000)),
        })
        .unwrap();
        sleep(Duration::from_millis(450)).await; // > 200, < 200 + 1000
        ev.send(frame()).unwrap();
        ok("x")
    });
    let (res, _, _) = supervise(&cfg, adapter, CancellationToken::new()).await;
    assert!(matches!(res, TurnResult::Done(_)), "{res:?}");

    // The extension ends at the next ordinary frame.
    let adapter = FakeAdapter::adapter(|_h, ev, cancel| async move {
        ev.send(AdapterEvent::NativeRetry {
            after: Some(Duration::from_millis(2000)),
        })
        .unwrap();
        ev.send(frame()).unwrap();
        until_cancelled(cancel).await
    });
    let (res, _, took) = supervise(&cfg, adapter, CancellationToken::new()).await;
    assert_eq!(failed(res).code.as_deref(), Some("stall:model_idle"));
    assert!(took < Duration::from_millis(1500), "{took:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn hard_cap_applies_even_with_steady_frames() {
    let cfg = ResilienceConfig {
        turn_hard_cap: Duration::from_millis(500),
        ..fast()
    };
    let adapter = FakeAdapter::adapter(|_h, ev, cancel| async move {
        loop {
            if ev.send(frame()).is_err() || cancel.is_cancelled() {
                return Err(AdapterError::new(ErrorClass::Cancelled, "x"));
            }
            sleep(Duration::from_millis(50)).await;
        }
    });
    let (res, _, took) = supervise(&cfg, adapter, CancellationToken::new()).await;
    let e = failed(res);
    assert_eq!(e.code.as_deref(), Some("stall:hard_cap"));
    assert!(took >= Duration::from_millis(500) && took < Duration::from_millis(1500));
}

#[tokio::test(flavor = "multi_thread")]
async fn engine_cancel_maps_to_cancelled() {
    let cfg = fast();
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        sleep(Duration::from_millis(100)).await;
        c2.cancel();
    });
    let adapter = FakeAdapter::adapter(|_h, ev, cancel| async move {
        ev.send(AdapterEvent::Session("s9".into())).unwrap();
        until_cancelled(cancel).await
    });
    let (res, _, took) = supervise(&cfg, adapter, cancel).await;
    let e = failed(res);
    assert_eq!(e.class, ErrorClass::Cancelled);
    assert_eq!(e.session_id.as_deref(), Some("s9"));
    assert!(took < Duration::from_millis(1000), "{took:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn misbehaving_adapter_is_abandoned_after_grace() {
    let cfg = ResilienceConfig {
        first_frame: Duration::from_millis(100),
        interrupt_grace: Duration::from_millis(100),
        ..fast()
    };
    // Ignores its token entirely.
    let adapter = FakeAdapter::adapter(|_h, _ev, _c| futures::future::pending());
    let (res, _, took) = supervise(&cfg, adapter, CancellationToken::new()).await;
    assert_eq!(failed(res).class, ErrorClass::Stalled);
    assert!(took < Duration::from_millis(1000), "{took:?}");

    let cancel = CancellationToken::new();
    cancel.cancel();
    let adapter = FakeAdapter::adapter(|_h, _ev, _c| futures::future::pending());
    let (res, _, took) = supervise(&cfg, adapter, cancel).await;
    assert_eq!(failed(res).class, ErrorClass::Cancelled);
    assert!(took < Duration::from_millis(1000));
}

#[tokio::test(flavor = "multi_thread")]
async fn closed_forward_channel_stops_the_turn() {
    let cfg = fast();
    let adapter = FakeAdapter::adapter(|_h, ev, cancel| async move {
        ev.send(frame()).unwrap();
        until_cancelled(cancel).await
    });
    let (tx, rx) = mpsc::unbounded_channel();
    drop(rx);
    let res = Supervisor::run_turn(&cfg, adapter, request(), host(), tx, CancellationToken::new()).await;
    assert_eq!(failed(res).class, ErrorClass::Cancelled);
}

#[tokio::test(flavor = "multi_thread")]
async fn adapter_errors_pass_through_with_seen_session() {
    let cfg = fast();
    let adapter = FakeAdapter::adapter(|_h, ev, _c| async move {
        ev.send(AdapterEvent::Session("abc".into())).unwrap();
        ev.send(frame()).unwrap();
        Err(classify::classify_text("read ECONNRESET token=sk-SECRET"))
    });
    let (res, evs, _) = supervise(&cfg, adapter, CancellationToken::new()).await;
    assert_eq!(evs.len(), 2, "frames queued before the error are forwarded");
    let e = failed(res);
    assert_eq!(e.class, ErrorClass::Network);
    assert_eq!(e.session_id.as_deref(), Some("abc"));
    assert!(!e.message.contains("SECRET"));

    let adapter = FakeAdapter::adapter(|_h, _ev, _c| async move {
        panic!("adapter bug");
    });
    let (res, _, _) = supervise(&cfg, adapter, CancellationToken::new()).await;
    assert_eq!(failed(res).code.as_deref(), Some("adapter:panic"));
}

// ------------------------------------------------------------ policy

fn fixed(u: f64) -> ResilienceOptions {
    ResilienceOptions {
        rng: Box::new(move || u),
        clock: Arc::new(|| 0.0),
        ..Default::default()
    }
}

fn no_breaker() -> ResilienceConfig {
    ResilienceConfig {
        breaker_threshold: u32::MAX,
        ..Default::default()
    }
}

fn err(class: ErrorClass) -> AdapterError {
    classified(class, "test")
}

#[test]
fn i3_crash_retried_three_times_then_gives_up() {
    let r = Resilience::with_options(no_breaker(), fixed(1.0));
    let a = route("p", "m");
    let e = classify::from_exit(Some(1));
    assert_eq!(e.class, ErrorClass::ProcessCrashed);
    for n in 1..=3u32 {
        assert_eq!(
            r.after_failure("claude", &a, &e, n, &[], 0.0),
            Recovery::Retry {
                delay: Duration::from_secs(2u64 << (n - 1)),
                attempt: n,
                max: 3,
                resume_session: None
            }
        );
    }
    assert_eq!(
        r.after_failure("claude", &a, &e, 4, &[], 0.0),
        Recovery::GiveUp {
            class: ErrorClass::ProcessCrashed,
            attempts: 4
        }
    );
    // Stall has the same budget; transient has 5.
    assert!(matches!(
        r.after_failure("claude", &a, &err(ErrorClass::Stalled), 4, &[], 0.0),
        Recovery::GiveUp { .. }
    ));
    assert!(matches!(
        r.after_failure("claude", &a, &err(ErrorClass::Network), 5, &[], 0.0),
        Recovery::Retry { max: 5, .. }
    ));
    assert!(matches!(
        r.after_failure("claude", &a, &err(ErrorClass::Network), 6, &[], 0.0),
        Recovery::GiveUp { .. }
    ));
}

#[test]
fn i4_retry_after_is_honored() {
    let r = Resilience::with_options(no_breaker(), fixed(1.0));
    let a = route("p", "m");
    let e = classify::from_http_status(429, Some(Duration::from_secs(30)));
    match r.after_failure("codex", &a, &e, 1, &[], 0.0) {
        Recovery::Retry { delay, .. } => assert_eq!(delay, Duration::from_secs(30)),
        other => panic!("{other:?}"),
    }
    // Backoff wins when it is longer than Retry-After.
    let e = classify::from_http_status(429, Some(Duration::from_secs(1)));
    match r.after_failure("codex", &a, &e, 4, &[], 0.0) {
        Recovery::Retry { delay, .. } => assert_eq!(delay, Duration::from_secs(16)),
        other => panic!("{other:?}"),
    }
    // HTTP-date form through the parser.
    let now = std::time::UNIX_EPOCH + Duration::from_secs(784_111_777);
    assert_eq!(
        classify::parse_retry_after("Sun, 06 Nov 1994 08:50:37 GMT", now),
        Some(Duration::from_secs(60))
    );
}

#[test]
fn i5_jitter_stays_within_bounds() {
    let base = Duration::from_secs(2);
    let cap = Duration::from_secs(300);
    let r = Resilience::new(no_breaker());
    let a = route("p", "m");
    for n in 1..=5u32 {
        let full = (base * (1 << (n - 1))).min(cap);
        for _ in 0..200 {
            let Recovery::Retry { delay, .. } =
                r.after_failure("codex", &a, &err(ErrorClass::Overloaded), n, &[], 0.0)
            else {
                panic!()
            };
            assert!(delay >= full / 2 && delay <= full, "n={n} {delay:?}");
        }
    }
    for n in [0, 3, 8, 40] {
        for u in [0.0, 0.25, 0.999, 1.0] {
            let d = backoff(base, cap, n, u);
            let full = if n >= 8 { cap } else { (base * (1 << n)).min(cap) };
            assert!(d >= full / 2 && d <= full);
        }
    }
}

#[test]
fn i6_breaker_opens_and_blocks_other_jobs_without_consuming_attempts() {
    let r = Resilience::with_options(ResilienceConfig::default(), fixed(1.0));
    let a = route("p", "m");
    let b = route("p", "other");
    let e = err(ErrorClass::Overloaded);
    // Three different jobs fail once each on the same route.
    assert!(matches!(r.after_failure("codex", &a, &e, 1, &[], 10.0), Recovery::Retry { .. }));
    assert!(matches!(r.after_failure("codex", &a, &e, 1, &[], 11.0), Recovery::Retry { .. }));
    assert_eq!(
        r.after_failure("codex", &a, &e, 1, &[], 12.0),
        Recovery::WaitForRoute { until_secs: 132.0 }
    );
    // Admission blocks every job on the route; other routes are unaffected.
    assert_eq!(r.admission_blocked_until("codex", &a, 50.0), Some(132.0));
    assert_eq!(r.admission_blocked_until("codex", &b, 50.0), None);
    assert_eq!(r.admission_blocked_until("claude", &a, 50.0), None);
    assert_eq!(r.health(), vec![("codex".into(), a.clone(), Health::Open)]);
    // After the open period exactly one probe is admitted.
    assert_eq!(r.admission_blocked_until("codex", &a, 132.0), None);
    assert!(r.admission_blocked_until("codex", &a, 133.0).is_some());
    r.on_success("codex", &a);
    assert_eq!(r.admission_blocked_until("codex", &a, 134.0), None);
    assert!(r.health().is_empty());
    let states: Vec<_> = r.take_transitions().into_iter().map(|t| t.to).collect();
    assert_eq!(states, [Health::Degraded, Health::Open, Health::HalfOpen, Health::Healthy]);
    assert!(r.take_transitions().is_empty());
}

#[test]
fn i8_probe_failure_is_retryable_but_removed_model_is_permanent() {
    let r = Resilience::with_options(no_breaker(), fixed(1.0));
    let a = route("p", "m");
    let probe = classified(ErrorClass::RouteUnavailable, CODE_ROUTE_PROBE_FAILED);
    for (n, secs) in [(1, 60), (2, 120), (3, 300)] {
        assert_eq!(
            r.after_failure("claude", &a, &probe, n, &[], 0.0),
            Recovery::Retry {
                delay: Duration::from_secs(secs),
                attempt: n,
                max: 3,
                resume_session: None
            }
        );
    }
    assert!(matches!(
        r.after_failure("claude", &a, &probe, 4, &[], 0.0),
        Recovery::GiveUp { .. }
    ));
    let removed = classified(ErrorClass::RouteUnavailable, CODE_ROUTE_NOT_CONFIGURED);
    assert_eq!(
        r.after_failure("claude", &a, &removed, 1, &[], 0.0),
        Recovery::GiveUp {
            class: ErrorClass::RouteUnavailable,
            attempts: 1
        }
    );
}

#[test]
fn permanent_and_session_classes() {
    let r = Resilience::with_options(no_breaker(), fixed(1.0));
    let a = route("p", "m");
    for c in [
        ErrorClass::Auth,
        ErrorClass::Billing,
        ErrorClass::Permission,
        ErrorClass::Protocol,
        ErrorClass::InputTooLarge,
        ErrorClass::Other,
        ErrorClass::Cancelled,
    ] {
        assert!(matches!(r.after_failure("codex", &a, &err(c), 1, &[], 0.0), Recovery::GiveUp { .. }), "{c:?}");
    }
    let mut e = err(ErrorClass::SessionMismatch);
    e.session_id = Some("stale".into());
    assert!(matches!(
        r.after_failure("codex", &a, &e, 1, &[], 0.0),
        Recovery::Retry { resume_session: None, max: 1, .. }
    ));
    assert!(matches!(r.after_failure("codex", &a, &e, 2, &[], 0.0), Recovery::GiveUp { .. }));
}

// ------------------------------------------------------------ rotation

fn order() -> Vec<Route> {
    vec![route("a", "1"), route("b", "1"), route("c", "1")]
}

#[test]
fn rotation_off_never_rotates() {
    let r = Resilience::with_options(ResilienceConfig::default(), fixed(0.5));
    let a = route("a", "1");
    for c in [
        ErrorClass::Network,
        ErrorClass::RateLimited,
        ErrorClass::Overloaded,
        ErrorClass::Stalled,
        ErrorClass::ProcessCrashed,
        ErrorClass::RouteUnavailable,
        ErrorClass::Auth,
        ErrorClass::Billing,
    ] {
        let mut ledger = TurnLedger::default();
        for n in 1..=12 {
            let rec = r.after_failure("codex", &a, &err(c), n, &[], n as f64);
            assert!(!matches!(rec, Recovery::Rotate { .. }), "{c:?} {rec:?}");
            let rec = r.decide(&mut ledger, "codex", &a, &err(c), &[], n as f64);
            assert!(!matches!(rec, Recovery::Rotate { .. }), "{c:?} {rec:?}");
        }
    }
}

#[test]
fn rotation_on_moves_past_unavailable_provider() {
    let r = Resilience::with_options(no_breaker(), fixed(1.0));
    let mut ledger = TurnLedger::default();
    let rec = r.decide(
        &mut ledger,
        "codex",
        &route("a", "1"),
        &err(ErrorClass::RouteUnavailable),
        &order(),
        0.0,
    );
    assert_eq!(
        rec,
        Recovery::Rotate {
            to: route("b", "1"),
            delay: Duration::from_secs(2),
            attempt: 1,
            reason: ErrorClass::RouteUnavailable
        }
    );
    assert_eq!(ledger.rotations, 1);
    assert_eq!(ledger.current, Some(route("b", "1")));
    // The new route gets its own transient budget.
    let rec = r.decide(&mut ledger, "codex", &route("b", "1"), &err(ErrorClass::Network), &order(), 1.0);
    assert!(matches!(rec, Recovery::Retry { attempt: 1, max: 5, .. }), "{rec:?}");
    // Auth on the current provider is route-level too.
    let rec = r.decide(&mut ledger, "codex", &route("b", "1"), &err(ErrorClass::Auth), &order(), 2.0);
    assert!(matches!(rec, Recovery::Rotate { ref to, .. } if *to == route("c", "1")), "{rec:?}");
    // Job-level failures never rotate.
    let rec = r.decide(&mut ledger, "codex", &route("c", "1"), &err(ErrorClass::Protocol), &order(), 3.0);
    assert!(matches!(rec, Recovery::GiveUp { .. }));
}

#[test]
fn rotation_after_same_route_retries_are_exhausted() {
    let r = Resilience::with_options(no_breaker(), fixed(1.0));
    let mut ledger = TurnLedger::default();
    let a = route("a", "1");
    for n in 1..=3 {
        let rec = r.decide(&mut ledger, "codex", &a, &err(ErrorClass::Stalled), &order(), n as f64);
        assert!(matches!(rec, Recovery::Retry { .. }), "{rec:?}");
    }
    let rec = r.decide(&mut ledger, "codex", &a, &err(ErrorClass::Stalled), &order(), 4.0);
    assert!(matches!(rec, Recovery::Rotate { ref to, attempt: 4, .. } if *to == route("b", "1")), "{rec:?}");
}

#[test]
fn rotation_skips_open_breakers() {
    let r = Resilience::with_options(ResilienceConfig::default(), fixed(1.0));
    // Open b's breaker via three unrelated jobs.
    for t in 0..3 {
        r.after_failure("codex", &route("b", "1"), &err(ErrorClass::Overloaded), 1, &[], t as f64);
    }
    let mut ledger = TurnLedger::default();
    let rec = r.decide(
        &mut ledger,
        "codex",
        &route("a", "1"),
        &err(ErrorClass::RouteUnavailable),
        &order(),
        5.0,
    );
    assert!(matches!(rec, Recovery::Rotate { ref to, .. } if *to == route("c", "1")), "{rec:?}");
    // The current route opening its own breaker also triggers rotation.
    let mut ledger = TurnLedger::default();
    let c = route("c", "1");
    for t in 0..2 {
        let rec = r.decide(&mut ledger, "codex", &c, &err(ErrorClass::Overloaded), &order(), 10.0 + t as f64);
        assert!(matches!(rec, Recovery::Retry { .. }));
    }
    let rec = r.decide(&mut ledger, "codex", &c, &err(ErrorClass::Overloaded), &order(), 12.0);
    assert!(matches!(rec, Recovery::Rotate { ref to, .. } if *to == route("a", "1")), "{rec:?}");
    // Everything else open: no candidate, fall back to waiting on the route.
    for t in 0..3 {
        r.after_failure("codex", &route("a", "1"), &err(ErrorClass::Overloaded), 1, &[], 20.0 + t as f64);
    }
    let mut ledger = TurnLedger::default();
    let rec = r.decide(&mut ledger, "codex", &c, &err(ErrorClass::Overloaded), &order(), 30.0);
    assert!(matches!(rec, Recovery::WaitForRoute { .. }), "{rec:?}");
}

#[test]
fn rotation_gives_up_after_two_full_passes() {
    let r = Resilience::with_options(no_breaker(), fixed(1.0));
    let mut ledger = TurnLedger::default();
    let mut at = route("a", "1");
    let mut rotations = 0;
    let mut delays = Vec::new();
    loop {
        match r.decide(&mut ledger, "codex", &at, &err(ErrorClass::RouteUnavailable), &order(), 0.0) {
            Recovery::Rotate { to, delay, .. } => {
                rotations += 1;
                delays.push(delay.as_secs());
                at = to;
            }
            Recovery::GiveUp { class, attempts } => {
                assert_eq!(class, ErrorClass::RouteUnavailable);
                assert_eq!(attempts, 7);
                break;
            }
            other => panic!("{other:?}"),
        }
        assert!(rotations <= 6);
    }
    assert_eq!(rotations, 6);
    // Each full pass doubles the rotation delay.
    assert_eq!(delays, [2, 2, 2, 4, 4, 4]);

    // The stateless entry point is bounded too.
    let mut at = route("a", "1");
    let mut steps = 0;
    for attempt in 1.. {
        steps += 1;
        assert!(steps < 50, "stateless rotation must terminate");
        match r.after_failure("codex", &at, &err(ErrorClass::Network), attempt, &order(), 0.0) {
            Recovery::Rotate { to, .. } => at = to,
            Recovery::GiveUp { .. } => break,
            Recovery::Retry { .. } => {}
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn custom_limits_apply() {
    let opts = ResilienceOptions {
        limits: RetryLimits {
            transient: 1,
            ..Default::default()
        },
        ..fixed(1.0)
    };
    let r = Resilience::with_options(no_breaker(), opts);
    let a = route("a", "1");
    assert!(matches!(r.after_failure("codex", &a, &err(ErrorClass::Network), 1, &[], 0.0), Recovery::Retry { max: 1, .. }));
    assert!(matches!(r.after_failure("codex", &a, &err(ErrorClass::Network), 2, &[], 0.0), Recovery::GiveUp { .. }));
}
