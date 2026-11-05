//! Contract between the scheduler and harness adapters (spec §7).
//!
//! Adapters run one native turn. They stream every native frame through the
//! [`EventSink`]; the engine journals the frames and the resilience layer uses
//! them as heartbeats. Adapters never write the database themselves.

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const AGENTS: [&str; 4] = ["codex", "claude", "deepseek", "antigravity"];

/// `{provider, model, mode}` selection (spec §6.4).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Route {
    pub provider: String,
    pub model: String,
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// Inputs of one native turn (spec §7.1 run options).
#[derive(Clone, Debug)]
pub struct RunRequest {
    pub ticket: i64,
    pub job: i64,
    pub turn: i64,
    pub agent: String,
    pub prompt: String,
    /// Validated immutable PNG/JPEG snapshot bytes for this turn.
    pub images: Vec<crate::images::ImageInput>,
    pub cwd: PathBuf,
    pub route: Route,
    /// readonly | ask | partial | full | auto | legacy
    pub profile: String,
    pub writable: bool,
    /// Native session to resume, if any.
    pub session_id: Option<String>,
}

/// One item emitted by an adapter during a turn.
#[derive(Clone, Debug)]
pub enum AdapterEvent {
    /// A journaled event: `native`, `delta`, `message.boundary`,
    /// `permission.effective`, `route.selected`, `note.deferred`, ...
    Journal { kind: String, data: Value },
    /// Native session id known early (persist before the turn completes so a
    /// retry can resume it; spec §9.2 D.1).
    Session(String),
    /// A native tool started (`id`); the watchdog switches to tool timeouts.
    ToolStarted(String),
    /// A native tool finished.
    ToolFinished(String),
    /// The harness announced its own retry/backoff; extend the idle timer.
    NativeRetry { after: Option<Duration> },
    /// Waiting on an approval; timers pause until the next frame.
    AwaitingApproval,
}

/// Channel the adapter writes events to. A closed channel means the engine
/// discarded the turn (cancelled or superseded); adapters should stop.
pub type EventSink = mpsc::UnboundedSender<AdapterEvent>;

/// Engine services available to an adapter during a turn.
pub trait TurnHost: Send + Sync {
    /// Ask for approval of an action; resolves false when denied/unattended.
    fn approve(&self, action: Value) -> BoxFuture<'static, bool>;
    /// Unconsumed mailbox notes for this job: `{seq, sender, body}`.
    fn pending_notes(&self) -> Vec<Value>;
    /// Mark notes consumed during this turn (logs `note.accepted`).
    fn accept_notes(&self, seqs: &[i64]);
}

/// Successful terminal of a native turn.
#[derive(Clone, Debug)]
pub struct RunOutcome {
    pub text: String,
    pub session_id: Option<String>,
}

/// Failure classes (spec §9.2 C).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Network,
    RateLimited,
    Overloaded,
    Stalled,
    ProcessCrashed,
    /// Provider/model not reachable or not configured right now.
    RouteUnavailable,
    Auth,
    Billing,
    Permission,
    Protocol,
    InputTooLarge,
    SessionMismatch,
    Cancelled,
    Other,
}

/// Structured adapter failure. `message` is operator-facing Chinese text and
/// must never contain credentials or raw stderr.
#[derive(Clone, Debug)]
pub struct AdapterError {
    pub class: ErrorClass,
    pub message: String,
    pub http_status: Option<u16>,
    pub retry_after: Option<Duration>,
    /// Session that was active when the failure happened (for resume).
    pub session_id: Option<String>,
    /// Short redacted code for journals, e.g. `exit:1`, `http:503`.
    pub code: Option<String>,
}

impl AdapterError {
    pub fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
            http_status: None,
            retry_after: None,
            session_id: None,
            code: None,
        }
    }
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for AdapterError {}

/// Catalog in the spec §7.2 JSON shape.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub value: Value,
}

/// A harness adapter. Implementations must be cancellation-aware: when
/// `cancel` fires they interrupt natively, then kill the process tree.
pub trait Adapter: Send + Sync {
    fn agent(&self) -> &'static str;
    fn catalog(&self) -> BoxFuture<'static, anyhow::Result<Catalog>>;
    fn run(
        &self,
        request: RunRequest,
        host: Arc<dyn TurnHost>,
        events: EventSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<RunOutcome, AdapterError>>;
}
