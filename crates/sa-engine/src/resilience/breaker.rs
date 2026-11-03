//! Per-route circuit breaker (spec §9.2 E).
//!
//! `Healthy → Degraded` after one route-health failure inside the window,
//! `→ Open` once `threshold` failures fall inside `window`, `Open → HalfOpen`
//! when `open` has elapsed and the next admission claims the single probe.
//! A failure while half-open re-opens; any success closes to `Healthy`.

use super::{Health, ResilienceConfig};
use crate::adapter::ErrorClass;
use std::collections::VecDeque;

/// How long a half-open route asks other jobs to wait while its probe runs.
pub const HALF_OPEN_RECHECK_SECS: f64 = 5.0;

#[derive(Clone, Debug)]
pub(crate) struct Breaker {
    pub state: Health,
    pub mode: String,
    failures: VecDeque<f64>,
    pub open_until: f64,
    probe_since: Option<f64>,
    pub last_class: Option<ErrorClass>,
}

/// `(from, to)` when the state changed.
pub(crate) type Change = Option<(Health, Health)>;

impl Breaker {
    pub fn new(mode: &str) -> Self {
        Self {
            state: Health::Healthy,
            mode: mode.to_owned(),
            failures: VecDeque::new(),
            open_until: 0.0,
            probe_since: None,
            last_class: None,
        }
    }

    fn prune(&mut self, now: f64, cfg: &ResilienceConfig) {
        let window = cfg.breaker_window.as_secs_f64();
        while self.failures.front().is_some_and(|t| now - *t > window) {
            self.failures.pop_front();
        }
    }

    fn set(&mut self, to: Health) -> Change {
        let from = self.state;
        self.state = to;
        (from != to).then_some((from, to))
    }

    /// Lazy time-based transitions that do not claim anything: a degraded
    /// route whose failures aged out of the window is healthy again.
    pub fn refresh(&mut self, now: f64, cfg: &ResilienceConfig) -> Change {
        self.prune(now, cfg);
        if self.state == Health::Degraded && self.failures.is_empty() {
            return self.set(Health::Healthy);
        }
        None
    }

    pub fn on_failure(&mut self, now: f64, cfg: &ResilienceConfig, class: ErrorClass) -> Change {
        self.prune(now, cfg);
        self.failures.push_back(now);
        self.last_class = Some(class);
        let open = cfg.breaker_open.as_secs_f64();
        match self.state {
            Health::HalfOpen => {
                self.probe_since = None;
                self.open_until = now + open;
                self.set(Health::Open)
            }
            // Late failures of jobs admitted before the breaker opened do not
            // extend the open period.
            Health::Open => None,
            _ if self.failures.len() as u32 >= cfg.breaker_threshold.max(1) => {
                self.open_until = now + open;
                self.set(Health::Open)
            }
            _ => self.set(Health::Degraded),
        }
    }

    pub fn on_success(&mut self) -> Change {
        self.failures.clear();
        self.probe_since = None;
        self.last_class = None;
        self.set(Health::Healthy)
    }

    /// Non-mutating admission check: `Some(until)` while jobs must wait.
    pub fn blocked_until(&self, now: f64, cfg: &ResilienceConfig) -> Option<f64> {
        match self.state {
            Health::Open if now < self.open_until => Some(self.open_until),
            Health::HalfOpen => {
                let since = self.probe_since?;
                let stale = since + cfg.breaker_open.as_secs_f64();
                (now < stale).then(|| (now + HALF_OPEN_RECHECK_SECS).min(stale))
            }
            _ => None,
        }
    }

    /// Mutating admission: an expired open breaker (or a stale probe) admits
    /// exactly one probe and becomes half-open.
    pub fn admit(&mut self, now: f64, cfg: &ResilienceConfig) -> (Option<f64>, Change) {
        if let Some(until) = self.blocked_until(now, cfg) {
            return (Some(until), None);
        }
        match self.state {
            Health::Open => {
                self.probe_since = Some(now);
                (None, self.set(Health::HalfOpen))
            }
            Health::HalfOpen => {
                self.probe_since = Some(now);
                (None, None)
            }
            _ => (None, None),
        }
    }

    /// State as seen at `now` without claiming a probe.
    pub fn observed(&self, now: f64) -> Health {
        match self.state {
            Health::Open if now >= self.open_until => Health::HalfOpen,
            s => s,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg() -> ResilienceConfig {
        ResilienceConfig {
            breaker_threshold: 3,
            breaker_window: Duration::from_secs(600),
            breaker_open: Duration::from_secs(120),
            ..Default::default()
        }
    }

    #[test]
    fn lifecycle() {
        let c = cfg();
        let mut b = Breaker::new("cli");
        assert_eq!(
            b.on_failure(0.0, &c, ErrorClass::Overloaded),
            Some((Health::Healthy, Health::Degraded))
        );
        assert_eq!(b.on_failure(1.0, &c, ErrorClass::Overloaded), None);
        assert_eq!(
            b.on_failure(2.0, &c, ErrorClass::Overloaded),
            Some((Health::Degraded, Health::Open))
        );
        assert_eq!(b.blocked_until(10.0, &c), Some(122.0));
        assert_eq!(b.admit(50.0, &c).0, Some(122.0));
        assert_eq!(b.observed(130.0), Health::HalfOpen);
        // First admission after the open period is the probe.
        assert_eq!(b.admit(130.0, &c), (None, Some((Health::Open, Health::HalfOpen))));
        // Others wait while the probe runs.
        assert_eq!(b.admit(131.0, &c).0, Some(136.0));
        // Probe failure re-opens.
        assert_eq!(
            b.on_failure(140.0, &c, ErrorClass::Network),
            Some((Health::HalfOpen, Health::Open))
        );
        assert_eq!(b.blocked_until(141.0, &c), Some(260.0));
        assert_eq!(b.admit(260.0, &c).0, None);
        assert_eq!(b.on_success(), Some((Health::HalfOpen, Health::Healthy)));
    }

    #[test]
    fn window_ages_out() {
        let c = cfg();
        let mut b = Breaker::new("cli");
        b.on_failure(0.0, &c, ErrorClass::Network);
        b.on_failure(1.0, &c, ErrorClass::Network);
        // Third failure outside the window of the first two does not open.
        assert_eq!(b.on_failure(700.0, &c, ErrorClass::Network), None);
        assert_eq!(b.state, Health::Degraded);
        assert_eq!(b.refresh(1400.0, &c), Some((Health::Degraded, Health::Healthy)));
    }

    #[test]
    fn stale_probe_is_replaced() {
        let c = cfg();
        let mut b = Breaker::new("cli");
        for t in 0..3 {
            b.on_failure(t as f64, &c, ErrorClass::Stalled);
        }
        assert_eq!(b.admit(200.0, &c).0, None);
        // Probe never reported back; after another open period a new probe.
        assert!(b.admit(250.0, &c).0.is_some());
        assert_eq!(b.admit(320.0, &c).0, None);
    }
}
