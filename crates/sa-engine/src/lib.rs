//! Super Agent scheduler (port of `runtime/engine*.cjs`).
//!
//! Behavior reference: `docs/dev/engine-spec.md`.
//! - `engine`: admission loop, turn execution, commit and failure handling.
//! - `dispatch`: streamed and final `SA_*` commands.
//! - `adapter`: contract between the engine and harness adapters.
//! - `knowledge`: shared per-project knowledge base (`SA_MEMO`).
//! - `resilience`: stall watchdog, error classification, retry policy,
//!   circuit breaker and provider rotation.
//! - `activity`: live per-job activity and status badges for the UI.

pub mod activity;
pub mod advice;
pub mod checkpoints;
pub mod check_suggestions;
mod checks;
pub mod adapter;
pub mod autonomy;
pub mod context;
pub mod context_selection;
pub mod db;
pub mod delivery;
pub mod dispatch;
pub mod engine;
pub mod execution_policy;
pub mod guides;
pub mod images;
pub mod integration_queue;
pub mod job;
pub mod knowledge;
pub mod owner;
pub mod pause;
pub mod protocol;
pub mod project_git;
pub mod resilience;
pub mod runner;
pub mod services;
pub mod scm;
pub mod workspaces;
mod verification;
pub mod stuck;
pub mod trials;

pub use activity::{ActivityPhase, BadgeTone, JobActivity, RetryInfo};
pub use engine::{CreateParams, Engine, EngineOptions};
pub use pause::PauseStatus;
pub use sa_store::knowledge::KnowledgeEntry;
