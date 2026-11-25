use super::{DB_FILE, JournalError};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, MAIN_DB, OpenFlags, backup::NO_PROGRESS};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Deserialize, Serialize)]
struct Marker {
    from: u32,
    to: u32,
    snapshot: PathBuf,
    started_at: SystemTime,
}

fn marker_path(dir: &Path) -> PathBuf {
    dir.join("backups").join("migration.json")
}

pub(super) fn check_marker(dir: &Path) -> Result<()> {
    let path = marker_path(dir);
    let contents = match fs::read(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("读取 {}", path.display())),
    };
    let marker: Marker = serde_json::from_slice(&contents)
        .with_context(|| format!("无法解析迁移标记 {}", path.display()))?;
    Err(JournalError::MigrationInterrupted {
        snapshot: marker.snapshot,
        from: marker.from,
        to: marker.to,
    }
    .into())
}

pub(super) fn remove_marker(dir: &Path) -> Result<()> {
    let path = marker_path(dir);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("移除迁移标记 {}", path.display())),
    }
}

// Fixed-width Unix time in UTC, in nanoseconds.
fn timestamp(time: SystemTime) -> Result<String> {
    Ok(format!(
        "{:020}",
        time.duration_since(UNIX_EPOCH)?.as_nanos()
    ))
}

fn snapshots(dir: &Path) -> io::Result<Vec<(PathBuf, SystemTime)>> {
    let mut snapshots = Vec::new();
    for entry in fs::read_dir(dir.join("backups"))? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(stamp) = name
            .to_str()
            .and_then(|name| name.strip_prefix("hub-"))
            .and_then(|name| name.strip_suffix(".sqlite3"))
        else {
            continue;
        };
        if stamp.len() == 20
            && stamp.bytes().all(|byte| byte.is_ascii_digit())
            && entry.file_type()?.is_file()
        {
            snapshots.push((entry.path(), entry.metadata()?.modified()?));
        }
    }
    snapshots.sort_unstable_by(|left, right| right.0.cmp(&left.0));
    Ok(snapshots)
}

pub(super) fn latest_snapshot(dir: &Path) -> Option<(PathBuf, SystemTime)> {
    snapshots(dir).ok()?.into_iter().next()
}

pub(super) fn prepare_migration(dir: &Path, path: &Path, from: u32, to: u32) -> Result<()> {
    let started_at = SystemTime::now();
    let stamp = timestamp(started_at)?;
    let dir = std::path::absolute(dir)?;
    let backups = dir.join("backups");
    fs::create_dir_all(&backups)?;
    let snapshot = backups.join(format!("hub-{stamp}.sqlite3"));
    let pending = backups.join(format!("hub-{stamp}.sqlite3.pending"));
    File::create_new(&pending)?;

    // The caller holds BEGIN IMMEDIATE. A separate read connection sees the
    // committed state while the writer lock excludes concurrent commits.
    let reader = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    reader.backup(MAIN_DB, &pending, NO_PROGRESS)?;
    let copy = Connection::open(&pending)?;
    let mode: String = copy.query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))?;
    ensure!(mode == "delete", "工单库快照无法切换为独立文件");
    copy.close().map_err(|(_, error)| error)?;
    OpenOptions::new().write(true).open(&pending)?.sync_all()?;
    fs::rename(&pending, &snapshot)?;

    for (old, _) in snapshots(&dir)?
        .into_iter()
        .filter(|(old, _)| old != &snapshot)
        .skip(1)
    {
        fs::remove_file(old)?;
    }

    let marker = Marker {
        from,
        to,
        snapshot,
        started_at,
    };
    let pending_marker = backups.join(format!("migration-{stamp}.json.pending"));
    let mut file = File::create_new(&pending_marker)?;
    serde_json::to_writer(&mut file, &marker)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    fs::rename(pending_marker, marker_path(&dir))?;
    Ok(())
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path);
    name.push(suffix);
    name.into()
}

struct PendingRestore<'a>(Option<&'a Path>);

impl Drop for PendingRestore<'_> {
    fn drop(&mut self) {
        if let Some(path) = self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

/// Restore a standalone snapshot. All journal connections must be closed.
/// The original database and sidecars remain together under a damaged name.
pub fn restore_snapshot(data_dir: &Path, snapshot: &Path) -> Result<()> {
    let stamp = timestamp(SystemTime::now())?;
    let path = data_dir.join(DB_FILE);
    let damaged = data_dir.join(format!("hub-damaged-{stamp}.sqlite3"));
    let staged = data_dir.join(format!("hub-restore-{stamp}.sqlite3.pending"));
    let mut source =
        File::open(snapshot).with_context(|| format!("读取快照 {}", snapshot.display()))?;
    for suffix in ["-wal", "-shm", "-journal"] {
        ensure!(
            !with_suffix(snapshot, suffix).try_exists()?,
            "恢复需要独立的工单库快照：{}",
            snapshot.display()
        );
    }
    // Arm cleanup only after creation. Declare it before the file handles
    // so they close before cleanup tries to remove the file on Windows.
    let mut pending = PendingRestore(None);
    let mut destination = File::create_new(&staged)?;
    pending.0 = Some(&staged);
    io::copy(&mut source, &mut destination)?;
    destination.sync_all()?;
    drop(destination);
    drop(source);

    let check = Connection::open_with_flags(&staged, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let pages: u32 = check.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let mode: String = check.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    ensure!(
        pages > 0 && mode != "wal",
        "恢复需要独立的工单库快照：{}",
        snapshot.display()
    );
    check.prepare("SELECT name FROM sqlite_schema")?;
    drop(check);

    let mut moved = Vec::new();
    let mut install = || -> Result<()> {
        // Keep rollback journals too, so SQLite cannot replay them on the copy.
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let original = with_suffix(&path, suffix);
            let archived = with_suffix(&damaged, suffix);
            ensure!(
                !archived.try_exists()?,
                "工单库归档已存在：{}",
                archived.display()
            );
            if original.try_exists()? {
                fs::rename(&original, &archived)?;
                moved.push((original, archived));
            }
        }
        fs::rename(&staged, &path)?;
        Ok(())
    };
    if let Err(error) = install() {
        for (original, archived) in moved.into_iter().rev() {
            fs::rename(&archived, &original).with_context(|| {
                format!("恢复失败（{error:#}），原文件位于 {}", archived.display())
            })?;
        }
        return Err(error).context("无法安装工单库快照");
    }
    pending.0 = None;
    remove_marker(data_dir)
}
