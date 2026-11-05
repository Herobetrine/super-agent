//! Harness adapters (port of `runtime/adapters/*`, spec §7).
//!
//! Each adapter implements [`sa_engine::adapter::Adapter`]: it discovers its
//! catalog, runs one native turn, streams frames to the engine and returns a
//! classified [`sa_engine::adapter::AdapterError`] on failure.

use sa_engine::adapter::Adapter;
use std::sync::Arc;

pub use custom::{Registry, probe_models, registry};
pub use revision::{catalog_revision, catalog_revision_with};

/// All four harness adapters bound to one environment snapshot, in
/// `AGENTS` order; pass to [`sa_engine::Engine::open`].
pub fn all(sys: sys::Sys) -> Vec<Arc<dyn Adapter>> {
    all_with(sys, registry(vec![]))
}

/// [`all`] with operator-managed custom providers. The registry is read on
/// every `catalog()`/`run()`, so edits apply without rebuilding adapters.
/// Antigravity has no custom endpoint support and ignores it.
pub fn all_with(sys: sys::Sys, registry: Registry) -> Vec<Arc<dyn Adapter>> {
    vec![
        Arc::new(codex::Codex::with_registry(sys.clone(), registry.clone())),
        Arc::new(claude::Claude::with_registry(sys.clone(), registry.clone())),
        Arc::new(deepseek::DeepSeek::with_registry(sys.clone(), registry)),
        Arc::new(antigravity::Antigravity::new(sys)),
    ]
}

pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod codex_config;
pub mod common;
pub mod catalog;
mod images;
pub mod custom;
pub mod deepseek;
pub mod proc;
pub mod revision;
pub mod sys;
