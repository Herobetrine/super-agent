//! Explicit, process-local Claude Code cache TTL policy.
//!
//! https://code.claude.com/docs/en/prompt-caching documents the two TTL
//! environment variables from 2.1.242. Gate our opt-in on that version and
//! preserve the operator's upstream controls. Gateway support remains the
//! operator's responsibility, so the default never requests a different TTL.

use crate::common::{read_json, route_gone};
use crate::proc::capture;
use crate::sys::Sys;
use sa_engine::adapter::AdapterError;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const POLICY: &str = "SUPERAGENT_CLAUDE_CACHE_TTL";
const BUCKETS: [(&str, &str); 2] = [
    ("CLAUDE_CODE_PROMPT_CACHE_TTL", "promptCacheTtl"),
    (
        "CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL",
        "subagentPromptCacheTtl",
    ),
];
const CONTROLS: [&str; 7] = [
    "FORCE_PROMPT_CACHING_5M",
    "ENABLE_PROMPT_CACHING_1H",
    "DISABLE_PROMPT_CACHING",
    "DISABLE_PROMPT_CACHING_HAIKU",
    "DISABLE_PROMPT_CACHING_SONNET",
    "DISABLE_PROMPT_CACHING_OPUS",
    "DISABLE_PROMPT_CACHING_FABLE",
];

/// Adjust only this turn's environment snapshot. Existing Claude controls
/// still outrank this preference in `apply`.
pub(crate) fn select_fallback(env: &mut Sys, ttl: sa_store::efficiency::Ttl) {
    use sa_store::efficiency::Ttl;
    match ttl {
        Ttl::Inherit => {}
        Ttl::FiveMinutes => env.set(POLICY, "5m"),
        Ttl::OneHour => env.set(POLICY, "1h"),
    }
}

pub(crate) fn selected_settings(source: &Value) -> Value {
    let mut result = json!({"env": {}});
    for (_, setting) in BUCKETS {
        if let Some(value) = source.get(setting) {
            result[setting] = value.clone();
        }
    }
    for key in CONTROLS
        .iter()
        .copied()
        .chain(BUCKETS.iter().map(|(key, _)| *key))
    {
        if let Some(value) = source["env"].get(key) {
            result["env"][key] = value.clone();
        }
    }
    result
}

fn requested(env: &Sys) -> Result<Option<&str>, AdapterError> {
    match env.var(POLICY) {
        None => Ok(None),
        Some(ttl @ ("5m" | "1h")) => Ok(Some(ttl)),
        Some(_) => Err(route_gone("SUPERAGENT_CLAUDE_CACHE_TTL 仅支持 5m 或 1h")),
    }
}

fn supported(version: &str) -> bool {
    let mut parts = version
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .split('.');
    let numbers: Option<Vec<u32>> = parts.by_ref().take(3).map(|p| p.parse().ok()).collect();
    numbers.is_some_and(|v| v.len() == 3 && v.as_slice() >= [2, 1, 242].as_slice())
}

fn settings(path: &Path) -> Result<Value, AdapterError> {
    read_json(path, Value::Null)
        .map_err(|_| route_gone("Claude 缓存策略无法读取现有 settings，已停止本次调用"))
}

/// Low to high file precedence. Only cache fields are subsequently copied.
fn sources(cwd: &Path, native: &Path, provider: &Value) -> Result<Vec<Value>, AdapterError> {
    let mut result = vec![settings(native)?, provider.clone()];
    let ancestors: Vec<_> = cwd.ancestors().collect();
    for directory in ancestors.into_iter().rev() {
        result.push(settings(&directory.join(".claude/settings.json"))?);
        result.push(settings(&directory.join(".claude/settings.local.json"))?);
    }
    Ok(result)
}

fn apply(env: &mut Sys, ttl: &str, sources: &[Value]) {
    // Claude settings.env overrides inherited shell variables. Both outrank
    // top-level TTL settings, followed by the platform's opt-in fallback.
    for key in CONTROLS
        .iter()
        .copied()
        .chain(BUCKETS.iter().map(|(key, _)| *key))
    {
        if let Some(value) = sources.iter().rev().find_map(|s| s["env"][key].as_str()) {
            env.set(key, value);
        }
    }
    for (key, setting) in BUCKETS {
        if env.get(key).is_none()
            && let Some(value) = sources.iter().rev().find_map(|s| s[setting].as_str())
        {
            env.set(key, value);
        }
    }
    // Force/enable and model-scoped disable controls keep the complete
    // upstream policy. Even an explicitly empty/false value is preserved.
    if CONTROLS.iter().any(|key| env.get(key).is_some()) {
        return;
    }
    for (key, _) in BUCKETS {
        if env.get(key).is_none() {
            env.set(key, ttl);
        }
    }
}

pub(crate) async fn configure(
    exe: &Path,
    cwd: &Path,
    native: &Path,
    provider: &Value,
    env: &mut Sys,
    cancel: &CancellationToken,
) -> Result<(), AdapterError> {
    let Some(ttl) = requested(env)?.map(str::to_string) else {
        return Ok(());
    };
    let sources = sources(cwd, native, provider)?;
    #[cfg(windows)]
    let sources = {
        let mut sources = sources;
        sources.push(settings(Path::new(
            r"C:\Program Files\ClaudeCode\managed-settings.json",
        ))?);
        sources
    };
    let version = capture(
        exe,
        &["--version".into()],
        cwd,
        env,
        "",
        Duration::from_secs(8),
        cancel,
    )
    .await?;
    if !supported(&version) {
        return Err(route_gone(
            "Claude 缓存 TTL 策略需要 Claude Code 2.1.242 或更新版本",
        ));
    }
    apply(env, &ttl, &sources);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn environment(vars: &[(&str, &str)]) -> Sys {
        Sys::new(
            vars.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            PathBuf::from("unused-home"),
            std::env::temp_dir(),
        )
    }

    #[test]
    fn opt_in_is_validated_and_versions_fail_closed() {
        assert_eq!(requested(&environment(&[])).unwrap(), None);
        assert_eq!(
            requested(&environment(&[(POLICY, "1h")])).unwrap(),
            Some("1h")
        );
        assert!(requested(&environment(&[(POLICY, "2h")])).is_err());
        for version in ["2.1.242 (Claude Code)", "2.1.317", "2.2.0", "3.0.0"] {
            assert!(supported(version), "{version}");
        }
        for version in ["", "unexpected", "2.1", "2.1.241", "1.99.999"] {
            assert!(!supported(version), "{version}");
        }
    }

    #[test]
    fn gui_fallback_preserves_upstream_cache_controls() {
        use sa_store::efficiency::Ttl;
        for ttl in [Ttl::FiveMinutes, Ttl::OneHour] {
            for key in CONTROLS {
                let mut env = environment(&[(key, "1")]);
                select_fallback(&mut env, ttl);
                let requested = requested(&env).unwrap().unwrap().to_string();
                apply(&mut env, &requested, &[]);
                assert_eq!(env.get(key), Some("1"));
                assert_eq!(env.get(BUCKETS[0].0), None);
                assert_eq!(env.get(BUCKETS[1].0), None);
            }
            let mut env = environment(&[(BUCKETS[0].0, "upstream-main"), (BUCKETS[1].0, "upstream-child")]);
            select_fallback(&mut env, ttl);
            let requested = requested(&env).unwrap().unwrap().to_string();
            apply(&mut env, &requested, &[]);
            assert_eq!(env.get(BUCKETS[0].0), Some("upstream-main"));
            assert_eq!(env.get(BUCKETS[1].0), Some("upstream-child"));
        }
    }

    #[tokio::test]
    async fn gui_ttl_changes_reach_only_the_next_process_environment() {
        use sa_store::efficiency::{self, Ttl};
        let mut original = Sys::from_process();
        let node = original.which(if cfg!(windows) { "node.exe" } else { "node" })
            .expect("offline cache fixture needs the installed Node.js runtime");
        original.remove(POLICY);
        for key in CONTROLS.iter().copied().chain(BUCKETS.iter().map(|(key, _)| *key)) {
            original.remove(key);
        }
        let shared = efficiency::shared(Default::default());
        for (ttl, expected) in [(Ttl::Inherit, None), (Ttl::FiveMinutes, Some("5m")), (Ttl::OneHour, Some("1h")), (Ttl::Inherit, None)] {
            shared.write().unwrap().claude_cache_ttl = ttl;
            let mut env = original.clone();
            select_fallback(&mut env, shared.read().unwrap().claude_cache_ttl);
            if let Some(ttl) = requested(&env).unwrap().map(str::to_string) {
                apply(&mut env, &ttl, &[]);
            }
            let output = capture(
                &node,
                &["-e".into(), "const e=process.env; console.log(JSON.stringify({policy:e.SUPERAGENT_CLAUDE_CACHE_TTL??null,main:e.CLAUDE_CODE_PROMPT_CACHE_TTL??null,child:e.CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL??null}));".into()],
                &env.temp,
                &env,
                "",
                Duration::from_secs(5),
                &CancellationToken::new(),
            ).await.unwrap();
            assert_eq!(serde_json::from_str::<Value>(&output).unwrap(), json!({"policy":expected,"main":expected,"child":expected}));
            assert!(original.get(POLICY).is_none());
        }
        original.set(POLICY, "1h");
        select_fallback(&mut original, Ttl::Inherit);
        assert_eq!(requested(&original).unwrap(), Some("1h"));
    }

    #[test]
    fn independent_buckets_preserve_env_and_setting_overrides() {
        let mut env = environment(&[(BUCKETS[0].0, "5m")]);
        apply(&mut env, "1h", &[json!({"subagentPromptCacheTtl":"5m"})]);
        assert_eq!(env.get(BUCKETS[0].0), Some("5m"));
        assert_eq!(env.get(BUCKETS[1].0), Some("5m"));
        apply(
            &mut env,
            "1h",
            &[json!({"env":{"CLAUDE_CODE_PROMPT_CACHE_TTL":"1h"}})],
        );
        assert_eq!(env.get(BUCKETS[0].0), Some("1h"));
        let mut env = environment(&[]);
        apply(
            &mut env,
            "1h",
            &[
                json!({"promptCacheTtl":"5m", "env":{"CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL":"5m"}}),
                json!({"promptCacheTtl":"1h", "subagentPromptCacheTtl":"1h"}),
            ],
        );
        assert_eq!(env.get(BUCKETS[0].0), Some("1h"));
        assert_eq!(env.get(BUCKETS[1].0), Some("5m"));
    }

    #[test]
    fn all_upstream_controls_take_precedence_without_copying_credentials() {
        for key in CONTROLS {
            let mut env = environment(&[("ANTHROPIC_AUTH_TOKEN", "selected-token")]);
            let source = json!({"env":{key:"1", "ANTHROPIC_AUTH_TOKEN":"other-token"}});
            apply(&mut env, "1h", &[source]);
            assert_eq!(env.get(key), Some("1"));
            assert_eq!(env.get(BUCKETS[0].0), None);
            assert_eq!(env.get(BUCKETS[1].0), None);
            assert_eq!(env.get("ANTHROPIC_AUTH_TOKEN"), Some("selected-token"));
        }
        let selected = selected_settings(&json!({
            "env": {"ANTHROPIC_API_KEY":"other-token", "DISABLE_PROMPT_CACHING":"1"},
            "promptCacheTtl":"5m", "permissions":{"defaultMode":"bypassPermissions"}
        }));
        assert_eq!(
            selected,
            json!({"env":{"DISABLE_PROMPT_CACHING":"1"}, "promptCacheTtl":"5m"})
        );
    }

    #[tokio::test]
    async fn no_opt_in_never_probes_or_reads_settings() {
        let mut env = environment(&[]);
        configure(
            Path::new("does-not-exist"),
            Path::new("does-not-exist"),
            Path::new("does-not-exist"),
            &Value::Null,
            &mut env,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(env.vars().is_empty());
    }

    #[test]
    fn files_supply_cache_only_overrides_even_when_cli_setting_sources_are_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        let native = dir.path().join("native-settings.json");
        std::fs::create_dir_all(cwd.join(".claude")).unwrap();
        std::fs::write(
            &native,
            r#"{"promptCacheTtl":"5m","env":{"ANTHROPIC_AUTH_TOKEN":"unselected"}}"#,
        )
        .unwrap();
        std::fs::write(
            cwd.join(".claude/settings.json"),
            r#"{"subagentPromptCacheTtl":"1h"}"#,
        )
        .unwrap();
        std::fs::write(
            cwd.join(".claude/settings.local.json"),
            r#"{"subagentPromptCacheTtl":"5m","env":{"DISABLE_PROMPT_CACHING_FABLE":"1"}}"#,
        )
        .unwrap();
        let values = sources(&cwd, &native, &Value::Null).unwrap();
        let mut env = environment(&[("ANTHROPIC_AUTH_TOKEN", "selected")]);
        apply(&mut env, "1h", &values);
        assert_eq!(env.get(BUCKETS[0].0), Some("5m"));
        assert_eq!(env.get(BUCKETS[1].0), Some("5m"));
        assert_eq!(env.get("DISABLE_PROMPT_CACHING_FABLE"), Some("1"));
        assert_eq!(env.get("ANTHROPIC_AUTH_TOKEN"), Some("selected"));
    }

    #[tokio::test]
    async fn invalid_settings_stop_opt_in_before_launching_a_child() {
        let dir = tempfile::tempdir().unwrap();
        let native = dir.path().join("settings.json");
        std::fs::write(&native, "invalid JSON").unwrap();
        let mut env = environment(&[(POLICY, "1h")]);
        let error = configure(
            Path::new("does-not-exist"),
            dir.path(),
            &native,
            &Value::Null,
            &mut env,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(error.message.contains("settings"));
        assert_eq!(env.get(BUCKETS[0].0), None);
    }

    #[tokio::test]
    async fn version_probe_rejects_unrecognized_cli_without_model_request() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = Sys::from_process();
        let node = env
            .which(if cfg!(windows) { "node.exe" } else { "node" })
            .expect("offline cache fixture needs the installed Node.js runtime");
        env.set(POLICY, "1h");
        env.remove(BUCKETS[0].0);
        let error = configure(
            &node,
            dir.path(),
            &dir.path().join("absent-settings.json"),
            &Value::Null,
            &mut env,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(error.message.contains("2.1.242"));
        assert_eq!(env.get(BUCKETS[0].0), None);
    }

    #[tokio::test]
    async fn child_process_receives_cache_policy_and_original_route() {
        let mut env = Sys::from_process();
        let node = env
            .which(if cfg!(windows) { "node.exe" } else { "node" })
            .expect("offline cache fixture needs the installed Node.js runtime");
        for key in CONTROLS
            .iter()
            .copied()
            .chain(BUCKETS.iter().map(|(key, _)| *key))
        {
            env.remove(key);
        }
        env.set("ANTHROPIC_BASE_URL", "https://fixture.invalid");
        env.set("ANTHROPIC_AUTH_TOKEN", "fixture-only-token");
        apply(&mut env, "1h", &[]);
        let fixture = r#"const e=process.env; console.log(JSON.stringify({main:e.CLAUDE_CODE_PROMPT_CACHE_TTL,subagent:e.CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL,base:e.ANTHROPIC_BASE_URL,token:e.ANTHROPIC_AUTH_TOKEN}));"#;
        let output = capture(
            &node,
            &["-e".into(), fixture.into()],
            &env.temp,
            &env,
            "",
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&output).unwrap(),
            json!({
                "main":"1h", "subagent":"1h", "base":"https://fixture.invalid", "token":"fixture-only-token"
            })
        );
    }
}
