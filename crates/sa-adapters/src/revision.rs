//! `catalogRevision(agent)` (port of `runtime/adapters/metadata.cjs` and the
//! adapters' `watchPaths()`): a fingerprint of the public metadata of every
//! file an adapter's discovery reads. A changed revision means the catalog
//! may have changed; an unchanged one lets the caller reuse its last catalog.
//!
//! Only metadata is hashed (path, size, timestamps, inode on Unix), never file
//! contents. The value is stable within one machine but not byte-identical to
//! the Node revision (Node also hashes the Windows file index, which stable
//! Rust cannot read).

use crate::custom::{self, Registry};
use crate::sys::Sys;
use crate::{antigravity, claude, codex_config, common, deepseek};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

fn nanos(t: std::io::Result<SystemTime>) -> String {
    t.ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or_else(|| "-".into(), |d| d.as_nanos().to_string())
}

/// `stamp(file)`: `[absolute, size, mtime, ctime/created, inode]` or
/// `[absolute, error]`.
fn stamp(path: &Path) -> Value {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let shown = absolute.to_string_lossy().into_owned();
    match std::fs::metadata(&absolute) {
        Ok(m) => {
            #[cfg(unix)]
            let extra = {
                use std::os::unix::fs::MetadataExt;
                (format!("{}.{}", m.ctime(), m.ctime_nsec()), m.ino().to_string())
            };
            #[cfg(not(unix))]
            let extra = (nanos(m.created()), String::new());
            json!([shown, m.len().to_string(), nanos(m.modified()), extra.0, extra.1])
        }
        Err(e) => json!([shown, format!("{:?}", e.kind())]),
    }
}

/// `fingerprint(paths)`: SHA-256 hex over the stamps.
pub fn fingerprint(paths: &[PathBuf]) -> String {
    let stamps: Vec<Value> = paths.iter().map(|p| stamp(p)).collect();
    format!("{:x}", Sha256::digest(Value::Array(stamps).to_string().as_bytes()))
}

fn toml_pointer(file: &Path) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    let table: toml::Table = toml::from_str(&text).ok()?;
    table.get("model_catalog_json")?.as_str().map(str::to_string)
}

/// Codex `watchPaths()` (CXC:201-227).
fn codex_paths(sys: &Sys) -> Vec<PathBuf> {
    let dir = codex_config::config_home(sys);
    let base = dir.join("config.toml");
    let mut paths = vec![base.clone(), dir.join("models_cache.json"), dir.join("auth.json")];
    let mut location = toml_pointer(&base);
    if let Some(profile) = sys.get("SUPERAGENT_CODEX_PROFILE").map(str::trim).filter(|p| {
        !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }) {
        let selected = dir.join(format!("{profile}.config.toml"));
        if let Some(over) = toml_pointer(&selected) {
            location = Some(over);
        }
        paths.push(selected);
    }
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut names: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().ends_with(".config.toml")))
            .collect();
        names.sort();
        paths.extend(names);
    }
    if let Some(l) = location {
        paths.push(crate::sys::resolve(&dir, &l));
    }
    paths.extend(codex_config::executable(sys));
    paths
}

/// Claude `watchPaths()` (CL:357-369).
fn claude_paths(sys: &Sys) -> Vec<PathBuf> {
    let dir = sys.home.join(".cc-switch");
    let native = claude::native_file(sys);
    let mut paths = common::cc_watch_paths(sys);
    let credentials = native.with_file_name(".credentials.json");
    paths.extend([
        dir.join("settings.json"),
        dir.join("live-state.json"),
        native,
        credentials,
        sys.platform_dir().join("claude-route.json"),
    ]);
    paths.extend(claude::executable(sys));
    paths
}

/// Antigravity `watchPaths()` (AG:207-219).
fn antigravity_paths(sys: &Sys) -> Vec<PathBuf> {
    let dir = sys.home.join(".cc-switch");
    let rel = antigravity::SETTINGS_REL;
    let mut paths = common::cc_watch_paths(sys);
    paths.extend([
        dir.join("settings.json"),
        dir.join("live-state.json"),
        sys.home.join(rel),
        sys.platform_dir().join("agy-reader").join(rel),
        sys.platform_dir().join("agy-reader-adc").join(rel),
    ]);
    paths.extend(antigravity::executable(sys));
    paths
}

/// Files whose metadata feeds `catalog_revision(agent)`; empty for an unknown
/// agent.
pub fn watch_paths(sys: &Sys, agent: &str) -> Vec<PathBuf> {
    match agent {
        "codex" => codex_paths(sys),
        "claude" => claude_paths(sys),
        "deepseek" => deepseek::watch_paths(sys),
        "antigravity" => antigravity_paths(sys),
        _ => vec![],
    }
}

/// `catalogRevision(agent)` for the harness's own configuration. Unknown
/// agents yield an empty string.
pub fn catalog_revision(sys: &Sys, agent: &str) -> String {
    let paths = watch_paths(sys, agent);
    if paths.is_empty() {
        return String::new();
    }
    fingerprint(&paths)
}

/// [`catalog_revision`] that also changes when the enabled custom providers
/// for `agent` (or the presence of their key variables) change. Key values
/// are not hashed.
pub fn catalog_revision_with(sys: &Sys, agent: &str, registry: &Registry) -> String {
    let base = catalog_revision(sys, agent);
    if base.is_empty() {
        return base;
    }
    let customs: Vec<Value> = custom::snapshot(registry, agent)
        .iter()
        .map(|p| json!([p, p.key_env.is_empty() || sys.var(&p.key_env).is_some()]))
        .collect();
    if customs.is_empty() {
        return base;
    }
    let mixed = json!([base, customs]).to_string();
    format!("{:x}", Sha256::digest(mixed.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sa_store::providers::CustomProvider;

    #[test]
    fn revision_tracks_file_metadata_and_registry() {
        let home = tempfile::tempdir().unwrap();
        let sys = Sys::new(
            vec![("CODEX_HOME".into(), home.path().join("codex").to_string_lossy().into_owned())],
            home.path().to_path_buf(),
            std::env::temp_dir(),
        );
        assert_eq!(catalog_revision(&sys, "nope"), "");
        let before = catalog_revision(&sys, "codex");
        assert_eq!(before.len(), 64);
        assert_eq!(before, catalog_revision(&sys, "codex"));
        std::fs::create_dir_all(home.path().join("codex")).unwrap();
        std::fs::write(home.path().join("codex/config.toml"), "model = \"x\"\n").unwrap();
        let after = catalog_revision(&sys, "codex");
        assert_ne!(before, after);
        std::fs::write(home.path().join("codex/work.config.toml"), "").unwrap();
        assert_ne!(after, catalog_revision(&sys, "codex"), "new profile file is watched");
        for agent in ["claude", "deepseek", "antigravity"] {
            assert_eq!(catalog_revision(&sys, agent).len(), 64);
        }

        let reg = custom::registry(vec![]);
        let plain = catalog_revision(&sys, "claude");
        assert_eq!(catalog_revision_with(&sys, "claude", &reg), plain);
        reg.write().unwrap().push(CustomProvider {
            id: "a".into(),
            agent: "claude".into(),
            name: "n".into(),
            base_url: "https://x.example".into(),
            key_env: "K".into(),
            models: vec!["m".into()],
            enabled: true,
        });
        let with = catalog_revision_with(&sys, "claude", &reg);
        assert_ne!(with, plain);
        assert_eq!(catalog_revision_with(&sys, "codex", &reg), catalog_revision(&sys, "codex"));
        reg.write().unwrap()[0].enabled = false;
        assert_eq!(catalog_revision_with(&sys, "claude", &reg), plain);
    }
}
