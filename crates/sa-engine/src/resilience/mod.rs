//! Resilience layer (spec §9.2 plus operator-requested provider rotation).
//!
//! The engine calls [`Supervisor::run_turn`] instead of `Adapter::run`
//! directly. It forwards adapter events, enforces stall timeouts, classifies
//! failures and returns a [`TurnResult`]. The engine then asks
//! [`Resilience::after_failure`] what to do next.
//!
//! OWNER: resilience builder. The engine builder codes against these public
//! signatures; change them only with coordination.

pub mod breaker;
pub mod classify;
pub mod policy;
pub mod rotation;
pub mod watchdog;

use crate::adapter::{
    Adapter, AdapterError, AdapterEvent, ErrorClass, Route, RunOutcome, RunRequest, TurnHost,
};
pub use policy::{
    CODE_ROUTE_NOT_CONFIGURED, CODE_ROUTE_PROBE_FAILED, HealthTransition, ResilienceOptions,
    RetryLimits, TurnLedger, backoff,
};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
pub use watchdog::{STALLED_KIND, StallPhase};

/// Timeouts and retry limits (spec §9.2 A). Defaults are proposals.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResilienceConfig {
    pub first_frame: Duration,
    pub model_idle: Duration,
    pub tool_idle: Duration,
    pub turn_hard_cap: Duration,
    pub interrupt_grace: Duration,
    pub retry_base: Duration,
    pub retry_cap: Duration,
    pub breaker_threshold: u32,
    pub breaker_window: Duration,
    pub breaker_open: Duration,
}

impl Default for ResilienceConfig {
    fn default() -> Self {
        Self {
            first_frame: Duration::from_secs(60),
            model_idle: Duration::from_secs(360),
            tool_idle: Duration::from_secs(1800),
            turn_hard_cap: Duration::from_secs(4 * 3600),
            interrupt_grace: Duration::from_millis(2400),
            retry_base: Duration::from_secs(2),
            retry_cap: Duration::from_secs(300),
            breaker_threshold: 3,
            breaker_window: Duration::from_secs(600),
            breaker_open: Duration::from_secs(120),
        }
    }
}

/// What happened in one supervised attempt.
#[derive(Debug)]
pub enum TurnResult {
    Done(RunOutcome),
    Failed(AdapterError),
}

/// Decision after a failed attempt.
#[derive(Clone, Debug, PartialEq)]
pub enum Recovery {
    /// Retry the same route after `delay`, resuming `session` when present.
    Retry {
        delay: Duration,
        attempt: u32,
        max: u32,
        resume_session: Option<String>,
    },
    /// Rotation is enabled and another provider/model is healthy: retry there
    /// with a fresh native session.
    Rotate {
        to: Route,
        delay: Duration,
        attempt: u32,
        reason: ErrorClass,
    },
    /// Route is circuit-open; wait without consuming an attempt.
    WaitForRoute { until_secs: f64 },
    /// Give up; the job fails with the classified message.
    GiveUp { class: ErrorClass, attempts: u32 },
}

/// Health of one `(agent, provider, model)` route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Healthy,
    Degraded,
    Open,
    HalfOpen,
}

/// Runs one adapter turn under a stall watchdog.
pub struct Supervisor;

impl Supervisor {
    /// Run `adapter.run`, forwarding every event to `forward`. On stall or
    /// hard cap it cancels the native turn and returns `Failed(Stalled)`.
    /// `cancel` is the engine's own cancellation (stop/follow) and maps to
    /// `ErrorClass::Cancelled`.
    ///
    /// Must be called inside a tokio runtime (the adapter future is spawned).
    /// Besides the adapter's own events, one `Journal{kind:"turn.stalled"}`
    /// is forwarded when the watchdog fires. A closed `forward` channel is
    /// treated like engine cancellation. The failure's `session_id` is the
    /// newest native session seen (adapter error, `Session` event, or the
    /// resumed one) so a retry can resume it.
    pub async fn run_turn(
        config: &ResilienceConfig,
        adapter: Arc<dyn Adapter>,
        request: RunRequest,
        host: Arc<dyn TurnHost>,
        forward: mpsc::UnboundedSender<AdapterEvent>,
        cancel: CancellationToken,
    ) -> TurnResult {
        watchdog::run_turn(config, adapter, request, host, forward, cancel).await
    }
}

/// Shared retry/breaker/rotation state for one engine instance (thread-safe).
pub struct Resilience {
    policy: policy::Policy,
}

impl Resilience {
    pub fn new(config: ResilienceConfig) -> Self {
        Self::with_options(config, ResilienceOptions::default())
    }
    /// Custom limits, rotation passes, RNG and clock (tests inject constants).
    pub fn with_options(config: ResilienceConfig, options: ResilienceOptions) -> Self {
        Self {
            policy: policy::Policy::new(config, options),
        }
    }
    pub fn config(&self) -> &ResilienceConfig {
        &self.policy.config
    }
    pub fn limits(&self) -> &RetryLimits {
        &self.policy.limits
    }
    /// Decide the next step after a failed attempt of a job on `route`.
    /// `attempt` counts failures of this logical turn so far (1-based).
    /// `rotation` lists the operator's ordered candidate routes for this agent
    /// when the rotation switch is on (empty means off).
    ///
    /// Stateless approximation: it assumes all earlier failures happened on
    /// `route`. Prefer [`Resilience::decide`] with a persisted
    /// [`TurnLedger`], which gives every rotated-to route its own budget.
    pub fn after_failure(
        &self,
        agent: &str,
        route: &Route,
        error: &AdapterError,
        attempt: u32,
        rotation: &[Route],
        now_secs: f64,
    ) -> Recovery {
        self.policy
            .after_failure(agent, route, error, attempt, rotation, now_secs)
    }
    /// Same decision with exact per-turn bookkeeping. `ledger` starts as
    /// `TurnLedger::default()` for each logical turn and is updated in place
    /// (failure counted; on `Rotate` the current route switches).
    pub fn decide(
        &self,
        ledger: &mut TurnLedger,
        agent: &str,
        route: &Route,
        error: &AdapterError,
        rotation: &[Route],
        now_secs: f64,
    ) -> Recovery {
        self.policy
            .decide(ledger, agent, route, error, rotation, now_secs)
    }
    /// Record a successful turn (closes a half-open breaker).
    pub fn on_success(&self, agent: &str, route: &Route) {
        self.policy.on_success(agent, route)
    }
    /// `Some(until)` when jobs on `route` must not be admitted yet. When an
    /// open breaker's period has elapsed, the first caller is admitted as the
    /// half-open probe; report its outcome via `on_success`/`decide`.
    pub fn admission_blocked_until(&self, agent: &str, route: &Route, now_secs: f64) -> Option<f64> {
        self.policy.admission_blocked_until(agent, route, now_secs)
    }
    /// Health snapshot for envelopes and UI. Routes not listed are healthy.
    pub fn health(&self) -> Vec<(String, Route, Health)> {
        self.policy.health()
    }
    /// Drain breaker state changes (journal them as `harness.health`).
    pub fn take_transitions(&self) -> Vec<HealthTransition> {
        self.policy.take_transitions()
    }
}
