//! Write-side journal connection for the scheduler (`openDB`, SC:69-107).
//!
//! Unlike [`crate::Store::open`] this does not create the studio-only
//! `studio_plan` table: the engine only reads it when the studio created it.

use crate::schema::{ADDITIONS, SCHEMA};
use crate::store::DB_FILE;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, TransactionBehavior};
use std::{
    collections::HashSet,
    fmt,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

#[path = "journal_backup.rs"]
mod backup;
pub use backup::restore_snapshot;

#[cfg(test)]
#[path = "../tests/journal_migrations/mod.rs"]
mod migration_tests;

#[derive(Debug)]
pub enum JournalError {
    MigrationInterrupted {
        snapshot: PathBuf,
        from: u32,
        to: u32,
    },
    Corrupt {
        path: PathBuf,
        code: i32,
        latest_snapshot: Option<(PathBuf, SystemTime)>,
    },
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MigrationInterrupted { snapshot, from, to } => {
                write!(
                    f,
                    "工单库迁移中断（{from} → {to}），可恢复的快照位于 {}",
                    snapshot.display()
                )
            }
            Self::Corrupt {
                path,
                code,
                latest_snapshot,
            } => {
                write!(f, "工单库损坏（SQLite {code}）：{}", path.display())?;
                if let Some((snapshot, _)) = latest_snapshot {
                    write!(f, "。最近的快照位于 {}", snapshot.display())?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for JournalError {}

/// Classify SQLite corruption reported by journal startup and schema setup.
pub fn classify_open_error(dir: &Path, error: anyhow::Error) -> anyhow::Error {
    let Some(rusqlite::Error::SqliteFailure(failure, _)) = error.downcast_ref() else {
        return error;
    };
    let code = failure.extended_code & 0xff;
    if matches!(
        code,
        rusqlite::ffi::SQLITE_CORRUPT | rusqlite::ffi::SQLITE_NOTADB
    ) {
        return JournalError::Corrupt {
            path: dir.join(DB_FILE),
            code,
            latest_snapshot: backup::latest_snapshot(dir),
        }
        .into();
    }
    error
}

/// Early-persisted native sessions (spec §9.2 D.1). Additive; older readers
/// ignore it.
pub const NATIVE_SESSIONS: &str = "CREATE TABLE IF NOT EXISTS native_sessions(
  job INTEGER NOT NULL REFERENCES jobs, turn INTEGER NOT NULL, agent TEXT NOT NULL,
  route TEXT NOT NULL, session TEXT NOT NULL, UNIQUE(job,turn,session));
CREATE INDEX IF NOT EXISTS native_sessions_job ON native_sessions(job,turn);";

/// Open (creating if needed) and migrate `<dir>/hub.sqlite3` with the
/// scheduler pragmas: `busy_timeout=5000; journal_mode=WAL; synchronous=FULL;
/// foreign_keys=ON; journal_size_limit=134217728`.
pub fn open_journal(dir: &Path) -> Result<Connection> {
    open_with_migrations(dir, MIGRATIONS).map_err(|error| classify_open_error(dir, error))
}

struct Migration {
    to: u32,
    needs_snapshot: bool,
    apply: fn(&Connection) -> Result<()>,
}

// Append a step for each new generation. The step and its version update
// commit together. Mark any step that isn't purely additive for backup.
const MIGRATIONS: &[Migration] = &[Migration {
    to: 1,
    needs_snapshot: false,
    apply: baseline_schema,
}];

fn open_with_migrations(dir: &Path, migrations: &[Migration]) -> Result<Connection> {
    backup::check_marker(dir)?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(DB_FILE);
    let mut conn = Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
    conn.busy_timeout(Duration::from_millis(5000))?;
    conn.execute_batch(
        "PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA journal_size_limit=134217728;",
    )?;
    let current = migrations.last().map_or(0, |step| step.to);
    loop {
        let empty = page_count(&conn)? == 0;
        // BEGIN IMMEDIATE initializes page 1 in an empty database. A deferred
        // transaction keeps the empty check and initial DDL in one snapshot.
        let tx = conn.transaction_with_behavior(if empty {
            TransactionBehavior::Deferred
        } else {
            TransactionBehavior::Immediate
        })?;
        backup::check_marker(dir)?;
        let from: u32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        ensure!(
            from <= current,
            "工单库版本 {from} 高于此程序支持的版本 {current}"
        );
        if from == current {
            break;
        }
        let step = migrations
            .iter()
            .find(|step| step.to == from + 1)
            .with_context(|| format!("缺少工单库从版本 {from} 开始的迁移"))?;
        if empty && page_count(&tx)? == 0 {
            tx.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        }
        if step.needs_snapshot {
            ensure!(!empty, "需要快照的迁移必须在初始建表之后运行");
            backup::prepare_migration(dir, &path, from, step.to)?;
        }
        (step.apply)(&tx)?;
        tx.pragma_update(None, "user_version", step.to)?;
        tx.commit()?;
        if step.needs_snapshot {
            backup::remove_marker(dir)?;
        }
    }
    let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    ensure!(mode == "wal", "工单库无法启用 WAL（当前模式：{mode}）");
    Ok(conn)
}

fn page_count(conn: &Connection) -> rusqlite::Result<u32> {
    conn.query_row("PRAGMA page_count", [], |row| row.get(0))
}

fn baseline_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    for (table, columns) in ADDITIONS {
        let existing: HashSet<String> = conn
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |r| r.get::<_, String>("name"))?
            .collect::<rusqlite::Result<_>>()?;
        for (name, definition) in *columns {
            if !existing.contains(*name) {
                conn.execute_batch(&format!(
                    "ALTER TABLE {table} ADD COLUMN {name} {definition}"
                ))?;
            }
        }
    }
    conn.execute_batch(NATIVE_SESSIONS)?;
    Ok(())
}
