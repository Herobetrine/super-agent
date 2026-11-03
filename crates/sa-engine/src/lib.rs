//! Super Agent scheduler (port of `runtime/engine*.cjs`).
//!
//! Behavior reference: `docs/dev/engine-spec.md`.
//! - `engine`: admission loop, turn execution, commit and failure handling.
//! - `dispatch`: streamed and final `SA_*` commands.
//! - `adapter`: contract between the engine and harness adapters.
//! - `resilience`: stall watchdog, error classification, retry policy,
//!   circuit breaker and provider rotation.

pub mod adapter;
pub mod autonomy;
pub mod context;
pub mod db;
pub mod dispatch;
pub mod engine;
pub mod guides;
pub mod job;
pub mod protocol;
pub mod resilience;
pub mod runner;
pub mod services;

pub use engine::{CreateParams, Engine, EngineOptions};
