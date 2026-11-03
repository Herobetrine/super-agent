//! Operator-managed provider registry and rotation settings.
//!
//! Custom providers point one harness (Claude Code, Codex or DeepSeek
//! Harness) at an extra Anthropic/OpenAI-compatible endpoint. Secrets are
//! never stored: `key_env` names the environment variable that holds the key.
//! Both tables are additive (`CREATE TABLE IF NOT EXISTS`); the Node engine
//! ignores them.

use anyhow::{Result, anyhow, bail};
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use url::Url;

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS studio_providers(
  id TEXT PRIMARY KEY, agent TEXT NOT NULL, name TEXT NOT NULL, base_url TEXT NOT NULL,
  key_env TEXT NOT NULL DEFAULT '', models TEXT NOT NULL DEFAULT '[]', enabled INTEGER NOT NULL DEFAULT 1,
  position INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS studio_provider_rotation(
  singleton INTEGER PRIMARY KEY CHECK(singleton=1), enabled INTEGER NOT NULL, routes TEXT NOT NULL);";

/// Harnesses that can be pointed at a custom endpoint.
pub const CUSTOM_AGENTS: [&str; 3] = ["claude", "codex", "deepseek"];
/// Harnesses a rotation order may name (custom or native providers).
pub const ROTATION_AGENTS: [&str; 4] = ["codex", "claude", "deepseek", "antigravity"];
/// Catalog provider ids of custom providers are `sa-<id>`.
pub const ROUTE_PREFIX: &str = "sa-";

pub const MAX_PROVIDERS: usize = 64;
pub const MAX_MODELS: usize = 256;
pub const MAX_ROTATION: usize = 32;
const MAX_NAME: usize = 64;
const MAX_URL: usize = 2048;

static ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9_-]{0,47}$").expect("valid regex"));
static ENV_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]{0,127}$").expect("valid regex"));
static MODEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\p{L}\p{N}_][\p{L}\p{N}_./:@+\[\]-]{0,199}$").expect("valid regex"));

/// Variables that are never credentials; reading one as a key would send
/// local machine details to the endpoint.
const NOT_SECRETS: [&str; 14] = [
    "PATH", "PATHEXT", "HOME", "USERPROFILE", "USERNAME", "APPDATA", "LOCALAPPDATA", "TEMP", "TMP", "SYSTEMROOT",
    "COMSPEC", "PWD", "SHELL", "COMPUTERNAME",
];

/// One operator-configured endpoint for one harness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomProvider {
    /// Empty on create; [`save`] assigns one.
    pub id: String,
    /// `claude` | `codex` | `deepseek`.
    pub agent: String,
    pub name: String,
    /// Claude: Anthropic API root (a trailing `/v1` is accepted). Codex and
    /// DeepSeek: OpenAI-compatible base including `/v1`.
    pub base_url: String,
    /// Name of the environment variable holding the key; empty only for
    /// loopback endpoints.
    pub key_env: String,
    pub models: Vec<String>,
    pub enabled: bool,
}

impl CustomProvider {
    /// Provider id in the harness catalog and in `Route.provider`.
    pub fn route_id(&self) -> String {
        format!("{ROUTE_PREFIX}{}", self.id)
    }
}

/// `{provider, model, mode}` as stored in a rotation order.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RouteRef {
    pub provider: String,
    pub model: String,
    pub mode: String,
}

/// Rotation switch and the ordered candidates per agent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rotation {
    pub enabled: bool,
    pub order: BTreeMap<String, Vec<RouteRef>>,
}

pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

fn loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "[::1]" | "::1" | "localhost")
}

/// HTTPS, or HTTP on a loopback host; no credentials, query or fragment.
/// Returns the URL without a trailing slash.
pub fn validate_base_url(base: &str) -> Result<String> {
    let base = base.trim();
    if base.is_empty() || base.len() > MAX_URL {
        bail!("供应商端点无效");
    }
    let u = Url::parse(base).map_err(|_| anyhow!("供应商端点无效"))?;
    let host = u.host_str().unwrap_or_default();
    let ok = !host.is_empty()
        && u.username().is_empty()
        && u.password().is_none()
        && u.query().is_none()
        && u.fragment().is_none()
        && (u.scheme() == "https" || (u.scheme() == "http" && loopback(host)));
    if !ok {
        bail!("供应商端点须为 HTTPS（本机地址可用 HTTP），且不能带凭据、查询参数或片段");
    }
    Ok(u.as_str().trim_end_matches('/').to_string())
}

pub fn is_loopback_url(base: &str) -> bool {
    Url::parse(base).is_ok_and(|u| loopback(u.host_str().unwrap_or_default()))
}

pub fn valid_model_id(id: &str) -> bool {
    MODEL.is_match(id)
}

fn clean_text(s: &str, max: usize, what: &str) -> Result<String> {
    let s = s.trim();
    if s.is_empty() || s.chars().count() > max || s.chars().any(char::is_control) {
        bail!("{what}无效");
    }
    Ok(s.to_string())
}

/// Normalized copy of `p`, or the first validation error.
pub fn validate(p: &CustomProvider) -> Result<CustomProvider> {
    let id = p.id.trim().to_string();
    if !id.is_empty() && !ID.is_match(&id) {
        bail!("供应商标识无效");
    }
    if !CUSTOM_AGENTS.contains(&p.agent.as_str()) {
        bail!("该执行端不支持自定义供应商");
    }
    let name = clean_text(&p.name, MAX_NAME, "供应商名称")?;
    let base_url = validate_base_url(&p.base_url)?;
    let key_env = p.key_env.trim().to_string();
    if key_env.is_empty() {
        if !is_loopback_url(&base_url) {
            bail!("远程端点须指定存放密钥的环境变量名");
        }
    } else if !ENV_NAME.is_match(&key_env) {
        bail!("密钥环境变量名无效（只填变量名，不要填写密钥本身）");
    } else if NOT_SECRETS.iter().any(|n| n.eq_ignore_ascii_case(&key_env)) {
        bail!("该环境变量不是密钥");
    }
    let mut seen = HashSet::new();
    let mut models = vec![];
    for m in &p.models {
        let m = m.trim();
        if m.is_empty() {
            continue;
        }
        if !valid_model_id(m) {
            bail!("模型 ID 无效：{}", m.chars().take(64).collect::<String>());
        }
        if seen.insert(m.to_string()) {
            models.push(m.to_string());
        }
    }
    if models.len() > MAX_MODELS {
        bail!("模型数量超过 {MAX_MODELS}");
    }
    Ok(CustomProvider {
        id,
        agent: p.agent.clone(),
        name,
        base_url,
        key_env,
        models,
        enabled: p.enabled,
    })
}

fn new_id(p: &CustomProvider) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let seed = format!(
        "{}\0{}\0{nanos}\0{}\0{}",
        p.agent,
        p.name,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    format!("{:x}", Sha256::digest(seed.as_bytes()))[..12].to_string()
}

/// All providers in creation order.
pub fn list(conn: &Connection) -> Result<Vec<CustomProvider>> {
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare("SELECT id,agent,name,base_url,key_env,models,enabled FROM studio_providers ORDER BY position,id")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, i64>(6)?,
        ))
    })?;
    let mut out = vec![];
    for row in rows {
        let (id, agent, name, base_url, key_env, models, enabled) = row?;
        out.push(CustomProvider {
            id,
            agent,
            name,
            base_url,
            key_env,
            models: serde_json::from_str(&models).unwrap_or_default(),
            enabled: enabled != 0,
        });
    }
    Ok(out)
}

/// Insert (empty or unknown id) or update a provider; returns its id.
pub fn save(conn: &Connection, p: &CustomProvider) -> Result<String> {
    ensure_schema(conn)?;
    let mut p = validate(p)?;
    let exists = !p.id.is_empty()
        && conn
            .query_row("SELECT 1 FROM studio_providers WHERE id=?", [&p.id], |_| Ok(()))
            .optional()?
            .is_some();
    let models = serde_json::to_string(&p.models)?;
    if exists {
        conn.execute(
            "UPDATE studio_providers SET agent=?,name=?,base_url=?,key_env=?,models=?,enabled=? WHERE id=?",
            params![p.agent, p.name, p.base_url, p.key_env, models, p.enabled as i64, p.id],
        )?;
        return Ok(p.id);
    }
    let count: i64 = conn.query_row("SELECT count(*) FROM studio_providers", [], |r| r.get(0))?;
    if count as usize >= MAX_PROVIDERS {
        bail!("自定义供应商数量超过 {MAX_PROVIDERS}");
    }
    if p.id.is_empty() {
        p.id = new_id(&p);
    }
    let position: i64 = conn.query_row("SELECT coalesce(max(position),0)+1 FROM studio_providers", [], |r| r.get(0))?;
    conn.execute(
        "INSERT INTO studio_providers(id,agent,name,base_url,key_env,models,enabled,position) VALUES(?,?,?,?,?,?,?,?)",
        params![p.id, p.agent, p.name, p.base_url, p.key_env, models, p.enabled as i64, position],
    )?;
    Ok(p.id)
}

/// Delete a provider and drop it from the rotation order. Returns whether a
/// row existed.
pub fn remove(conn: &Connection, id: &str) -> Result<bool> {
    ensure_schema(conn)?;
    let tx = conn.unchecked_transaction()?;
    let gone = tx.execute("DELETE FROM studio_providers WHERE id=?", [id])? > 0;
    if gone {
        let mut r = rotation(&tx)?;
        let route = format!("{ROUTE_PREFIX}{id}");
        for list in r.order.values_mut() {
            list.retain(|x| x.provider != route);
        }
        r.order.retain(|_, v| !v.is_empty());
        save_rotation(&tx, &r)?;
    }
    tx.commit()?;
    Ok(gone)
}

/// Stored rotation settings; disabled and empty when never saved.
pub fn rotation(conn: &Connection) -> Result<Rotation> {
    ensure_schema(conn)?;
    let row: Option<(i64, String)> = conn
        .query_row("SELECT enabled,routes FROM studio_provider_rotation WHERE singleton=1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    Ok(match row {
        Some((enabled, routes)) => Rotation {
            enabled: enabled != 0,
            order: serde_json::from_str(&routes).unwrap_or_default(),
        },
        None => Rotation::default(),
    })
}

/// Normalized copy of `r`: known agents, `cli` routes, no duplicates.
pub fn validate_rotation(r: &Rotation) -> Result<Rotation> {
    let mut order = BTreeMap::new();
    for (agent, routes) in &r.order {
        if !ROTATION_AGENTS.contains(&agent.as_str()) {
            bail!("轮换顺序含未知执行端");
        }
        let mut seen = HashSet::new();
        let mut list = vec![];
        for x in routes {
            let provider = clean_text(&x.provider, 200, "轮换供应商")?;
            let model = clean_text(&x.model, 200, "轮换模型")?;
            if x.mode != "cli" {
                bail!("轮换路由只能使用 CLI 执行方式");
            }
            if seen.insert((provider.clone(), model.clone())) {
                list.push(RouteRef {
                    provider,
                    model,
                    mode: "cli".into(),
                });
            }
        }
        if list.len() > MAX_ROTATION {
            bail!("每个执行端的轮换路由不超过 {MAX_ROTATION} 条");
        }
        if !list.is_empty() {
            order.insert(agent.clone(), list);
        }
    }
    Ok(Rotation {
        enabled: r.enabled,
        order,
    })
}

pub fn save_rotation(conn: &Connection, r: &Rotation) -> Result<()> {
    ensure_schema(conn)?;
    let r = validate_rotation(r)?;
    conn.execute(
        "INSERT INTO studio_provider_rotation(singleton,enabled,routes) VALUES(1,?,?)
         ON CONFLICT(singleton) DO UPDATE SET enabled=excluded.enabled,routes=excluded.routes",
        params![r.enabled as i64, serde_json::to_string(&r.order)?],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(agent: &str, base: &str, key: &str) -> CustomProvider {
        CustomProvider {
            id: String::new(),
            agent: agent.into(),
            name: " 网关 ".into(),
            base_url: base.into(),
            key_env: key.into(),
            models: vec!["m-1".into(), " m-1 ".into(), "claude-sonnet-4-5[1m]".into(), "".into()],
            enabled: true,
        }
    }

    #[test]
    fn base_url_rules() {
        assert_eq!(validate_base_url("https://api.example.com/v1/").unwrap(), "https://api.example.com/v1");
        assert_eq!(validate_base_url("http://127.0.0.1:8080").unwrap(), "http://127.0.0.1:8080");
        assert!(validate_base_url("http://localhost:1/v1").is_ok());
        assert!(validate_base_url("http://[::1]:9/").is_ok());
        for bad in [
            "http://api.example.com",
            "https://user:pw@api.example.com",
            "https://api.example.com/v1?key=x",
            "https://api.example.com/#f",
            "ftp://127.0.0.1",
            "not a url",
            "",
        ] {
            assert!(validate_base_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn validation_normalizes_and_rejects() {
        let p = validate(&provider("codex", "https://x.example/v1/", "MY_KEY")).unwrap();
        assert_eq!(p.name, "网关");
        assert_eq!(p.base_url, "https://x.example/v1");
        assert_eq!(p.models, vec!["m-1", "claude-sonnet-4-5[1m]"]);
        assert!(validate(&provider("antigravity", "https://x.example", "K")).is_err());
        assert!(validate(&provider("codex", "https://x.example", "")).is_err());
        assert!(validate(&provider("codex", "http://127.0.0.1:1", "")).is_ok());
        assert!(validate(&provider("codex", "https://x.example", "sk-abc def")).is_err());
        assert!(validate(&provider("codex", "https://x.example", "path")).is_err());
        let mut bad_model = provider("claude", "https://x.example", "K");
        bad_model.models = vec!["bad model".into()];
        assert!(validate(&bad_model).is_err());
        let mut bad_id = provider("claude", "https://x.example", "K");
        bad_id.id = "Bad Id".into();
        assert!(validate(&bad_id).is_err());
        let mut bad_name = provider("claude", "https://x.example", "K");
        bad_name.name = "a\nb".into();
        assert!(validate(&bad_name).is_err());
    }

    #[test]
    fn crud_round_trip_and_rotation_pruning() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(list(&conn).unwrap().is_empty());
        assert_eq!(rotation(&conn).unwrap(), Rotation::default());
        let a = save(&conn, &provider("claude", "https://a.example", "A_KEY")).unwrap();
        let b = save(&conn, &provider("codex", "http://localhost:4000/v1", "")).unwrap();
        assert_ne!(a, b);
        assert!(ID.is_match(&a));
        let all = list(&conn).unwrap();
        assert_eq!(all.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), vec![a.as_str(), b.as_str()]);
        assert_eq!(all[0].route_id(), format!("sa-{a}"));

        let mut edit = all[0].clone();
        edit.enabled = false;
        edit.models = vec!["x".into()];
        assert_eq!(save(&conn, &edit).unwrap(), a);
        let again = list(&conn).unwrap();
        assert!(!again[0].enabled);
        assert_eq!(again[0].models, vec!["x"]);
        assert_eq!(again.len(), 2);

        let mut fixed = provider("deepseek", "https://d.example/v1", "D");
        fixed.id = "my-gw".into();
        assert_eq!(save(&conn, &fixed).unwrap(), "my-gw");

        let route = |p: &str, m: &str| RouteRef {
            provider: p.into(),
            model: m.into(),
            mode: "cli".into(),
        };
        let mut order = BTreeMap::new();
        order.insert(
            "claude".to_string(),
            vec![route(&format!("sa-{a}"), "x"), route("ccs-native", "opus"), route("ccs-native", "opus")],
        );
        order.insert("codex".to_string(), vec![route(&format!("sa-{b}"), "m-1")]);
        save_rotation(&conn, &Rotation { enabled: true, order }).unwrap();
        let r = rotation(&conn).unwrap();
        assert!(r.enabled);
        assert_eq!(r.order["claude"].len(), 2, "duplicates dropped");

        assert!(remove(&conn, &a).unwrap());
        assert!(!remove(&conn, &a).unwrap());
        let r = rotation(&conn).unwrap();
        assert_eq!(r.order["claude"], vec![route("ccs-native", "opus")]);
        assert!(remove(&conn, &b).unwrap());
        assert!(!rotation(&conn).unwrap().order.contains_key("codex"));
        assert_eq!(list(&conn).unwrap().len(), 1);
    }

    #[test]
    fn rotation_validation() {
        let conn = Connection::open_in_memory().unwrap();
        let bad_agent = Rotation {
            enabled: true,
            order: BTreeMap::from([("gpt".to_string(), vec![])]),
        };
        assert!(save_rotation(&conn, &bad_agent).is_err());
        let bad_mode = Rotation {
            enabled: true,
            order: BTreeMap::from([(
                "codex".to_string(),
                vec![RouteRef {
                    provider: "openai".into(),
                    model: "gpt".into(),
                    mode: "desktop".into(),
                }],
            )]),
        };
        assert!(save_rotation(&conn, &bad_mode).is_err());
        assert_eq!(rotation(&conn).unwrap(), Rotation::default());
    }

    #[test]
    fn schema_is_additive_on_a_journal() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::journal::open_journal(dir.path()).unwrap();
        ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
        save(&conn, &provider("claude", "https://a.example", "K")).unwrap();
        drop(conn);
        let conn = crate::journal::open_journal(dir.path()).unwrap();
        assert_eq!(list(&conn).unwrap().len(), 1);
    }
}
