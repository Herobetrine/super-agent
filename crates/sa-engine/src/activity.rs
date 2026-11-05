//! Live per-job activity for the UI (spec §9.2 G).
//!
//! Each admitted turn keeps an in-memory [`Tracker`] that the engine updates
//! from the adapter events it forwards (every event is a heartbeat, like the
//! stall watchdog). [`crate::Engine::activity`] snapshots the running turns
//! of a ticket plus the queued jobs that wait for a scheduled retry, and
//! [`JobActivity::badge`] maps a snapshot to the status badge.

use crate::adapter::AdapterEvent;
use crate::resilience::{Health, ResilienceConfig};
use serde::Serialize;
use std::collections::BTreeSet;
use std::time::Duration;

/// Frames older than this make a running job "等待上游" (spec §9.2 G).
pub const UPSTREAM_WAIT_MS: u64 = 30_000;

/// What a job is doing right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityPhase {
    /// Admitted: route selection, catalog probes and prompt assembly.
    Setup,
    /// The native turn started; no frame yet (watchdog `first_frame`).
    Spawning,
    /// Frames arriving, no tool open (watchdog `model_idle`).
    Streaming,
    /// At least one native tool is open (watchdog `tool_idle`).
    ToolRunning,
    /// Waiting for an operator approval; idle timers pause.
    AwaitingApproval,
    /// The native turn returned; committing the reply.
    Closing,
    /// Queued until `retry.next_at` after a failed attempt.
    RetryWait,
}

/// Visual weight of a badge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BadgeTone {
    /// Normal progress.
    Active,
    /// Waiting on something external that is expected to answer.
    Waiting,
    /// A tool is running.
    Busy,
    /// Degraded route or an interrupted turn that will retry.
    Warning,
    /// Suspected stall or an open circuit breaker.
    Danger,
}

/// A scheduled retry of a queued job (from the journal).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RetryInfo {
    /// Failed attempts of this logical turn so far.
    pub attempt: u32,
    /// Retry budget of the class; `None` when unbounded (continuous root,
    /// rotation, waiting for an open route).
    pub max: Option<u32>,
    /// Error class (`overloaded`, `stalled`, …) or `circuit_open`.
    pub class: String,
    /// Epoch seconds when the job becomes admissible again.
    pub next_at: f64,
}

/// Snapshot of one running job, or of a queued job with a scheduled retry.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct JobActivity {
    pub job: i64,
    pub agent: String,
    pub phase: ActivityPhase,
    /// Milliseconds since the last native frame (or since the phase began
    /// when there was none). 0 for queued jobs.
    pub idle_ms: u64,
    /// Open native tool ids (at most 16).
    pub open_tools: Vec<String>,
    /// Epoch seconds of the last native frame of this turn.
    pub last_frame_at: Option<f64>,
    /// Epoch seconds of the admission of this turn; `None` when queued.
    pub started_at: Option<f64>,
    /// Watchdog limit of the current phase, when the runner has one.
    pub threshold_ms: Option<u64>,
    /// Health of the job's route (healthy when unknown).
    pub health: Health,
    pub retry: Option<RetryInfo>,
}

impl JobActivity {
    /// True when the job has been idle for more than 80 % of its phase's
    /// watchdog limit.
    pub fn suspected_stall(&self) -> bool {
        self.threshold_ms
            .is_some_and(|t| t > 0 && u128::from(self.idle_ms) * 5 > u128::from(t) * 4)
    }

    /// Status badge `(label, tone)`. Precedence: scheduled retry (or open
    /// breaker while queued), awaiting approval, suspected stall, open
    /// breaker, degraded route, tool running, waiting for upstream, running.
    /// The retry label omits `n/max`; see [`JobActivity::badge_text`].
    pub fn badge(&self) -> (&'static str, BadgeTone) {
        if self.retry.is_some() {
            return if self.health == Health::Open {
                ("执行端熔断", BadgeTone::Danger)
            } else {
                ("已中断，重试中", BadgeTone::Warning)
            };
        }
        if self.phase == ActivityPhase::AwaitingApproval {
            return ("等待审批", BadgeTone::Waiting);
        }
        if self.suspected_stall() {
            return ("疑似卡住", BadgeTone::Danger);
        }
        match self.health {
            Health::Open => return ("执行端熔断", BadgeTone::Danger),
            Health::Degraded | Health::HalfOpen => return ("执行端降级", BadgeTone::Warning),
            Health::Healthy => {}
        }
        if self.phase == ActivityPhase::ToolRunning {
            return ("工具执行中", BadgeTone::Busy);
        }
        if self.idle_ms > UPSTREAM_WAIT_MS {
            return ("等待上游", BadgeTone::Waiting);
        }
        ("运行中", BadgeTone::Active)
    }

    /// The badge label with retry details: `已中断，重试中 n/max · mm:ss`
    /// (`n` without `/max` when unbounded; the countdown to `next_at` from
    /// `now_secs`, omitted once due).
    pub fn badge_text(&self, now_secs: f64) -> String {
        let (label, _) = self.badge();
        let Some(r) = &self.retry else { return label.to_string() };
        let mut out = label.to_string();
        if label == "已中断，重试中" {
            out.push(' ');
            out.push_str(&r.attempt.to_string());
            if let Some(max) = r.max {
                out.push('/');
                out.push_str(&max.to_string());
            }
        }
        let left = (r.next_at - now_secs).ceil();
        if left >= 1.0 {
            let left = left as u64;
            out.push_str(&format!(" · {:02}:{:02}", left / 60, left % 60));
        }
        out
    }
}

/// In-memory heartbeat state of one admitted turn. Times are epoch seconds.
#[derive(Debug)]
pub(crate) struct Tracker {
    phase: ActivityPhase,
    started_at: f64,
    phase_at: f64,
    last_frame_at: Option<f64>,
    open_tools: BTreeSet<String>,
    awaiting: bool,
    approvals: usize,
    extension: Duration,
}

impl Tracker {
    pub(crate) fn new(now: f64) -> Self {
        Self {
            phase: ActivityPhase::Setup,
            started_at: now,
            phase_at: now,
            last_frame_at: None,
            open_tools: BTreeSet::new(),
            awaiting: false,
            approvals: 0,
            extension: Duration::ZERO,
        }
    }

    pub(crate) fn set_phase(&mut self, phase: ActivityPhase, now: f64) {
        self.phase = phase;
        self.phase_at = now;
    }

    /// Mirror of the watchdog's `Watch::on_event`.
    pub(crate) fn on_event(&mut self, ev: &AdapterEvent, now: f64) {
        self.last_frame_at = Some(now);
        if matches!(self.phase, ActivityPhase::Spawning | ActivityPhase::Setup) {
            self.set_phase(ActivityPhase::Streaming, now);
        }
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
            AdapterEvent::NativeRetry { after } => self.extension = after.unwrap_or(Duration::ZERO),
            AdapterEvent::AwaitingApproval => self.awaiting = true,
            AdapterEvent::Session(_) | AdapterEvent::Journal { .. } => {}
        }
    }

    pub(crate) fn approval(&mut self, started: bool, now: f64) {
        if started {
            self.approvals += 1;
        } else {
            self.approvals = self.approvals.saturating_sub(1);
            if self.approvals == 0 {
                self.awaiting = false;
                // The decision is activity; restart the idle clock.
                self.last_frame_at = Some(now);
            }
        }
    }

    /// `(phase, idle_ms, open_tools, last_frame_at, started_at, threshold_ms)`.
    pub(crate) fn snapshot(&self, now: f64, limits: Option<&ResilienceConfig>) -> Snapshot {
        let phase = if self.approvals > 0 || self.awaiting {
            ActivityPhase::AwaitingApproval
        } else if self.phase == ActivityPhase::Streaming && !self.open_tools.is_empty() {
            ActivityPhase::ToolRunning
        } else {
            self.phase
        };
        let since = match phase {
            ActivityPhase::Streaming | ActivityPhase::ToolRunning | ActivityPhase::AwaitingApproval => {
                self.last_frame_at.unwrap_or(self.phase_at)
            }
            _ => self.phase_at,
        };
        let idle_ms = ((now - since).max(0.0) * 1000.0) as u64;
        let ms = |d: Duration| d.as_millis().min(u128::from(u64::MAX)) as u64;
        let threshold_ms = limits.and_then(|c| match phase {
            ActivityPhase::Spawning => Some(ms(c.first_frame)),
            ActivityPhase::Streaming => Some(ms(c.model_idle + self.extension.min(c.retry_cap))),
            ActivityPhase::ToolRunning => Some(ms(c.tool_idle)),
            _ => None,
        });
        Snapshot {
            phase,
            idle_ms,
            open_tools: self.open_tools.iter().take(16).cloned().collect(),
            last_frame_at: self.last_frame_at,
            started_at: self.started_at,
            threshold_ms,
        }
    }
}

pub(crate) struct Snapshot {
    pub phase: ActivityPhase,
    pub idle_ms: u64,
    pub open_tools: Vec<String>,
    pub last_frame_at: Option<f64>,
    pub started_at: f64,
    pub threshold_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(phase: ActivityPhase, idle_ms: u64) -> JobActivity {
        JobActivity {
            job: 1,
            agent: "codex".into(),
            phase,
            idle_ms,
            open_tools: vec![],
            last_frame_at: None,
            started_at: Some(0.0),
            threshold_ms: Some(360_000),
            health: Health::Healthy,
            retry: None,
        }
    }

    #[test]
    fn badges_follow_the_documented_precedence() {
        assert_eq!(running(ActivityPhase::Streaming, 1_000).badge(), ("运行中", BadgeTone::Active));
        assert_eq!(running(ActivityPhase::Streaming, 31_000).badge(), ("等待上游", BadgeTone::Waiting));
        assert_eq!(running(ActivityPhase::ToolRunning, 31_000).badge(), ("工具执行中", BadgeTone::Busy));
        // 80 % of 360 s is 288 s.
        assert_eq!(running(ActivityPhase::Streaming, 288_000).badge().0, "等待上游");
        assert_eq!(running(ActivityPhase::Streaming, 288_001).badge(), ("疑似卡住", BadgeTone::Danger));
        assert_eq!(running(ActivityPhase::AwaitingApproval, 900_000).badge().0, "等待审批");
        let mut a = running(ActivityPhase::Streaming, 0);
        a.health = Health::Degraded;
        assert_eq!(a.badge(), ("执行端降级", BadgeTone::Warning));
        a.health = Health::Open;
        assert_eq!(a.badge(), ("执行端熔断", BadgeTone::Danger));
        let mut q = running(ActivityPhase::RetryWait, 0);
        q.threshold_ms = None;
        q.retry = Some(RetryInfo {
            attempt: 2,
            max: Some(5),
            class: "overloaded".into(),
            next_at: 100.0,
        });
        assert_eq!(q.badge(), ("已中断，重试中", BadgeTone::Warning));
        assert_eq!(q.badge_text(25.0), "已中断，重试中 2/5 · 01:15");
        assert_eq!(q.badge_text(100.0), "已中断，重试中 2/5");
        q.retry.as_mut().unwrap().max = None;
        assert_eq!(q.badge_text(99.5), "已中断，重试中 2 · 00:01");
        q.health = Health::Open;
        assert_eq!(q.badge_text(40.0), "执行端熔断 · 01:00");
    }

    #[test]
    fn tracker_follows_the_watchdog_phases() {
        let cfg = ResilienceConfig::default();
        let mut t = Tracker::new(100.0);
        assert_eq!(t.snapshot(101.0, Some(&cfg)).phase, ActivityPhase::Setup);
        t.set_phase(ActivityPhase::Spawning, 101.0);
        let s = t.snapshot(103.0, Some(&cfg));
        assert_eq!((s.phase, s.idle_ms, s.threshold_ms), (ActivityPhase::Spawning, 2_000, Some(60_000)));
        t.on_event(&AdapterEvent::ToolStarted("call-1".into()), 104.0);
        let s = t.snapshot(110.0, Some(&cfg));
        assert_eq!((s.phase, s.idle_ms), (ActivityPhase::ToolRunning, 6_000));
        assert_eq!(s.open_tools, vec!["call-1".to_string()]);
        assert_eq!(s.threshold_ms, Some(1_800_000));
        t.on_event(&AdapterEvent::ToolFinished("call-1".into()), 111.0);
        t.on_event(&AdapterEvent::NativeRetry { after: Some(Duration::from_secs(10)) }, 112.0);
        let s = t.snapshot(112.0, Some(&cfg));
        assert_eq!((s.phase, s.threshold_ms), (ActivityPhase::Streaming, Some(370_000)));
        t.approval(true, 113.0);
        assert_eq!(t.snapshot(500.0, Some(&cfg)).phase, ActivityPhase::AwaitingApproval);
        assert_eq!(t.snapshot(500.0, Some(&cfg)).threshold_ms, None);
        t.approval(false, 120.0);
        let s = t.snapshot(121.0, None);
        assert_eq!((s.phase, s.idle_ms, s.threshold_ms), (ActivityPhase::Streaming, 1_000, None));
        assert_eq!(s.started_at, 100.0);
    }
}
