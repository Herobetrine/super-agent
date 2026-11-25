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

/// Keep the current row for each configured model, including its capabilities.
pub fn supplement_models(current: &mut Vec<Value>, cached: &[Value]) {
    let ids: std::collections::HashSet<_> = current
        .iter()
        .filter_map(|model| model["id"].as_str().map(str::to_owned))
        .collect();
    current.extend(
        cached
            .iter()
            .filter(|model| model["id"].as_str().is_some_and(|id| !ids.contains(id)))
            .cloned(),
    );
}

impl Entry {
    /// Iterate the new directory so removed sources stay removed. An adapter
    /// whose route ID survives endpoint edits also supplies a source identity.
    pub fn merge_failed_sources(&self, live: &mut Value, same_fingerprint: bool) {
        let Some(previous) = self.value["providers"].as_array() else {
            return;
        };
        let mut default = live["default"].clone();
        let mut restored_default = false;
        let Some(rows) = live["providers"].as_array_mut() else {
            return;
        };
        for row in rows.iter_mut().filter(|row| row["catalogError"] == true) {
            let Some(old) = previous.iter().find(|old| {
                old["id"].is_string()
                    && old["id"] == row["id"]
                    && match (
                        old["sourceIdentity"].as_str().filter(|id| !id.is_empty()),
                        row["sourceIdentity"].as_str().filter(|id| !id.is_empty()),
                    ) {
                        (Some(before), Some(after)) => before == after,
                        // Older snapshots may predate the per-source identity.
                        (None, _) => same_fingerprint,
                        (Some(_), None) => false,
                    }
                    && (old["catalogError"] != true || old["cachedModels"] == true)
                    && old["models"]
                        .as_array()
                        .is_some_and(|models| !models.is_empty())
            }) else {
                continue;
            };
            let Some(models) = row["models"].as_array_mut() else {
                continue;
            };
            let was_empty = models.is_empty();
            supplement_models(
                models,
                old["models"].as_array().expect("matched cached models"),
            );
            row["cachedModels"] = Value::Bool(true);
            if was_empty && row.get("unavailable_reason").is_none() {
                row["available"] = Value::Bool(old["available"] != false);
            }
            if row["defaultModel"].as_str().is_none_or(str::is_empty)
                && let Some(model) = old.get("defaultModel")
            {
                row["defaultModel"] = model.clone();
            }
            let old_default = &self.value["default"];
            if default["provider"] == row["id"]
                && default["provider"] == old_default["provider"]
                && default["mode"] == old_default["mode"]
                && default["model"].as_str().is_none_or(str::is_empty)
            {
                default["model"] = old_default["model"].clone();
                restored_default = true;
            }
        }
        if restored_default {
            live["default"] = default;
            live["cachedDefault"] = Value::Bool(true);
        }
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
