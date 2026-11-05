//! Process environment snapshot used by discovery and child processes.
//!
//! Adapters read configuration from the user's home and environment. Tests
//! inject a synthetic [`Sys`] instead of mutating the real process
//! environment.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Sys {
    vars: Vec<(String, String)>,
    /// `os.homedir()`.
    pub home: PathBuf,
    /// `os.tmpdir()`.
    pub temp: PathBuf,
}

fn same_key(a: &str, b: &str) -> bool {
    if cfg!(windows) { a.eq_ignore_ascii_case(b) } else { a == b }
}

impl Sys {
    pub fn from_process() -> Self {
        let vars: Vec<(String, String)> = std::env::vars().collect();
        let mut sys = Self {
            vars,
            home: PathBuf::new(),
            temp: std::env::temp_dir(),
        };
        let home = if cfg!(windows) { sys.get("USERPROFILE") } else { sys.get("HOME") };
        sys.home = home.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        sys
    }

    /// A synthetic environment (tests).
    pub fn new(vars: Vec<(String, String)>, home: PathBuf, temp: PathBuf) -> Self {
        Self { vars, home, temp }
    }

    /// Environment lookup; case-insensitive on Windows like `process.env`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars
            .iter()
            .find(|(k, _)| same_key(k, key))
            .map(|(_, v)| v.as_str())
    }

    /// Non-empty value.
    pub fn var(&self, key: &str) -> Option<&str> {
        self.get(key).filter(|v| !v.is_empty())
    }

    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        self.remove(key);
        self.vars.push((key.to_string(), value.into()));
    }

    pub fn remove(&mut self, key: &str) {
        self.vars.retain(|(k, _)| !same_key(k, key));
    }

    pub fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.vars.retain(|(k, _)| keep(k));
    }

    pub fn vars(&self) -> &[(String, String)] {
        &self.vars
    }

    /// `~/.local/share/superagent` (AC:9).
    pub fn platform_dir(&self) -> PathBuf {
        self.home.join(".local/share/superagent")
    }

    pub fn appdata(&self) -> PathBuf {
        self.var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.home.join("AppData/Roaming"))
    }

    /// `which(name)` (AC:27-39): first regular file on an absolute PATH entry.
    pub fn which(&self, name: &str) -> Option<PathBuf> {
        let path = self.get("PATH")?;
        std::env::split_paths(path)
            .filter(|d| d.is_absolute())
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    }
}

/// `path.resolve(base, p)`.
pub fn resolve(base: &Path, p: &str) -> PathBuf {
    let p = Path::new(p);
    if p.is_absolute() { p.to_path_buf() } else { base.join(p) }
}
