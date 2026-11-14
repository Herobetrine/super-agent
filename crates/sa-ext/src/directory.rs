//! Real-directory boundaries for extension availability, independent of tool arguments.

use anyhow::{Context, Result, ensure};
use sa_engine::db::{Q, str_of};
use sa_engine::services::strip_verbatim;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub(crate) fn real_directory(path: &Path) -> Result<PathBuf> {
    let real = strip_verbatim(path.canonicalize().context("扩展工作目录不可用")?);
    ensure!(real.is_dir(), "扩展工作目录必须是现存目录");
    Ok(real)
}

pub(crate) fn contains(root: &Path, real: &Path) -> bool {
    // canonicalize resolves equivalent Windows spellings to stored components.
    // Folding case here would merge distinct NTFS case-sensitive directories.
    real.starts_with(root)
}

pub(crate) fn available(row: &Map<String, Value>, cwd: Option<&Path>) -> bool {
    let root = str_of(row, "scope_root");
    root.is_empty() || cwd.is_some_and(|cwd| contains(Path::new(root), cwd))
}

pub(crate) fn ticket_directory(q: &Q, ticket: i64) -> Result<PathBuf> {
    let row = q.one("SELECT cwd FROM tickets WHERE id=?", &[&ticket])?;
    let row = row.context("工单不存在")?;
    real_directory(Path::new(str_of(&row, "cwd")))
}
