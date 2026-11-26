//! Write-side journal connection for the scheduler (`openDB`, SC:69-107).
//!
//! Unlike [`crate::Store::open`] this does not create the studio-only
//! `studio_plan` table: the engine only reads it when the studio created it.

use crate::schema::{ADDITIONS, SCHEMA};
use crate::store::DB_FILE;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use std::{
    collections::HashSet,
    fmt,
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

#[path = "journal_backup.rs"]
mod backup;

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
    InvalidMigrationMarker {
        path: PathBuf,
        reason: String,
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
            Self::InvalidMigrationMarker { path, reason } => {
                write!(f, "无法读取迁移标记（{}）：{reason}", path.display())
            }
        }
    }
}

impl std::error::Error for JournalError {}

/// Classify SQLite corruption reported by journal startup and schema setup.
pub fn classify_open_error(dir: &Path, error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref().and_then(corruption_code) {
        Some(code) => corrupt_error(dir, code).into(),
        None => error,
    }
}

fn corruption_code(error: &rusqlite::Error) -> Option<i32> {
    let rusqlite::Error::SqliteFailure(failure, _) = error else {
        return None;
    };
    let code = failure.extended_code & 0xff;
    matches!(
        code,
        rusqlite::ffi::SQLITE_CORRUPT | rusqlite::ffi::SQLITE_NOTADB
    )
    .then_some(code)
}

fn corrupt_error(dir: &Path, code: i32) -> JournalError {
    let path = dir.join(DB_FILE);
    JournalError::Corrupt {
        path,
        code,
        latest_snapshot: backup::latest_snapshot(dir),
    }
}

/// Observe the migration marker and probe the main database header without taking
/// locks or creating SQLite sidecars. This probe ignores WAL content.
pub fn inspect(data_dir: &Path) -> Option<JournalError> {
    if let Some(error) = backup::inspect_marker(data_dir) {
        return Some(error);
    }
    let result = open_immutable(&data_dir.join(DB_FILE)).and_then(|conn| {
        Ok(conn.query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))?)
    });
    corruption_code(result.err()?.downcast_ref()?).map(|code| corrupt_error(data_dir, code))
}

fn open_immutable(path: &Path) -> Result<Connection> {
    let path = std::path::absolute(path)?;
    let mut uri = url::Url::from_file_path(path)
        .map_err(|()| anyhow::anyhow!("工单库路径无法转换为 SQLite 文件 URI"))?;
    uri.query_pairs_mut().append_pair("immutable", "1");
    // Encode UNC paths as part of the URI path. SQLite rejects a remote URI
    // authority unless it was built with SQLITE_ALLOW_URI_AUTHORITY.
    let unc;
    let uri = if let Some(host) = uri.host_str() {
        unc = format!("file:////{host}{}?immutable=1", uri.path());
        unc.as_str()
    } else {
        uri.as_str()
    };
    Ok(Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?)
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
const MIGRATIONS: &[Migration] = &[
    Migration {
        to: 1,
        needs_snapshot: false,
        apply: baseline_schema,
    },
    Migration {
        to: 2,
        needs_snapshot: true,
        apply: crate::follow_queue::migrate,
    },
    Migration {
        to: 3,
        needs_snapshot: true,
        apply: crate::follow_queue::migrate_assignments,
    },
];

fn open_with_migrations(dir: &Path, migrations: &[Migration]) -> Result<Connection> {
    fs::create_dir_all(dir)?;
    // Serialize the marker lifecycle, which extends past the SQLite commit.
    let migration_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(".journal-migration.lock"))?;
    migration_lock.lock()?;
    backup::check_marker(dir)?;
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
        // The migration writer owns this directory until the marker lifecycle
        // is complete. Clean up abandoned copies before every migration step.
        backup::clean_pending_snapshots(dir)?;
        if empty && page_count(&tx)? == 0 {
            tx.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
            // Bootstrap a brand-new journal in one transaction. No prior user
            // data exists to snapshot, and no other writer can add any between steps.
            for step in migrations {
                (step.apply)(&tx)?;
                tx.pragma_update(None, "user_version", step.to)?;
            }
            tx.commit()?;
            break;
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
