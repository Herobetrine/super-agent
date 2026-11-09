//! Input checks shared by the management API and `invoke`
//! (`runtime/extensions.cjs` X:9-62).

use anyhow::{Result, bail, ensure};
use sa_engine::protocol::{js_whitespace, utf16_len};
use sa_engine::services::strip_verbatim;
use serde_json::{Value, json};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use url::Url;

/// Largest number of skills or servers enabled for one ticket (X:17).
pub const MAX_CHOICES: usize = 16;

pub const BAD_ENDPOINT: &str = "请使用不含凭据、查询或片段的 HTTPS 地址；回环服务可使用 HTTP";

/// `nonempty(v, max)` (X:12-14): non-blank after `trim()`, at most `max`
/// UTF-16 units, no NUL.
pub fn nonempty_str(s: &str, max: usize) -> bool {
    !s.chars().all(js_whitespace) && utf16_len(s) <= max && !s.contains('\0')
}

pub fn nonempty(v: Option<&Value>, max: usize) -> bool {
    matches!(v, Some(Value::String(s)) if nonempty_str(s, max))
}

/// Skill and server ids selected for a ticket (`choices(raw)`, X:15-20).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Choices {
    pub skills: Vec<String>,
    pub servers: Vec<String>,
}

impl Choices {
    pub fn to_value(&self) -> Value {
        json!({"skills": self.skills, "servers": self.servers})
    }
}

/// `choices(raw)`: both groups present, at most 16 non-empty ids each;
/// duplicates are dropped keeping the first occurrence (`new Set`).
pub fn choices(raw: &Value) -> Result<Choices> {
    let (Some(skills), Some(servers)) = (
        raw.get("skills").and_then(Value::as_array),
        raw.get("servers").and_then(Value::as_array),
    ) else {
        bail!("扩展授权格式无效");
    };
    let mut out = [vec![], vec![]];
    for (group, list) in [skills, servers].into_iter().zip(out.iter_mut()) {
        ensure!(
            group.len() <= MAX_CHOICES && group.iter().all(|id| nonempty(Some(id), 100)),
            "每类最多启用 16 项"
        );
        for id in group.iter().filter_map(Value::as_str) {
            if !list.iter().any(|x: &String| x == id) {
                list.push(id.to_string());
            }
        }
    }
    let [skills, servers] = out;
    Ok(Choices { skills, servers })
}

/// Node's `path.isAbsolute`: on Windows a rooted path without a drive
/// (`\tools\x.exe`) also counts.
fn is_absolute(p: &Path) -> bool {
    p.is_absolute() || p.has_root()
}

/// `executable(command)` (X:21-46): an absolute path or a bare program name
/// found on `path` (`.exe` appended on Windows when there is no extension).
/// Returns the real path; on Windows it must be an `.exe`.
pub fn executable(command: Option<&Value>, path: Option<&OsStr>) -> Result<String> {
    ensure!(nonempty(command, 1024), "请提供可执行程序");
    let command = command.and_then(Value::as_str).unwrap_or_default();
    let absolute = is_absolute(Path::new(command));
    ensure!(
        absolute || !command.contains(['\\', '/', ':']),
        "命令必须是绝对路径或 PATH 中的程序名"
    );
    let candidates: Vec<PathBuf> = if absolute {
        vec![PathBuf::from(command)]
    } else {
        let name = if cfg!(windows) && Path::new(command).extension().is_none() {
            format!("{command}.exe")
        } else {
            command.to_string()
        };
        path.map(std::env::split_paths)
            .into_iter()
            .flatten()
            .filter(|p| is_absolute(p))
            .map(|p| p.join(&name))
            .collect()
    };
    for p in candidates {
        if !p.is_file() {
            continue;
        }
        let real = strip_verbatim(p.canonicalize()?);
        let real = real.to_string_lossy().into_owned();
        if cfg!(windows) {
            ensure!(
                real.to_lowercase().ends_with(".exe"),
                "Windows 服务请指定 .exe；Node 服务可使用 node 并将入口脚本作为第一个参数"
            );
        }
        return Ok(real);
    }
    bail!("未找到可执行程序，请填写已安装程序的绝对路径")
}

/// `endpoint(value)` (X:47-61): HTTPS, or HTTP on a loopback host, with no
/// credentials, query or fragment. An empty `?`/`#` is allowed like WHATWG
/// `search`/`hash`.
pub fn endpoint(value: Option<&Value>) -> Result<Url> {
    let Some(Ok(url)) = value.and_then(Value::as_str).map(Url::parse) else {
        bail!(BAD_ENDPOINT);
    };
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    ensure!(
        (url.scheme() == "https" || (local && url.scheme() == "http"))
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none_or(str::is_empty)
            && url.fragment().is_none_or(str::is_empty),
        BAD_ENDPOINT
    );
    Ok(url)
}

static TOKEN_ENV: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^MCP_[A-Z0-9_]{1,96}$").expect("valid regex"));

/// Bearer token variable name for an HTTP server (X:195): empty, or a
/// dedicated `MCP_*` variable. Only the name is stored; the value is read
/// from the environment at connect time and never persisted or journaled.
pub fn token_env(name: &str) -> Result<()> {
    ensure!(
        name.is_empty() || TOKEN_ENV.is_match(name),
        "请使用 MCP_ 开头的服务专用环境变量名"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_dedupe_and_limits() {
        let c = choices(&json!({"skills": ["a", "b", "a"], "servers": []})).unwrap();
        assert_eq!(c.skills, ["a", "b"]);
        assert_eq!(c.to_value(), json!({"skills": ["a", "b"], "servers": []}));
        let err = |v: Value| choices(&v).unwrap_err().to_string();
        assert_eq!(err(json!({"skills": []})), "扩展授权格式无效");
        assert_eq!(err(json!(null)), "扩展授权格式无效");
        let many: Vec<String> = (0..17).map(|i| i.to_string()).collect();
        assert_eq!(
            err(json!({"skills": many, "servers": []})),
            "每类最多启用 16 项"
        );
        assert_eq!(
            err(json!({"skills": [" "], "servers": []})),
            "每类最多启用 16 项"
        );
        assert_eq!(
            err(json!({"skills": [], "servers": [1]})),
            "每类最多启用 16 项"
        );
        assert_eq!(
            err(json!({"skills": ["a\0"], "servers": []})),
            "每类最多启用 16 项"
        );
        assert_eq!(
            err(json!({"skills": ["x".repeat(101)], "servers": []})),
            "每类最多启用 16 项"
        );
    }

    #[test]
    fn endpoints() {
        let ok = |s: &str| endpoint(Some(&json!(s))).map(|u| u.to_string());
        assert_eq!(
            ok("https://example.com/mcp").unwrap(),
            "https://example.com/mcp"
        );
        assert_eq!(
            ok("http://127.0.0.1:8080/mcp").unwrap(),
            "http://127.0.0.1:8080/mcp"
        );
        assert!(ok("http://localhost/mcp").is_ok());
        assert!(ok("http://[::1]:9/mcp").is_ok());
        for bad in [
            "http://remote.invalid/mcp",
            "https://user:pw@example.com/mcp",
            "https://user@example.com/mcp",
            "https://example.com/mcp?token=1",
            "https://example.com/mcp#frag",
            "ftp://127.0.0.1/mcp",
            "not a url",
        ] {
            assert_eq!(ok(bad).unwrap_err().to_string(), BAD_ENDPOINT, "{bad}");
        }
        assert_eq!(endpoint(None).unwrap_err().to_string(), BAD_ENDPOINT);
    }

    #[test]
    fn token_names() {
        assert!(token_env("").is_ok());
        assert!(token_env("MCP_GITHUB_TOKEN").is_ok());
        for bad in [
            "GITHUB_TOKEN",
            "MCP_",
            "mcp_x",
            "MCP_A-B",
            &format!("MCP_{}", "A".repeat(97)),
        ] {
            assert!(token_env(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn executables() {
        let dir = tempfile::tempdir().unwrap();
        let name = if cfg!(windows) { "tool.exe" } else { "tool" };
        let exe = dir.path().join(name);
        std::fs::write(&exe, b"").unwrap();
        let path = std::env::join_paths([Path::new("relative"), dir.path()]).unwrap();
        let real = strip_verbatim(exe.canonicalize().unwrap())
            .to_string_lossy()
            .into_owned();
        assert_eq!(executable(Some(&json!("tool")), Some(&path)).unwrap(), real);
        assert_eq!(
            executable(Some(&json!(exe.to_str().unwrap())), None).unwrap(),
            real
        );
        let err = |v: Value| executable(Some(&v), Some(&path)).unwrap_err().to_string();
        assert_eq!(err(json!("")), "请提供可执行程序");
        assert_eq!(err(json!(1)), "请提供可执行程序");
        assert_eq!(
            err(json!("bin/tool")),
            "命令必须是绝对路径或 PATH 中的程序名"
        );
        assert_eq!(err(json!("c:tool")), "命令必须是绝对路径或 PATH 中的程序名");
        assert_eq!(
            err(json!("missing")),
            "未找到可执行程序，请填写已安装程序的绝对路径"
        );
        if cfg!(windows) {
            let script = dir.path().join("server.cmd");
            std::fs::write(&script, b"").unwrap();
            assert_eq!(
                err(json!(script.to_str().unwrap())),
                "Windows 服务请指定 .exe；Node 服务可使用 node 并将入口脚本作为第一个参数"
            );
        }
    }
}
