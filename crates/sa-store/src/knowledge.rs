//! Shared per-project knowledge base (`project_knowledge`).
//!
//! Every agent of every ticket whose canonical `cwd` is the same project
//! shares these entries: durable facts (`memory`), lessons from failures and
//! successes (`lesson`) and verified progress (`progress`). The scheduler
//! writes them (`SA_MEMO`, automatic root progress, operator edits) and puts
//! the newest active ones in each turn's envelope.
//!
//! The table is additive (`CREATE TABLE IF NOT EXISTS`): the Node engine
//! ignores it, and read paths treat a journal without it as empty.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// Created by the scheduler's journal opener. Active rows are unique per
/// project and body digest; archived rows keep their history.
pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS project_knowledge(
  id INTEGER PRIMARY KEY, cwd TEXT NOT NULL,
  kind TEXT NOT NULL CHECK(kind IN ('memory','lesson','progress')), body TEXT NOT NULL,
  ticket INTEGER, job INTEGER, agent TEXT, created REAL NOT NULL, sha256 TEXT NOT NULL,
  archived INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS project_knowledge_cwd ON project_knowledge(cwd,archived,id);
CREATE UNIQUE INDEX IF NOT EXISTS project_knowledge_active ON project_knowledge(cwd,sha256) WHERE archived=0;";

/// Valid `kind` values.
pub const KINDS: [&str; 3] = ["memory", "lesson", "progress"];
/// Maximum body length in UTF-16 code units (like the other control limits).
pub const MAX_BODY: usize = 2000;
/// Rows returned by [`list`]; older entries stay in the journal.
pub const LIST_LIMIT: i64 = 1000;

/// One knowledge entry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeEntry {
    pub id: i64,
    /// Canonical project directory (`tickets.cwd`).
    pub cwd: String,
    /// `memory`, `lesson` or `progress`.
    pub kind: String,
    pub body: String,
    /// Source ticket, if any.
    pub ticket: Option<i64>,
    /// Source job; `None` for operator entries.
    pub job: Option<i64>,
    /// Harness name, or `operator`.
    pub agent: Option<String>,
    /// Unix seconds.
    pub created: f64,
    pub archived: bool,
}

/// Whether the journal has the table (older journals do not).
pub fn exists(conn: &Connection) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT 1 FROM sqlite_master WHERE type='table' AND name='project_knowledge'")?
        .exists([])?)
}

fn rows(conn: &Connection, sql: &str, cwd: &str, limit: i64) -> Result<Vec<KnowledgeEntry>> {
    if !exists(conn)? {
        return Ok(vec![]);
    }
    let mut stmt = conn.prepare_cached(sql)?;
    let out = stmt
        .query_map(params![cwd, limit], |r| {
            Ok(KnowledgeEntry {
                id: r.get(0)?,
                cwd: r.get(1)?,
                kind: r.get(2)?,
                body: r.get(3)?,
                ticket: r.get(4)?,
                job: r.get(5)?,
                agent: r.get(6)?,
                created: r.get(7)?,
                archived: r.get::<_, i64>(8)? != 0,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(out)
}

/// Entries of a project, newest first, including archived ones (at most
/// [`LIST_LIMIT`]). Read-only; usable on an observer connection.
pub fn list(conn: &Connection, cwd: &str) -> Result<Vec<KnowledgeEntry>> {
    rows(
        conn,
        "SELECT id,cwd,kind,body,ticket,job,agent,created,archived FROM project_knowledge
      WHERE cwd=? ORDER BY id DESC LIMIT ?",
        cwd,
        LIST_LIMIT,
    )
}

/// Active (non-archived) entries of a project, newest first.
pub fn active(conn: &Connection, cwd: &str, limit: i64) -> Result<Vec<KnowledgeEntry>> {
    rows(
        conn,
        "SELECT id,cwd,kind,body,ticket,job,agent,created,archived FROM project_knowledge
      WHERE cwd=? AND archived=0 ORDER BY id DESC LIMIT ?",
        cwd,
        limit,
    )
}

/// [`list`] on a fresh read-only connection to `<dir>/hub.sqlite3`, for the
/// observer window (safe while another process owns the scheduler).
pub fn read(dir: &Path, cwd: &str) -> Result<Vec<KnowledgeEntry>> {
    let path = dir.join(crate::DB_FILE);
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("open {} read-only", path.display()))?;
    conn.busy_timeout(Duration::from_millis(5000))?;
    list(&conn, cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_table_reads_empty_and_list_includes_archived() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(crate::DB_FILE);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t(x)").unwrap();
        assert!(list(&conn, "/p").unwrap().is_empty());
        assert!(read(dir.path(), "/p").unwrap().is_empty());
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        let add = |cwd: &str, body: &str, sha: &str, archived: i64| {
            conn.execute(
                "INSERT INTO project_knowledge(cwd,kind,body,agent,created,sha256,archived) VALUES(?,'memory',?,'operator',1.5,?,?)",
                params![cwd, body, sha, archived],
            )
        };
        add("/p", "a", "x", 1).unwrap();
        add("/p", "a", "x", 0).unwrap();
        // Active rows are unique per project and digest.
        assert!(add("/p", "a", "x", 0).is_err());
        add("/q", "a", "x", 0).unwrap();
        let rows = read(dir.path(), "/p").unwrap();
        assert_eq!(rows.iter().map(|r| (r.id, r.archived)).collect::<Vec<_>>(), vec![(2, false), (1, true)]);
        assert_eq!(active(&conn, "/p", 10).unwrap().len(), 1);
        assert!(conn
            .execute("INSERT INTO project_knowledge(cwd,kind,body,created,sha256) VALUES('/p','secret','b',1,'y')", [])
            .is_err());
    }
}
