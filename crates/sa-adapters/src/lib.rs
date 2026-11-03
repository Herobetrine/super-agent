//! Harness adapters (port of `runtime/adapters/*`, spec §7).
//!
//! Each adapter implements [`sa_engine::adapter::Adapter`]: it discovers its
//! catalog, runs one native turn, streams frames to the engine and returns a
//! classified [`sa_engine::adapter::AdapterError`] on failure.

use sa_engine::adapter::Adapter;
use std::sync::Arc;

/// All four harness adapters bound to one environment snapshot, in
/// `AGENTS` order; pass to [`sa_engine::Engine::open`].
pub fn all(sys: sys::Sys) -> Vec<Arc<dyn Adapter>> {
    vec![
        Arc::new(codex::Codex::new(sys.clone())),
        Arc::new(claude::Claude::new(sys.clone())),
        Arc::new(deepseek::DeepSeek::new(sys.clone())),
        Arc::new(antigravity::Antigravity::new(sys)),
    ]
}

pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod codex_config;
pub mod common;
pub mod deepseek;
pub mod proc;
pub mod sys;
