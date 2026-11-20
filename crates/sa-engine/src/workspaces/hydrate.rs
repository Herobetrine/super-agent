//! Ignored-resource hydration for isolated worktrees and merge candidates.
//!
//! `git worktree add` only materializes tracked files. Projects that build
//! from ignored generated resources (extracted assets, vendored tools) name
//! the paths to copy from the source checkout; only files Git ignores under
//! those paths are copied, so tracked content always comes from Git. Changes
//! a task makes to hydrated files stay in its worktree and are never merged.
use super::{command, git_argv};
use crate::db::{Q, now, str_of};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::fs;
use std::path::{Component, Path};
use std::time::Duration;

pub const SCHEMA: &str =
    "CREATE TABLE IF NOT EXISTS workspace_hydration(project TEXT PRIMARY KEY,paths TEXT NOT NULL,updated REAL NOT NULL);";
/// At most this many configured paths per project.
pub const MAX_PATHS: usize = 32;
/// Refuse a hydration larger than this many files ...
pub const MAX_FILES: usize = 200_000;
/// ... or bytes, instead of copying part of it.
pub const MAX_BYTES: u64 = 8 << 30;

/// Normalized, deduplicated project-relative paths. Rejects absolute paths,
/// `..`, and anything inside `.git`.
pub fn validate(paths: &[String]) -> Result<Vec<String>> {
    ensure!(paths.len() <= MAX_PATHS, "补齐路径最多 {MAX_PATHS} 条");
    let mut out: Vec<String> = vec![];
    for raw in paths {
        let text = raw.trim().replace('\\', "/");
        let text = text.trim_start_matches("./").trim_end_matches('/');
        ensure!(!text.is_empty(), "补齐路径不能为空");
        ensure!(
            Path::new(text).components().all(|c| matches!(c, Component::Normal(_))),
            "补齐路径必须是项目内的相对路径：{raw}"
        );
        ensure!(!text.split('/').any(|s| s.eq_ignore_ascii_case(".git")), "补齐路径不能指向 .git：{raw}");
        if !out.iter().any(|p| p == text) {
            out.push(text.to_string());
        }
    }
    Ok(out)
}

pub fn paths(q: &Q, project: &str) -> Result<Vec<String>> {
    Ok(match q.one("SELECT paths FROM workspace_hydration WHERE project=?", &[&project])? {
        Some(row) => serde_json::from_str(str_of(&row, "paths")).unwrap_or_default(),
        None => vec![],
    })
}

/// Replace the project's paths; an empty list removes the configuration.
pub fn set(q: &Q, project: &str, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        q.run("DELETE FROM workspace_hydration WHERE project=?", &[&project])?;
    } else {
        q.run(
            "INSERT OR REPLACE INTO workspace_hydration(project,paths,updated) VALUES(?,?,?)",
            &[&project, &serde_json::to_string(paths)?, &now()],
        )?;
    }
    Ok(())
}

/// Copy the ignored files under `paths` from `source` into `dest`. Files that
/// already match by size and are not older are kept, so a repeated call after
/// a crash completes the copy. Returns the receipt, `null` when not configured.
pub fn hydrate(source: &Path, dest: &Path, paths: &[String]) -> Result<Value> {
    if paths.is_empty() {
        return Ok(Value::Null);
    }
    let mut args = vec!["ls-files", "-z", "--others", "--ignored", "--exclude-standard", "--"];
    args.extend(paths.iter().map(String::as_str));
    let (ok, out) = command(source, &git_argv(&args), Duration::from_secs(120))?;
    ensure!(ok, "列出待补齐的忽略文件失败");
    let mut files = vec![];
    let mut bytes = 0u64;
    for rel in out.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        let rel = String::from_utf8(rel.to_vec())?;
        let meta = fs::symlink_metadata(source.join(&rel))?;
        if !meta.is_file() {
            continue;
        }
        bytes += meta.len();
        files.push((rel, meta));
        ensure!(
            files.len() <= MAX_FILES && bytes <= MAX_BYTES,
            "补齐资源超过上限（{MAX_FILES} 个文件或 {} GiB），请缩小补齐路径",
            MAX_BYTES >> 30
        );
    }
    let mut copied = 0usize;
    for (rel, meta) in &files {
        let to = dest.join(rel);
        if let Ok(existing) = fs::metadata(&to)
            && existing.len() == meta.len()
            && existing.modified().ok() >= meta.modified().ok()
        {
            continue;
        }
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source.join(rel), &to)?;
        copied += 1;
    }
    Ok(json!({"paths": paths, "files": files.len(), "bytes": bytes, "copied": copied}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let (ok, out) = command(dir, &git_argv(args), Duration::from_secs(30)).unwrap();
        assert!(ok, "{}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn paths_must_stay_inside_the_project() {
        assert_eq!(
            validate(&["./gen/".into(), "gen".into(), r"assets\cache".into()]).unwrap(),
            ["gen", "assets/cache"]
        );
        for bad in ["", "../x", "/abs", "C:/abs", "a/../b", ".git/hooks", "x/.GIT"] {
            assert!(validate(&[bad.into()]).is_err(), "{bad}");
        }
        let many: Vec<String> = (0..=MAX_PATHS).map(|i| format!("p{i}")).collect();
        assert!(validate(&many).is_err());
    }

    #[test]
    fn copies_only_ignored_files_under_the_paths_and_resumes() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let s = source.path();
        git(s, &["init", "-q"]);
        fs::write(s.join(".gitignore"), "gen/\ntmp/\n!gen/tracked.txt\n").unwrap();
        fs::create_dir_all(s.join("gen/sub")).unwrap();
        fs::create_dir_all(s.join("tmp")).unwrap();
        fs::write(s.join("gen/a.bin"), b"aaaa").unwrap();
        fs::write(s.join("gen/sub/资源.json"), "{}").unwrap();
        fs::write(s.join("gen/tracked.txt"), "tracked").unwrap();
        fs::write(s.join("tmp/huge.bin"), b"x").unwrap();
        git(s, &["add", "-f", ".gitignore", "gen/tracked.txt"]);
        let paths = vec!["gen".to_string()];
        let receipt = hydrate(s, dest.path(), &paths).unwrap();
        assert_eq!((receipt["files"].as_u64(), receipt["copied"].as_u64(), receipt["bytes"].as_u64()), (Some(2), Some(2), Some(6)));
        assert!(dest.path().join("gen/a.bin").is_file());
        assert!(dest.path().join("gen/sub/资源.json").is_file());
        assert!(!dest.path().join("gen/tracked.txt").exists(), "tracked files come from Git");
        assert!(!dest.path().join("tmp").exists(), "unconfigured paths are not copied");
        let again = hydrate(s, dest.path(), &paths).unwrap();
        assert_eq!(again["copied"], 0);
        assert!(hydrate(s, dest.path(), &[]).unwrap().is_null());
    }
}
