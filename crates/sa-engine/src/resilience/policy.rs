//! Retry policy, per-route breakers and rotation decisions (spec §9.2 D/E
//! plus operator-requested provider rotation).

use super::breaker::{Breaker, Change};
use super::classify::{MAX_RETRY_AFTER, is_route_health};
use super::rotation::{next_candidate, same_route};
use super::{Health, Recovery, ResilienceConfig};
use crate::adapter::{AdapterError, ErrorClass, Route};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Adapter `code` prefix for a provider/model that is removed from the
/// configuration (permanent `RouteUnavailable`).
pub const CODE_ROUTE_NOT_CONFIGURED: &str = "route:not_configured";
/// Adapter `code` for a catalog probe that failed (timeout/IO); retryable.
pub const CODE_ROUTE_PROBE_FAILED: &str = "route:probe_failed";

/// Per-class retry budgets on one route within one logical turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryLimits {
    /// Network, RateLimited, Overloaded.
    pub transient: u32,
    pub crash: u32,
    pub stall: u32,
    pub route_unavailable: u32,
    /// Retry once with a fresh native session.
    pub session_mismatch: u32,
}

impl Default for RetryLimits {
    fn default() -> Self {
        Self {
            transient: 5,
            crash: 3,
            stall: 3,
            route_unavailable: 3,
            session_mismatch: 1,
        }
    }
}

impl RetryLimits {
    /// Retries allowed for this failure on the same route (0 = permanent).
    pub fn for_error(&self, error: &AdapterError) -> u32 {
        if error.native_retry_exhausted() {
            return 0;
        }
        match error.class {
            ErrorClass::Network | ErrorClass::RateLimited | ErrorClass::Overloaded => self.transient,
            ErrorClass::ProcessCrashed => self.crash,
            ErrorClass::Stalled => self.stall,
            ErrorClass::RouteUnavailable
                if error
                    .code
                    .as_deref()
                    .is_some_and(|c| c.starts_with(CODE_ROUTE_NOT_CONFIGURED)) =>
            {
                0
            }
            ErrorClass::RouteUnavailable => self.route_unavailable,
            ErrorClass::SessionMismatch => self.session_mismatch,
            _ => 0,
        }
    }
}

/// Backoff ladder for retryable `RouteUnavailable` (spec §9.2 C), seconds.
const ROUTE_LADDER: [u64; 3] = [60, 120, 300];

/// `min(cap, base·2^n) · (0.5 + 0.5·u)` with `u ∈ [0, 1]`.
pub fn backoff(base: Duration, cap: Duration, n: u32, u: f64) -> Duration {
    let exp = base.as_secs_f64() * 2f64.powi(n.min(62) as i32);
    let d = exp.min(cap.as_secs_f64()).max(0.0);
    let u = if u.is_finite() { u.clamp(0.0, 1.0) } else { 0.0 };
    Duration::from_secs_f64(d * (0.5 + 0.5 * u))
}

/// Seconds since the unix epoch.
pub fn wall_clock() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Default uniform `[0, 1)` source (SplitMix64; no cryptographic needs).
pub fn default_rng() -> Box<dyn FnMut() -> f64 + Send> {
    let mut state = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0x9E37_79B9_7F4A_7C15, |d| d.as_nanos() as u64)
        ^ (std::process::id() as u64).rotate_left(32);
    Box::new(move || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64
    })
}

/// Injectable dependencies and tunables of [`super::Resilience`].
pub struct ResilienceOptions {
    pub limits: RetryLimits,
    /// Full passes over the rotation order allowed per logical turn.
    pub rotation_passes: u32,
    /// Uniform `[0, 1)`; tests inject a constant.
    pub rng: Box<dyn FnMut() -> f64 + Send>,
    /// Seconds since the epoch; used where no `now` is passed (`health`).
    pub clock: Arc<dyn Fn() -> f64 + Send + Sync>,
}

impl Default for ResilienceOptions {
    fn default() -> Self {
        Self {
            limits: RetryLimits::default(),
            rotation_passes: 2,
            rng: default_rng(),
            clock: Arc::new(wall_clock),
        }
    }
}

/// Per-logical-turn failure bookkeeping. The engine keeps one per job turn
/// (it is serde so it can be persisted with the job) and passes it to
/// [`super::Resilience::decide`]; `after_failure` derives one from `attempt`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnLedger {
    /// All failures of this logical turn.
    pub failures: u32,
    /// Failures on `current` since it was selected.
    pub route_failures: u32,
    /// Rotations performed in this logical turn.
    pub rotations: u32,
    /// Route of the most recent attempt.
    pub current: Option<Route>,
}

impl TurnLedger {
    fn note_failure(&mut self, route: &Route) {
        if !self.current.as_ref().is_some_and(|c| same_route(c, route)) {
            self.current = Some(route.clone());
            self.route_failures = 0;
        }
        self.failures += 1;
        self.route_failures += 1;
    }
}

/// A breaker state change, for `harness.health` journal events.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HealthTransition {
    pub agent: String,
    pub route: Route,
    pub from: Health,
    pub to: Health,
    pub reason_class: Option<ErrorClass>,
    /// Open-until (epoch seconds) when `to` is `Open`.
    pub until: Option<f64>,
    pub at: f64,
}

const MAX_PENDING_TRANSITIONS: usize = 1024;

type RouteKey = (String, String, String);

fn key(agent: &str, route: &Route) -> RouteKey {
    (agent.to_owned(), route.provider.clone(), route.model.clone())
}

pub(crate) struct Inner {
    rng: Box<dyn FnMut() -> f64 + Send>,
    breakers: HashMap<RouteKey, Breaker>,
    transitions: Vec<HealthTransition>,
}

pub(crate) struct Policy {
    pub config: ResilienceConfig,
    pub limits: RetryLimits,
    pub rotation_passes: u32,
    pub clock: Arc<dyn Fn() -> f64 + Send + Sync>,
    inner: Mutex<Inner>,
}

impl Policy {
    pub fn new(config: ResilienceConfig, options: ResilienceOptions) -> Self {
        Self {
            config,
            limits: options.limits,
            rotation_passes: options.rotation_passes,
            clock: options.clock,
            inner: Mutex::new(Inner {
                rng: options.rng,
                breakers: HashMap::new(),
                transitions: Vec::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock cannot leave the maps inconsistent
        // in a way that matters more than losing health data; keep going.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn record(inner: &mut Inner, agent: &str, route: &Route, b: &Breaker, change: Change, now: f64) {
        if let Some((from, to)) = change {
            if inner.transitions.len() >= MAX_PENDING_TRANSITIONS {
                inner.transitions.remove(0);
            }
            inner.transitions.push(HealthTransition {
                agent: agent.to_owned(),
                route: Route {
                    provider: route.provider.clone(),
                    model: route.model.clone(),
                    mode: b.mode.clone(),
                    reasoning_effort: route.reasoning_effort.clone(),
                },
                from,
                to,
                reason_class: if to == Health::Healthy { None } else { b.last_class },
                until: (to == Health::Open).then_some(b.open_until),
                at: now,
            });
        }
    }

    fn blocked(&self, inner: &Inner, agent: &str, route: &Route, now: f64) -> bool {
        inner
            .breakers
            .get(&key(agent, route))
            .and_then(|b| b.blocked_until(now, &self.config))
            .is_some()
    }

    /// Stateless form: assumes every earlier failure of the turn happened on
    /// `route` and that failures beyond the class budget were rotations.
    pub fn after_failure(
        &self,
        agent: &str,
        route: &Route,
        error: &AdapterError,
        attempt: u32,
        rotation: &[Route],
        now: f64,
    ) -> Recovery {
        let prior = attempt.max(1) - 1;
        let limit = self.limits.for_error(error);
        let mut ledger = TurnLedger {
            failures: prior,
            route_failures: prior,
            rotations: prior.saturating_sub(limit),
            current: Some(route.clone()),
        };
        self.decide(&mut ledger, agent, route, error, rotation, now)
    }

    pub fn decide(
        &self,
        ledger: &mut TurnLedger,
        agent: &str,
        route: &Route,
        error: &AdapterError,
        rotation: &[Route],
        now: f64,
    ) -> Recovery {
        ledger.note_failure(route);
        let class = error.class;
        if class == ErrorClass::Cancelled {
            return Recovery::GiveUp {
                class,
                attempts: ledger.failures,
            };
        }
        let cfg = &self.config;
        let mut guard = self.lock();
        let inner = &mut *guard;

        let mut open_until = None;
        if is_route_health(class) {
            let b = inner
                .breakers
                .entry(key(agent, route))
                .or_insert_with(|| Breaker::new(&route.mode));
            let change = b.on_failure(now, cfg, class);
            if b.state == Health::Open {
                open_until = Some(b.open_until);
            }
            let snapshot = b.clone();
            Self::record(inner, agent, route, &snapshot, change, now);
        }

        if error.native_retry_exhausted() {
            return Recovery::GiveUp { class, attempts: ledger.failures };
        }
        let limit = self.limits.for_error(error);
        let exhausted = ledger.route_failures > limit;
        let route_level = matches!(
            class,
            ErrorClass::RouteUnavailable | ErrorClass::Auth | ErrorClass::Billing
        ) || open_until.is_some();
        let same_route_retryable = matches!(
            class,
            ErrorClass::Network
                | ErrorClass::RateLimited
                | ErrorClass::Overloaded
                | ErrorClass::Stalled
                | ErrorClass::ProcessCrashed
        );

        if !rotation.is_empty() && (route_level || (exhausted && same_route_retryable)) {
            let len = rotation.len() as u32;
            if ledger.rotations >= self.rotation_passes.saturating_mul(len) {
                return Recovery::GiveUp {
                    class,
                    attempts: ledger.failures,
                };
            }
            let pick = next_candidate(rotation, route, class, |r| self.blocked(inner, agent, r, now));
            if let Some(i) = pick {
                let to = rotation[i].clone();
                let pass = ledger.rotations / len;
                let u = (inner.rng)();
                let delay = backoff(cfg.retry_base, cfg.retry_cap, pass, u);
                ledger.rotations += 1;
                ledger.current = Some(to.clone());
                ledger.route_failures = 0;
                return Recovery::Rotate {
                    to,
                    delay,
                    attempt: ledger.failures,
                    reason: class,
                };
            }
        }

        if limit == 0 || exhausted {
            return Recovery::GiveUp {
                class,
                attempts: ledger.failures,
            };
        }
        if let Some(until) = open_until {
            return Recovery::WaitForRoute { until_secs: until };
        }
        let n = ledger.route_failures - 1;
        let u = (inner.rng)();
        let mut delay = if class == ErrorClass::RouteUnavailable {
            let step = ROUTE_LADDER[(n as usize).min(ROUTE_LADDER.len() - 1)];
            backoff(Duration::from_secs(step), Duration::from_secs(step), 0, u)
        } else {
            backoff(cfg.retry_base, cfg.retry_cap, n, u)
        };
        if let Some(after) = error.retry_after {
            delay = delay.max(after.min(MAX_RETRY_AFTER));
        }
        let resume_session = if class == ErrorClass::SessionMismatch {
            None
        } else {
            error.session_id.clone()
        };
        Recovery::Retry {
            delay,
            attempt: ledger.route_failures,
            max: limit,
            resume_session,
        }
    }

    pub fn on_success(&self, agent: &str, route: &Route) {
        let now = (self.clock)();
        let mut guard = self.lock();
        let inner = &mut *guard;
        if let Some(mut b) = inner.breakers.remove(&key(agent, route)) {
            let change = b.on_success();
            Self::record(inner, agent, route, &b, change, now);
        }
    }

    pub fn admission_blocked_until(&self, agent: &str, route: &Route, now: f64) -> Option<f64> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let k = key(agent, route);
        let b = inner.breakers.get_mut(&k)?;
        let mut changes = vec![b.refresh(now, &self.config)];
        let (until, change) = b.admit(now, &self.config);
        changes.push(change);
        let snapshot = b.clone();
        for c in changes {
            Self::record(inner, agent, route, &snapshot, c, now);
        }
        if snapshot.state == Health::Healthy {
            inner.breakers.remove(&k);
        }
        until
    }

    pub fn health(&self) -> Vec<(String, Route, Health)> {
        let now = (self.clock)();
        let mut guard = self.lock();
        let inner = &mut *guard;
        let mut out = Vec::new();
        let mut healed = Vec::new();
        for (k, b) in inner.breakers.iter_mut() {
            if let Some(change) = b.refresh(now, &self.config) {
                healed.push((k.clone(), b.clone(), change));
                continue;
            }
            out.push((
                k.0.clone(),
                Route {
                    provider: k.1.clone(),
                    model: k.2.clone(),
                    mode: b.mode.clone(),
                    reasoning_effort: None,
                },
                b.observed(now),
            ));
        }
        for (k, b, change) in healed {
            inner.breakers.remove(&k);
            let route = Route {
                provider: k.1.clone(),
                model: k.2.clone(),
                mode: b.mode.clone(),
                reasoning_effort: None,
            };
            Self::record(inner, &k.0, &route, &b, Some(change), now);
        }
        out.sort_by(|a, b| (&a.0, &a.1.provider, &a.1.model).cmp(&(&b.0, &b.1.provider, &b.1.model)));
        out
    }

    pub fn take_transitions(&self) -> Vec<HealthTransition> {
        std::mem::take(&mut self.lock().transitions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_bounds() {
        let base = Duration::from_secs(2);
        let cap = Duration::from_secs(300);
        assert_eq!(backoff(base, cap, 0, 0.0), Duration::from_secs(1));
        assert_eq!(backoff(base, cap, 0, 1.0), Duration::from_secs(2));
        assert_eq!(backoff(base, cap, 3, 1.0), Duration::from_secs(16));
        assert_eq!(backoff(base, cap, 30, 1.0), cap);
        assert_eq!(backoff(base, cap, 30, 0.0), cap / 2);
        assert_eq!(backoff(base, cap, 1, f64::NAN), Duration::from_secs(2));
    }

    #[test]
    fn default_rng_in_unit_interval() {
        let mut rng = default_rng();
        for _ in 0..10_000 {
            let u = rng();
            assert!((0.0..1.0).contains(&u));
        }
    }

    #[test]
    fn route_unavailable_permanent_code() {
        let limits = RetryLimits::default();
        let mut e = AdapterError::new(ErrorClass::RouteUnavailable, "x");
        assert_eq!(limits.for_error(&e), 3);
        e.code = Some(CODE_ROUTE_NOT_CONFIGURED.into());
        assert_eq!(limits.for_error(&e), 0);
    }

    #[test]
    fn native_exhaustion_stops_replay_and_rotation_without_hiding_overload() {
        let policy = Policy::new(ResilienceConfig::default(), ResilienceOptions::default());
        let route = Route { provider: "fixture".into(), model: "image-model".into(),
            mode: "cli".into(), reasoning_effort: None };
        let alternate = Route { model: "alternate".into(), ..route.clone() };
        let mut error = AdapterError::new(ErrorClass::Overloaded, "native overload");
        error.http_status = Some(503);
        assert!(matches!(policy.decide(&mut TurnLedger::default(), "antigravity", &route,
            &error, &[], 0.0), Recovery::Retry { .. }));
        error.code = Some("native:retry_exhausted:agy:attempts:4".into());
        assert_eq!(RetryLimits::default().for_error(&error), 0);
        assert_eq!(policy.decide(&mut TurnLedger::default(), "antigravity", &route,
            &error, &[route.clone(), alternate], 1.0),
            Recovery::GiveUp { class: ErrorClass::Overloaded, attempts: 1 });
        assert_eq!(error.http_status, Some(503));
    }
}
