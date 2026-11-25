//! Compatibility evidence used by the catalog. Unknown versions stay usable.
use crate::{cache_policy, deepseek, proc::capture, sys::Sys};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compatibility {
    Verified,
    Unverified,
    Broken,
}

const BROKEN: &[(&str, &str)] = &[];

pub fn classify(agent: &str, version: &str) -> Compatibility {
    if BROKEN.contains(&(agent, version)) {
        Compatibility::Broken
    } else if (agent == "claude" && cache_policy::supported(version))
        || (agent == "deepseek" && deepseek::SUPPORTED_VERSIONS.contains(&version))
    {
        Compatibility::Verified
    } else {
        Compatibility::Unverified
    }
}

pub(crate) fn info(agent: &str, version: Option<String>) -> Value {
    json!({"compatibility":classify(agent, version.as_deref().unwrap_or("")), "version":version})
}

pub(crate) fn detect(
    agent: &'static str,
    sys: Sys,
    exe: Option<PathBuf>,
) -> BoxFuture<'static, Value> {
    Box::pin(async move {
        let mut version = None;
        if let Some(exe) = exe
            && let Ok(output) = capture(
                &exe,
                &["--version".into()],
                &sys.home,
                &sys,
                "",
                Duration::from_secs(8),
                &CancellationToken::new(),
            )
            .await
        {
            version = output
                .split_whitespace()
                .map(|s| s.trim_start_matches('v'))
                .find(|s| {
                    s.len() <= 80
                        && s.starts_with(|c: char| c.is_ascii_digit())
                        && s.contains('.')
                        && s.bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b".-+".contains(&c))
                })
                .map(str::to_owned);
        }
        info(agent, version)
    })
}

/// One locally configured model; effort capability must come from discovery.
pub(crate) fn local(route: Value, available: bool) -> Value {
    json!({"providers":[{"id": route["provider"], "models":[{"id":route["model"]}]}],
        "modes":[{"id":"cli","available":available}], "default":route})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn version_evidence() {
        assert_eq!(classify("claude", "2.1.241"), Compatibility::Unverified);
        assert_eq!(classify("claude", "2.1.242"), Compatibility::Verified);
        assert_eq!(
            classify("deepseek", deepseek::VERSION),
            Compatibility::Verified
        );
        assert_eq!(classify("codex", "2.1.242"), Compatibility::Unverified);
        assert_eq!(classify("antigravity", ""), Compatibility::Unverified);
    }
}
