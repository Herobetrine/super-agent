//! Last successful full executor catalogs in the writer journal.
use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::Value;
use std::collections::HashMap;

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS catalog_cache(
    agent TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, fetched_at REAL NOT NULL, value TEXT NOT NULL)";

#[derive(Clone)]
pub struct Entry {
    pub fingerprint: String,
    pub fetched_at: f64,
    pub value: Value,
}

impl Entry {
    /// Some adapters return a diagnostic catalog on a provider probe error.
    /// Keep a previously discovered directory until that source responds again.
    pub fn loses_good_source(&self, live: &Value) -> bool {
        live["providers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|row| row["catalogError"] == true)
            .any(|failed| {
                self.value["providers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|old| {
                        old["id"] == failed["id"]
                            && old["catalogError"] != true
                            && old["models"]
                                .as_array()
                                .is_some_and(|models| !models.is_empty())
                    })
            })
    }
}

pub fn load(conn: &Connection) -> Result<HashMap<String, Entry>> {
    let mut stmt = conn.prepare("SELECT agent,fingerprint,fetched_at,value FROM catalog_cache")?;
    let mut rows = stmt.query([])?;
    let mut entries = HashMap::new();
    while let Some(row) = rows.next()? {
        let raw: String = row.get(3)?;
        if let Ok(value @ Value::Object(_)) = serde_json::from_str(&raw) {
            entries.insert(
                row.get(0)?,
                Entry {
                    fingerprint: row.get(1)?,
                    fetched_at: row.get(2)?,
                    value,
                },
            );
        }
    }
    Ok(entries)
}

pub fn save(conn: &Connection, agent: &str, entry: &Entry) -> Result<()> {
    conn.execute(
        "INSERT INTO catalog_cache(agent,fingerprint,fetched_at,value) VALUES(?,?,?,?)
        ON CONFLICT(agent) DO UPDATE SET fingerprint=excluded.fingerprint,
        fetched_at=excluded.fetched_at,value=excluded.value",
        params![
            agent,
            entry.fingerprint,
            entry.fetched_at,
            entry.value.to_string()
        ],
    )?;
    Ok(())
}
