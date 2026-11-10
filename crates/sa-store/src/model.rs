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
    pub native_activity: NativeActivity,
    #[serde(default)]
    pub usage: crate::usage::TicketUsage,
    /// Ascending by seq; at most 180.
    pub events: Vec<EventView>,
    /// Pass back as `after` to receive only newer events.
    pub cursor: i64,
    pub approvals: Vec<Approval>,
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
