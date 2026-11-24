//! Read-model contract shared by the store and the desktop UI.
//!
//! These types mirror the values that `runtime/studio.cjs` returned over IPC
//! (field names are snake_case Rust equivalents of the JavaScript keys).
//! Event payloads stay as JSON because their shape depends on `kind`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One row of the sidebar history (`Studio.history`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TicketSummary {
    pub id: i64,
    /// First 500 characters of the goal.
    pub goal: String,
    pub cwd: String,
    pub lead: String,
    /// Unix seconds.
    pub created: f64,
    /// State of the root (lead) job.
    pub state: String,
    /// Operator-given title (`studio_ticket_meta`), shown instead of the goal.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub archived: bool,
    /// Operator attention reason: approval | question | failed | partial.
    #[serde(default)]
    pub attention: Option<String>,
}

/// Ticket header of a snapshot.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TicketDetail {
    pub id: i64,
    pub goal: String,
    pub objective: Option<String>,
    pub cwd: String,
    pub lead: String,
    pub created: f64,
    pub permission_profile: String,
    /// `{agent: {provider, model, mode}}`
    pub agent_routes: Value,
    pub allow_lead_routing: bool,
    /// State of the root (lead) job.
    pub state: String,
    /// Durable admission gate, independent of the current root job state.
    #[serde(default)]
    pub pause_status: Option<String>,
    #[serde(default)]
    pub attention: Option<String>,
}

/// A job row with bounded body/result previews (`JOB_DETAILS`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct JobView {
    pub id: i64,
    pub agent: String,
    pub parent: Option<i64>,
    pub role: String,
    pub state: String,
    pub turn: i64,
    pub writing: bool,
    pub ready_after: f64,
    pub permission_override: Option<String>,
    /// First 8000 characters.
    pub body: String,
    /// First 5000 characters.
    pub result: Option<String>,
    /// Native route `{provider, model, mode}` actually bound to the job.
    pub route: Option<Value>,
    #[serde(default)]
    pub outcome: Option<Value>,
    /// Latest platform completion-check result, separate from the agent outcome.
    #[serde(default)]
    pub checks: Option<Value>,
    #[serde(default)]
    pub needs: Vec<i64>,
}

/// A visible journal event or a derived progress row (`kind == "activity"`).
///
/// For `kind == "activity"` the payload is `{label, category, state, ...}` and
/// `agent` is the owning job's agent. For journal rows `agent` is `None`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EventView {
    pub seq: i64,
    pub kind: String,
    pub ts: f64,
    pub job: Option<i64>,
    pub turn: Option<i64>,
    pub agent: Option<String>,
    pub payload: Value,
}

/// A native (harness-internal) task such as a Claude subagent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NativeTask {
    pub id: String,
    pub job: i64,
    pub turn: i64,
    pub label: String,
    /// running | completed | failed | stopped | unknown
    pub state: String,
    pub last_tool: String,
    pub ts: f64,
    /// Short operator-facing Agent title (Claude `task_started` description
    /// that passes the privacy filter). Added for parity with the JS task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Native tool-use count reported by the harness (`toolUses` in JS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_uses: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LatestActivity {
    pub job: i64,
    pub turn: i64,
    pub label: String,
    pub ts: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct NativeActivity {
    pub tasks: Vec<NativeTask>,
    pub latest: Option<LatestActivity>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Approval {
    pub id: i64,
    pub job: i64,
    pub action: Value,
}

/// A pending operator question, with a stable agent-supplied key.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Question {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub job: i64,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub fallback: Option<String>,
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub asked: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ContinuousRun {
    pub phase: String,
    pub milestone: i64,
    pub learning: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PlanItem {
    pub id: i64,
    pub body: String,
    pub done: bool,
}

/// `Studio.snapshot` result.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub ticket: TicketDetail,
    pub jobs: Vec<JobView>,
    pub total_jobs: i64,
    pub running_peers: i64,
    /// Ready queued collaborators across the whole ticket, excluding dependency waits and the root.
    #[serde(default)]
    pub queued_peers: i64,
    pub native_activity: NativeActivity,
    /// The activity projection has not reached the end of the journal yet:
    /// `events` holds only visible events and `native_activity` is empty.
    /// Request the ticket again with `after == 0` (see `Store::snapshot_within`).
    #[serde(default)]
    pub loading: bool,
    #[serde(default)]
    pub usage: crate::usage::TicketUsage,
    /// Current subscription windows for harnesses used by this ticket.
    #[serde(default)]
    pub quota: Vec<crate::quota::Observed>,
    /// Ascending by seq: at most 180 main-feed events plus the text deltas
    /// of the lead's in-flight turn. Collaborator rounds that ended before
    /// `lead_resumed` are folded out (see [`folded_from_feed`]).
    pub events: Vec<EventView>,
    /// Seq of the lead's latest turn admission; 0 before the first one.
    #[serde(default)]
    pub lead_resumed: i64,
    /// Pass back as `after` to receive only newer events.
    pub cursor: i64,
    pub approvals: Vec<Approval>,
    #[serde(default)]
    pub questions: Vec<Question>,
    pub run: Option<ContinuousRun>,
    pub plan: Vec<PlanItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct JobsPage {
    pub jobs: Vec<JobView>,
    pub more: bool,
    pub next: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ActivityPage {
    /// Descending by seq; at most 80.
    pub events: Vec<EventView>,
    pub more: bool,
    pub next: Option<i64>,
}

/// Job states whose current round is still open.
pub const OPEN_JOB_STATES: &[&str] = &["running", "queued", "waiting", "interrupted"];

/// Event kinds that stay in the main feed even when their round is folded:
/// they ask for or record a decision by the user.
pub const DECISION_KINDS: &[&str] = &["ask", "answer", "ask.withdrawn", "approval.request", "approval.decision"];

/// Whether a journal event stays out of the main conversation feed.
///
/// Once the lead resumes (a new root turn is admitted), earlier rounds are
/// summarised by the lead's own replies, so only those replies (with the
/// user's messages, dispatches and decisions) stay in the main feed:
/// - the lead's progress rows and stage reports (`activity`) before
///   `lead_resumed` are folded;
/// - a collaborator event is folded when it precedes `lead_resumed` and its
///   round has ended: the job is no longer open, or it has since moved on to a
///   newer turn. `round` is the job's `(state, turn)`; `None` (a job outside
///   the snapshot's preview) counts as ended. Collaborator results stay in the
///   collaboration panel.
///
/// Decisions for the user always stay. Collaborator text deltas never reach
/// the main feed.
pub fn folded_from_feed(
    kind: &str,
    seq: i64,
    job: Option<i64>,
    turn: Option<i64>,
    root: Option<i64>,
    lead_resumed: i64,
    round: Option<(&str, i64)>,
) -> bool {
    let Some(job) = job else { return false };
    let Some(root) = root else { return false };
    if root == job {
        return kind == "activity" && seq < lead_resumed;
    }
    if DECISION_KINDS.contains(&kind) {
        return false;
    }
    if kind == "delta" {
        return true;
    }
    if seq >= lead_resumed {
        return false;
    }
    match round {
        None => true,
        Some((state, current)) => !OPEN_JOB_STATES.contains(&state) || turn.is_some_and(|t| t < current),
    }
}
