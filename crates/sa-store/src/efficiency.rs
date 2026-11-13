//! Operator-managed context delivery and Claude cache preferences.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};

/// GUI fallback for Claude's process-local TTL policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ttl {
    #[default]
    Inherit,
    FiveMinutes,
    OneHour,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub incremental_context: bool,
    pub lazy_mcp_schemas: bool,
    pub concise_handoffs: bool,
    pub claude_cache_ttl: Ttl,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            incremental_context: true,
            lazy_mcp_schemas: true,
            concise_handoffs: true,
            claude_cache_ttl: Ttl::Inherit,
        }
    }
}

/// Shared by the desktop host, engine and adapters. Read at each turn.
pub type Shared = Arc<RwLock<Settings>>;

pub fn shared(settings: Settings) -> Shared {
    Arc::new(RwLock::new(settings))
}

pub fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS studio_efficiency(
           singleton INTEGER PRIMARY KEY CHECK(singleton=1), settings TEXT NOT NULL);",
    )?;
    Ok(())
}

/// Read-only compatible, including journals created before these settings.
pub fn load(conn: &Connection) -> Result<Settings> {
    let exists = conn
        .prepare_cached("SELECT 1 FROM sqlite_master WHERE type='table' AND name='studio_efficiency'")?
        .exists([])?;
    if !exists {
        return Ok(Settings::default());
    }
    let value: Option<String> = conn
        .query_row(
            "SELECT settings FROM studio_efficiency WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    value
        .map(|text| serde_json::from_str(&text).context("读取 Token 与缓存设置失败"))
        .transpose()
        .map(Option::unwrap_or_default)
}

/// Call inside the host's transaction, then publish to [`Shared`] on success.
pub fn save(conn: &Connection, settings: &Settings) -> Result<()> {
    init(conn)?;
    conn.execute(
        "INSERT INTO studio_efficiency(singleton,settings) VALUES(1,?1)
         ON CONFLICT(singleton) DO UPDATE SET settings=excluded.settings",
        params![serde_json::to_string(settings)?],
    )?;
    Ok(())
}
