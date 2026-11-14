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
use std::collections::BTreeMap;
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
        .prepare_cached(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='project_knowledge'",
        )?
        .exists([])?)
}

fn rows(conn: &Connection, sql: &str, cwd: &str, limit: i64) -> Result<Vec<KnowledgeEntry>> {
    if !exists(conn)? {
        return Ok(vec![]);
    }
    let mut stmt = conn.prepare_cached(sql)?;
    let out = stmt
        .query_map(params![cwd, limit], entry_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(out)
}

fn entry_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<KnowledgeEntry> {
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
}

/// A deterministic lexical match; `score` is relevance, not correctness or
/// validation confidence. Created time remains source time, not validation time.
pub struct KnowledgeMatch {
    pub entry: KnowledgeEntry,
    pub score: i64,
}

fn cjk(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff | 0x20000..=0x3134f)
}

/// Bounded query expansion, no MATCH/LIKE syntax or user SQL. ASCII case
/// folding agrees with SQLite's built-in lower(); Chinese uses exact phrases
/// and overlapping bigrams rather than relying on English token boundaries.
fn query_terms(query: &str) -> Vec<(String, i64)> {
    const STOP: &[&str] = &[
        "the",
        "and",
        "for",
        "with",
        "from",
        "this",
        "that",
        "please",
        "fix",
        "check",
        "update",
        "implement",
        "use",
        "our",
    ];
    let query: String = query.chars().take(4096).collect();
    let mut terms = BTreeMap::<String, i64>::new();
    let mut add = |s: &str, weight| {
        let s = s.trim().to_ascii_lowercase();
        if (2..=160).contains(&s.chars().count()) && !STOP.contains(&s.as_str()) {
            let old = terms.entry(s).or_default();
            *old = (*old).max(weight);
        }
    };
    if query.chars().count() <= 160 {
        add(&query, 24);
    }
    for token in query.split(|c: char| !c.is_ascii_alphanumeric() && !"_./\\:-".contains(c)) {
        let token = token.trim_matches(['.', ':', '-', '/', '\\']);
        let specific = token.contains(['/', '\\', '.', '_', ':']);
        if token.len() >= 3 {
            add(token, if specific { 16 } else { 4 });
        }
        if specific {
            for part in token.split(|c: char| !c.is_ascii_alphanumeric()) {
                if part.len() >= 3 {
                    add(part, 4);
                }
            }
        }
    }
    for phrase in query.split(|c| !cjk(c)) {
        let chars: Vec<_> = phrase.chars().collect();
        if chars.len() <= 40 {
            add(phrase, 8);
        }
        for pair in chars.windows(2) {
            add(&pair.iter().collect::<String>(), 2);
        }
    }
    let mut terms: Vec<_> = terms.into_iter().collect();
    terms.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    terms.truncate(32);
    terms
}

/// Query the entire project's active history before limiting candidates. This
/// read-only fallback needs no FTS migration and works with existing journals.
/// Query terms and project identity are bound values. Memory/output is bounded;
/// SQL matching work grows with active entries in this one project.
pub fn relevant(
    conn: &Connection,
    cwd: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<KnowledgeMatch>> {
    if limit == 0 || !exists(conn)? {
        return Ok(vec![]);
    }
    let terms = query_terms(query);
    if terms.is_empty() {
        return Ok(vec![]);
    }
    let mut values = vec![rusqlite::types::Value::Text(cwd.to_string())];
    let mut clauses = vec![];
    for (term, weight) in terms {
        values.push(rusqlite::types::Value::Text(term));
        clauses.push(format!(
            "CASE WHEN instr(lower(body),?{})>0 THEN {weight} ELSE 0 END",
            values.len()
        ));
    }
    let score = clauses.join("+");
    values.push(rusqlite::types::Value::Integer(limit.min(256) as i64));
    let sql = format!(
        "SELECT id,cwd,kind,body,ticket,job,agent,created,archived,({score}) AS relevance
        FROM project_knowledge WHERE cwd=?1 AND archived=0 AND ({score})>0
        ORDER BY relevance DESC,id DESC LIMIT ?{}",
        values.len()
    );
    let mut stmt = conn.prepare(&sql)?;
    Ok(stmt
        .query_map(rusqlite::params_from_iter(values), |r| {
            Ok(KnowledgeMatch {
                entry: entry_from_row(r)?,
                score: r.get(9)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
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
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
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
        assert_eq!(
            rows.iter().map(|r| (r.id, r.archived)).collect::<Vec<_>>(),
            vec![(2, false), (1, true)]
        );
        assert_eq!(active(&conn, "/p", 10).unwrap().len(), 1);
        assert!(conn
            .execute("INSERT INTO project_knowledge(cwd,kind,body,created,sha256) VALUES('/p','secret','b',1,'y')", [])
            .is_err());
    }
}
