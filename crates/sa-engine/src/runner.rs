//! How the engine runs one native turn and what it does after a failure.
//!
//! The engine is generic over [`TurnRunner`]. [`ResilientRunner`] (the
//! default) delegates to [`Supervisor::run_turn`] and [`Resilience`];
//! [`PassThroughRunner`] calls the adapter directly and keeps the legacy JS
//! failure handling (auto-root regex retry, E:486-540).

use crate::adapter::{Adapter, AdapterError, AdapterEvent, Route, RunRequest, TurnHost};
use crate::resilience::{
    Health, HealthTransition, Recovery, Resilience, ResilienceConfig, Supervisor, TurnLedger, TurnResult,
};
use futures::future::BoxFuture;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Legacy per-turn wall clock (E:26).
pub const LEGACY_TURN_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub trait TurnRunner: Send + Sync {
    /// Run one native turn, forwarding every adapter event to `forward`.
    fn run_turn(
        &self,
        adapter: Arc<dyn Adapter>,
        request: RunRequest,
        host: Arc<dyn TurnHost>,
        forward: mpsc::UnboundedSender<AdapterEvent>,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, TurnResult>;

    /// Recovery after a failed attempt; `None` selects the legacy JS rules.
    fn after_failure(
        &self,
        _agent: &str,
        _route: &Route,
        _error: &AdapterError,
        _attempt: u32,
        _rotation: &[Route],
        _now_secs: f64,
    ) -> Option<Recovery> {
        None
    }

    /// Recovery with exact per-turn bookkeeping (the engine rebuilds
    /// `ledger` from the journal); `None` selects the legacy JS rules.
    fn decide(
        &self,
        _ledger: &mut TurnLedger,
        _agent: &str,
        _route: &Route,
        _error: &AdapterError,
        _rotation: &[Route],
        _now_secs: f64,
    ) -> Option<Recovery> {
        None
    }

    fn on_success(&self, _agent: &str, _route: &Route) {}

    /// Breaker state changes since the last call (journaled as `harness.health`).
    fn take_transitions(&self) -> Vec<HealthTransition> {
        vec![]
    }

    fn admission_blocked_until(&self, _agent: &str, _route: &Route, _now_secs: f64) -> Option<f64> {
        None
    }

    fn health(&self) -> Vec<(String, Route, Health)> {
        vec![]
    }

    /// Engine-level wall clock per turn. `None` when the runner enforces its
    /// own hard cap.
    fn wall_clock(&self) -> Option<Duration> {
        Some(LEGACY_TURN_TIMEOUT)
    }
}

/// Calls `Adapter::run` directly; legacy failure handling.
#[derive(Default)]
pub struct PassThroughRunner {
    /// Override of the legacy 30-minute wall clock (tests).
    pub wall_clock: Option<Duration>,
}

impl TurnRunner for PassThroughRunner {
    fn run_turn(
        &self,
        adapter: Arc<dyn Adapter>,
        request: RunRequest,
        host: Arc<dyn TurnHost>,
        forward: mpsc::UnboundedSender<AdapterEvent>,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, TurnResult> {
        Box::pin(async move {
            match adapter.run(request, host, forward, cancel).await {
                Ok(outcome) => TurnResult::Done(outcome),
                Err(err) => TurnResult::Failed(err),
            }
        })
    }

    fn wall_clock(&self) -> Option<Duration> {
        Some(self.wall_clock.unwrap_or(LEGACY_TURN_TIMEOUT))
    }
}

/// Supervised turns with classification, bounded retry, breaker and rotation.
pub struct ResilientRunner {
    config: ResilienceConfig,
    resilience: Resilience,
}

impl ResilientRunner {
    pub fn new(config: ResilienceConfig) -> Self {
        let resilience = Resilience::new(config.clone());
        Self { config, resilience }
    }

    /// Custom retry limits, rotation passes, RNG or clock (tests).
    pub fn with_resilience(resilience: Resilience) -> Self {
        Self {
            config: resilience.config().clone(),
            resilience,
        }
    }
}

impl Default for ResilientRunner {
    fn default() -> Self {
        Self::new(ResilienceConfig::default())
    }
}

impl TurnRunner for ResilientRunner {
    fn run_turn(
        &self,
        adapter: Arc<dyn Adapter>,
        request: RunRequest,
        host: Arc<dyn TurnHost>,
        forward: mpsc::UnboundedSender<AdapterEvent>,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, TurnResult> {
        let config = self.config.clone();
        Box::pin(async move { Supervisor::run_turn(&config, adapter, request, host, forward, cancel).await })
    }

    fn after_failure(
        &self,
        agent: &str,
        route: &Route,
        error: &AdapterError,
        attempt: u32,
        rotation: &[Route],
        now_secs: f64,
    ) -> Option<Recovery> {
        Some(self.resilience.after_failure(agent, route, error, attempt, rotation, now_secs))
    }

    fn decide(
        &self,
        ledger: &mut TurnLedger,
        agent: &str,
        route: &Route,
        error: &AdapterError,
        rotation: &[Route],
        now_secs: f64,
    ) -> Option<Recovery> {
        Some(self.resilience.decide(ledger, agent, route, error, rotation, now_secs))
    }

    fn on_success(&self, agent: &str, route: &Route) {
        self.resilience.on_success(agent, route)
    }

    fn take_transitions(&self) -> Vec<HealthTransition> {
        self.resilience.take_transitions()
    }

    fn admission_blocked_until(&self, agent: &str, route: &Route, now_secs: f64) -> Option<f64> {
        self.resilience.admission_blocked_until(agent, route, now_secs)
    }

    fn health(&self) -> Vec<(String, Route, Health)> {
        self.resilience.health()
    }

    /// `Supervisor` enforces `turn_hard_cap` and stall timeouts instead.
    fn wall_clock(&self) -> Option<Duration> {
        None
    }
}

/// Ordered rotation candidates per agent; empty when rotation is off.
pub trait RotationSource: Send + Sync {
    fn candidates(&self, agent: &str) -> Vec<Route>;
}

/// Rotation disabled.
pub struct NoRotation;

impl RotationSource for NoRotation {
    fn candidates(&self, _agent: &str) -> Vec<Route> {
        vec![]
    }
}

/// In-memory rotation list (tests; the provider registry supplies a
/// DB-backed implementation later).
#[derive(Default)]
pub struct StaticRotation(pub HashMap<String, Vec<Route>>);

impl RotationSource for StaticRotation {
    fn candidates(&self, agent: &str) -> Vec<Route> {
        self.0.get(agent).cloned().unwrap_or_default()
    }
}
