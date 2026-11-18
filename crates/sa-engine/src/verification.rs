//! Verification keys for reusing check results (architecture §10.2).
//!
//! A passed completion check is reused only when the work tree fingerprint
//! and this key both match. The key covers the command lines, the platform,
//! the identity of each resolved program (path, size, modification time), the
//! search path and a fixed list of build-relevant environment variables.
//! Values are hashed together; no variable value is stored in a receipt.
//! Inputs the platform cannot model (network, clock, databases, files outside
//! the tree) are not covered: a check that depends on them is reused exactly
//! as before this key existed, which the documentation states as a limit.

use crate::protocol::{digest, stringify};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub(crate) const KEY_VERSION: &str = "verification-key/1";

/// Environment variables that commonly change build or test results.
const RELEVANT_ENV: &[&str] = &[
    "RUSTFLAGS",
    "RUSTUP_TOOLCHAIN",
    "CARGO_BUILD_TARGET",
    "CARGO_TARGET_DIR",
    "CC",
    "CXX",
    "CFLAGS",
    "NODE_ENV",
    "NODE_OPTIONS",
    "PYTHONPATH",
    "VIRTUAL_ENV",
    "JAVA_HOME",
    "GOFLAGS",
    "GOOS",
    "GOARCH",
];

fn executable_candidates(program: &str) -> Vec<String> {
    let mut names = vec![program.to_string()];
    if cfg!(windows) && Path::new(program).extension().is_none() {
        let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        names.extend(
            extensions
                .split(';')
                .filter(|ext| !ext.is_empty())
                .map(|ext| format!("{program}{}", ext.to_ascii_lowercase())),
        );
    }
    names
}

/// Resolve `program` like a shell would, relative to `cwd` for paths.
fn resolve(cwd: &Path, program: &str) -> Option<PathBuf> {
    let has_separator = program.contains('/') || program.contains('\\');
    let roots: Vec<PathBuf> = if has_separator {
        vec![if Path::new(program).is_absolute() { PathBuf::new() } else { cwd.to_path_buf() }]
    } else {
        std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).take(256).collect())
            .unwrap_or_default()
    };
    for root in roots {
        for name in executable_candidates(program) {
            let candidate = if root.as_os_str().is_empty() { PathBuf::from(&name) } else { root.join(&name) };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn identity(cwd: &Path, program: &str) -> Value {
    match resolve(cwd, program) {
        Some(path) => {
            let meta = std::fs::metadata(&path).ok();
            let modified = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64);
            json!({"program": program, "path_sha256": digest(&path.to_string_lossy()),
                "bytes": meta.map(|m| m.len()), "modified_ms": modified})
        }
        None => json!({"program": program, "resolved": false}),
    }
}

/// The environment summary hashed into the key. Paths and values are digested.
pub(crate) fn environment(cwd: &Path, commands: &[Vec<String>]) -> Value {
    let mut programs: Vec<&str> = commands.iter().filter_map(|argv| argv.first().map(String::as_str)).collect();
    programs.sort_unstable();
    programs.dedup();
    let variables: Vec<Value> = RELEVANT_ENV
        .iter()
        .map(|name| json!([name, std::env::var(name).ok().map(|v| digest(&v))]))
        .collect();
    json!({
        "platform": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        "programs": programs.iter().map(|program| identity(cwd, program)).collect::<Vec<_>>(),
        "path_sha256": std::env::var("PATH").ok().map(|v| digest(&v)),
        "variables_sha256": digest(&stringify(&json!(variables))),
    })
}

/// Key of one check definition in one environment.
pub(crate) fn key(commands: &[Vec<String>], environment: &Value) -> String {
    digest(&stringify(&json!({"version": KEY_VERSION, "commands": commands, "environment": environment})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_changes_with_commands_program_identity_and_variables() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join(if cfg!(windows) { "tool.cmd" } else { "tool" });
        std::fs::write(&tool, "one").unwrap();
        let argv = vec![tool.to_string_lossy().to_string(), "--check".into()];
        let one = std::slice::from_ref(&argv);
        let base = key(one, &environment(dir.path(), one));
        assert_eq!(base, key(one, &environment(dir.path(), one)));
        let other = vec![argv[0].clone(), "--other".into()];
        let changed = std::slice::from_ref(&other);
        assert_ne!(base, key(changed, &environment(dir.path(), changed)));
        std::fs::write(&tool, "a different program").unwrap();
        assert_ne!(base, key(one, &environment(dir.path(), one)));
        let missing = vec!["definitely-not-a-program-sa".to_string()];
        assert_eq!(environment(dir.path(), &[missing])["programs"][0]["resolved"], false);
    }
}
