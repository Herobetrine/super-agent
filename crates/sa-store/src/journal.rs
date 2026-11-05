//! Write-side journal connection for the scheduler (`openDB`, SC:69-107).
//!
//! Unlike [`crate::Store::open`] this does not create the studio-only
//! `studio_plan` table: the engine only reads it when the studio created it.

use crate::schema::{ADDITIONS, SCHEMA};
use crate::store::DB_FILE;
use anyhow::{Context, Result};
use rusqlite::{Connection, TransactionBehavior};
use std::{collections::HashSet, path::Path, time::Duration};

/// Early-persisted native sessions (spec §9.2 D.1). Additive; older readers
/// ignore it.
pub const NATIVE_SESSIONS: &str = "CREATE TABLE IF NOT EXISTS native_sessions(
  job INTEGER NOT NULL REFERENCES jobs, turn INTEGER NOT NULL, agent TEXT NOT NULL,
  route TEXT NOT NULL, session TEXT NOT NULL, UNIQUE(job,turn,session));
CREATE INDEX IF NOT EXISTS native_sessions_job ON native_sessions(job,turn);";

/// Open (creating if needed) and migrate `<dir>/hub.sqlite3` with the
/// scheduler pragmas: `busy_timeout=5000; journal_mode=WAL; synchronous=FULL;
/// foreign_keys=ON`. Migration runs in one `BEGIN IMMEDIATE` transaction.
pub fn open_journal(dir: &Path) -> Result<Connection> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(DB_FILE);
    let mut conn = Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
    conn.busy_timeout(Duration::from_millis(5000))?;
    let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(SCHEMA)?;
    for (table, columns) in ADDITIONS {
        let existing: HashSet<String> = tx
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |r| r.get::<_, String>("name"))?
            .collect::<rusqlite::Result<_>>()?;
        for (name, definition) in *columns {
            if !existing.contains(*name) {
                tx.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {name} {definition}"))?;
            }
        }
    }
    tx.execute_batch(NATIVE_SESSIONS)?;
    tx.commit()?;
    Ok(conn)
}
