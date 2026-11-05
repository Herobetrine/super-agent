//! Stall watchdog around one adapter turn (spec §9.2 B).
//!
//! Every adapter event is a heartbeat. The idle limit depends on the phase:
//! `first_frame` until the first event, `tool_idle` while any tool is open,
//! `model_idle` otherwise. `AwaitingApproval` (and any in-flight
//! `TurnHost::approve`) pauses the idle timer; `NativeRetry{after}` extends
//! it by `after` until the next frame. `turn_hard_cap` always applies.
//! On breach the adapter's own token is cancelled, the adapter gets
//! `interrupt_grace` to wind down, and the turn fails as `Stalled`.

use super::classify::classified;
use super::{ResilienceConfig, TurnResult};
use crate::adapter::{
    Adapter, AdapterError, AdapterEvent, ErrorClass, RunOutcome, RunRequest, TurnHost,
};
use futures::FutureExt;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Journal kind forwarded once when the watchdog fires.
pub const STALLED_KIND: &str = "turn.stalled";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StallPhase {
    FirstFrame,
    ModelIdle,
    ToolIdle,
    HardCap,
}

impl StallPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FirstFrame => "first_frame",
            Self::ModelIdle => "model_idle",
            Self::ToolIdle => "tool_idle",
            Self::HardCap => "hard_cap",
        }
    }
    fn zh(self) -> &'static str {
        match self {
            Self::FirstFrame => "启动后迟迟没有输出",
            Self::ModelIdle => "等待模型输出时长时间无响应",
            Self::ToolIdle => "工具执行期间长时间无响应",
            Self::HardCap => "超过单轮执行时长上限",
        }
    }
}

enum Ctl {
    ApprovalStarted,
    ApprovalEnded,
}

/// Wraps the engine host so in-flight approvals pause the idle timer.
struct WatchedHost {
    inner: Arc<dyn TurnHost>,
    ctl: mpsc::UnboundedSender<Ctl>,
}

struct ApprovalGuard(mpsc::UnboundedSender<Ctl>);
impl Drop for ApprovalGuard {
    fn drop(&mut self) {
        let _ = self.0.send(Ctl::ApprovalEnded);
    }
}

impl TurnHost for WatchedHost {
    fn approve(&self, action: Value) -> BoxFuture<'static, bool> {
        let _ = self.ctl.send(Ctl::ApprovalStarted);
        let guard = ApprovalGuard(self.ctl.clone());
        let fut = self.inner.approve(action);
        async move {
            let ok = fut.await;
            drop(guard);
            ok
        }
        .boxed()
    }
    fn pending_notes(&self) -> Vec<Value> {
        self.inner.pending_notes()
    }
    fn accept_notes(&self, seqs: &[i64]) {
        self.inner.accept_notes(seqs)
    }
}

struct Watch<'a> {
    cfg: &'a ResilienceConfig,
    started: Instant,
    last: Instant,
    seen_frame: bool,
    open_tools: BTreeSet<String>,
    awaiting: bool,
    approvals: usize,
    extension: Duration,
    session: Option<String>,
}

impl<'a> Watch<'a> {
    fn new(cfg: &'a ResilienceConfig, now: Instant) -> Self {
        Self {
            cfg,
            started: now,
            last: now,
            seen_frame: false,
            open_tools: BTreeSet::new(),
            awaiting: false,
            approvals: 0,
            extension: Duration::ZERO,
            session: None,
        }
    }

    fn idle_phase(&self) -> (StallPhase, Duration) {
        if !self.seen_frame {
            (StallPhase::FirstFrame, self.cfg.first_frame)
        } else if !self.open_tools.is_empty() {
            (StallPhase::ToolIdle, self.cfg.tool_idle)
        } else {
            (StallPhase::ModelIdle, self.cfg.model_idle)
        }
    }

    fn deadline(&self) -> (Instant, StallPhase) {
        let hard = self.started + self.cfg.turn_hard_cap;
        if self.awaiting || self.approvals > 0 {
            return (hard, StallPhase::HardCap);
        }
        let (phase, limit) = self.idle_phase();
        let idle = self.last + limit + self.extension;
        if idle < hard { (idle, phase) } else { (hard, StallPhase::HardCap) }
    }

    fn on_event(&mut self, ev: &AdapterEvent, now: Instant) {
        self.seen_frame = true;
        self.last = now;
        if !matches!(ev, AdapterEvent::NativeRetry { .. }) {
            self.extension = Duration::ZERO;
        }
        if !matches!(ev, AdapterEvent::AwaitingApproval) {
            self.awaiting = false;
        }
        match ev {
            AdapterEvent::ToolStarted(id) => {
                self.open_tools.insert(id.clone());
            }
            AdapterEvent::ToolFinished(id) => {
                self.open_tools.remove(id);
            }
            AdapterEvent::NativeRetry { after } => {
                self.extension = after.unwrap_or(Duration::ZERO).min(self.cfg.retry_cap);
            }
            AdapterEvent::AwaitingApproval => self.awaiting = true,
            AdapterEvent::Session(id) => self.session = Some(id.clone()),
            AdapterEvent::Journal { .. } => {}
        }
    }

    fn on_ctl(&mut self, ctl: Ctl, now: Instant) {
        match ctl {
            Ctl::ApprovalStarted => self.approvals += 1,
            Ctl::ApprovalEnded => {
                self.approvals = self.approvals.saturating_sub(1);
                if self.approvals == 0 {
                    // The decision is itself activity; restart the idle clock.
                    self.awaiting = false;
                    self.last = now;
                }
            }
        }
    }
}

fn cancelled_error(session: Option<String>) -> AdapterError {
    let mut e = classified(ErrorClass::Cancelled, "cancelled");
    e.session_id = session;
    e
}

/// Give the adapter `grace` to finish, then abort its task. Never blocks
/// longer than `grace`, even if the adapter ignores its token or blocks.
async fn settle(
    task: &mut JoinHandle<Result<RunOutcome, AdapterError>>,
    grace: Duration,
) -> Option<Result<RunOutcome, AdapterError>> {
    match tokio::time::timeout(grace, &mut *task).await {
        Ok(Ok(res)) => Some(res),
        Ok(Err(_)) => None,
        Err(_) => {
            task.abort();
            None
        }
    }
}

pub(crate) async fn run_turn(
    config: &ResilienceConfig,
    adapter: Arc<dyn Adapter>,
    request: RunRequest,
    host: Arc<dyn TurnHost>,
    forward: mpsc::UnboundedSender<AdapterEvent>,
    cancel: CancellationToken,
) -> TurnResult {
    if cancel.is_cancelled() {
        return TurnResult::Failed(cancelled_error(request.session_id.clone()));
    }
    // Child token: engine cancellation reaches the adapter, and the watchdog
    // can cancel the adapter without touching the engine's token.
    let token = cancel.child_token();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel();
    let host: Arc<dyn TurnHost> = Arc::new(WatchedHost {
        inner: host,
        ctl: ctl_tx,
    });
    let resumed = request.session_id.clone();
    let mut task = tokio::spawn(adapter.run(request, host, tx, token.clone()));
    let mut w = Watch::new(config, Instant::now());
    let mut rx_open = true;
    let mut ctl_open = true;

    let outcome = loop {
        let (deadline, phase) = w.deadline();
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let late = settle(&mut task, config.interrupt_grace).await;
                let session = match late {
                    Some(Err(e)) => e.session_id.or(w.session.take()),
                    _ => w.session.take(),
                };
                return TurnResult::Failed(cancelled_error(session.or(resumed)));
            }
            ev = rx.recv(), if rx_open => match ev {
                Some(ev) => {
                    w.on_event(&ev, Instant::now());
                    if forward.send(ev).is_err() {
                        // The engine discarded this turn: stop the adapter.
                        token.cancel();
                        settle(&mut task, config.interrupt_grace).await;
                        return TurnResult::Failed(cancelled_error(w.session.take().or(resumed)));
                    }
                }
                None => rx_open = false,
            },
            ctl = ctl_rx.recv(), if ctl_open => match ctl {
                Some(c) => w.on_ctl(c, Instant::now()),
                None => ctl_open = false,
            },
            res = &mut task => break res,
            _ = tokio::time::sleep_until(deadline) => {
                let now = Instant::now();
                let idle = now.saturating_duration_since(if phase == StallPhase::HardCap { w.started } else { w.last });
                let _ = forward.send(AdapterEvent::Journal {
                    kind: STALLED_KIND.into(),
                    data: json!({
                        "phase": phase.as_str(),
                        "idle_ms": idle.as_millis() as u64,
                        "open_tools": w.open_tools.iter().take(16).collect::<Vec<_>>(),
                        "open_tool_count": w.open_tools.len(),
                    }),
                });
                token.cancel();
                let late = settle(&mut task, config.interrupt_grace).await;
                let mut e = classified(ErrorClass::Stalled, &format!("stall:{}", phase.as_str()));
                e.message = if phase == StallPhase::HardCap {
                    format!("执行端{}（{} 分钟），已中断本轮", phase.zh(), config.turn_hard_cap.as_secs() / 60)
                } else {
                    format!("执行端{}（已空闲 {} 秒），已中断本轮", phase.zh(), idle.as_secs())
                };
                let late_session = match late {
                    Some(Err(err)) => err.session_id,
                    Some(Ok(out)) => out.session_id,
                    None => None,
                };
                e.session_id = w.session.take().or(late_session).or(resumed);
                return TurnResult::Failed(e);
            }
        }
    };

    // Forward frames the adapter queued before it returned.
    while let Ok(ev) = rx.try_recv() {
        w.on_event(&ev, Instant::now());
        if forward.send(ev).is_err() {
            break;
        }
    }
    match outcome {
        Ok(Ok(out)) => TurnResult::Done(out),
        Ok(Err(mut e)) => {
            if cancel.is_cancelled() {
                return TurnResult::Failed(cancelled_error(e.session_id.or(w.session.take()).or(resumed)));
            }
            if e.session_id.is_none() {
                e.session_id = w.session.take().or(resumed);
            }
            TurnResult::Failed(e)
        }
        Err(join) => {
            let mut e = classified(ErrorClass::Other, if join.is_panic() { "adapter:panic" } else { "adapter:aborted" });
            e.message = "执行端适配器异常退出".into();
            e.session_id = w.session.take().or(resumed);
            TurnResult::Failed(e)
        }
    }
}
