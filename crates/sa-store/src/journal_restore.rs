use super::{
    DB_FILE, FailureIdentity, JournalError, RestoreIdentity,
    backup::{
        PendingFile, durable_rename, local_name, marker_digest, marker_path, read_optional,
        remove_file, remove_marker, sync_directory, timestamp, with_suffix, write_record,
    },
    file_identity, open_immutable,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    time::SystemTime,
};

const SIDECARS: [&str; 3] = ["-wal", "-shm", "-journal"];

#[derive(Debug)]
pub enum RestoreOutcome {
    Installed,
    /// The database is in place. Use `retry_restore_cleanup` for this error.
    InstalledMarkerNotRemoved(anyhow::Error),
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Installing,
    Archived,
    Installed,
    RollingBack,
}

#[derive(Deserialize, Serialize)]
struct Record {
    snapshot_name: PathBuf,
    staged_name: String,
    damaged_name: String,
    had_journal: bool,
    migration_digest: Option<[u8; 32]>,
    phase: Phase,
}

struct Files {
    live: PathBuf,
    staged: PathBuf,
    damaged: PathBuf,
}

/// Serializes startup migration, restore and repair of a restore record.
/// Callers must also close all existing connections before restoring.
pub(super) fn lock(dir: &Path) -> Result<File> {
    let acquire = || -> io::Result<File> {
        fs::create_dir_all(dir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(".journal-restore.lock"))?;
        lock.lock()?;
        Ok(lock)
    };
    acquire().map_err(|error| {
        inspect_record(dir).map_or_else(|| error.into(), |diagnostic| diagnostic.into())
    })
}

fn record_path(dir: &Path) -> PathBuf {
    dir.join("backups").join("restore.json")
}

fn interrupted(
    dir: &Path,
    record: Option<&Record>,
    installed: bool,
    error: anyhow::Error,
) -> JournalError {
    JournalError::RestoreInterrupted {
        record: record_path(dir),
        snapshot_name: record.map(|record| record.snapshot_name.clone()),
        installed,
        reason: format!("{error:#}"),
    }
}

impl Record {
    fn files(&self, dir: &Path) -> Result<Files> {
        let stamp = self
            .staged_name
            .strip_prefix("hub-restore-")
            .and_then(|name| name.strip_suffix(".sqlite3.pending"))
            .context("恢复临时文件名无效")?;
        ensure!(
            stamp.len() == 20
                && stamp.bytes().all(|byte| byte.is_ascii_digit())
                && self.damaged_name == format!("hub-damaged-{stamp}.sqlite3")
                && self.snapshot_name.as_os_str()
                    == std::ffi::OsStr::new(local_name(&self.snapshot_name)?),
            "恢复记录的文件名无效"
        );
        Ok(Files {
            live: dir.join(DB_FILE),
            staged: dir.join(&self.staged_name),
            damaged: dir.join(&self.damaged_name),
        })
    }
}

fn read_record(dir: &Path) -> Result<Option<Record>> {
    let read = || -> Result<Option<Record>> {
        read_optional(&record_path(dir))?
            .map(|bytes| serde_json::from_slice(&bytes))
            .transpose()
            .map_err(Into::into)
    };
    read().map_err(|error| interrupted(dir, None, false, error).into())
}

pub(super) fn inspect_record(dir: &Path) -> Option<JournalError> {
    match read_record(dir) {
        Ok(None) => None,
        Ok(Some(record)) => {
            let installed = record
                .files(dir)
                .and_then(|files| is_installed(&record, &files));
            Some(match installed {
                Ok(true) => interrupted(
                    dir,
                    Some(&record),
                    true,
                    anyhow::anyhow!("快照已安装，等待清理恢复标记"),
                ),
                Ok(false) => interrupted(
                    dir,
                    Some(&record),
                    false,
                    anyhow::anyhow!("等待完成或回滚恢复"),
                ),
                Err(error) => interrupted(dir, Some(&record), false, error),
            })
        }
        Err(error) => error.downcast().ok(),
    }
}

fn is_installed(record: &Record, files: &Files) -> Result<bool> {
    Ok(matches!(record.phase, Phase::Archived | Phase::Installed)
        && regular_file(&files.live)?
        && !regular_file(&files.staged)?)
}

fn regular_file(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file(),
                "恢复路径不是普通文件：{}",
                path.display()
            );
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn move_if_present(from: &Path, to: &Path) -> Result<()> {
    if regular_file(from)? {
        durable_rename(from, to, false)
            .with_context(|| format!("移动 {} 到 {}", from.display(), to.display()))?;
    }
    Ok(())
}

// Move main first so a partially archived database is identifiable at startup.
fn install(dir: &Path, record: &mut Record, files: &Files) -> Result<()> {
    move_if_present(&files.live, &files.damaged)?;
    for suffix in SIDECARS {
        move_if_present(
            &with_suffix(&files.live, suffix),
            &with_suffix(&files.damaged, suffix),
        )?;
    }
    set_phase(dir, record, Phase::Archived)?;
    durable_rename(&files.staged, &files.live, false)?;
    Ok(())
}

// Move main last so its presence proves the archived sidecars were returned.
fn rollback(dir: &Path, record: &mut Record, files: &Files) -> Result<()> {
    set_phase(dir, record, Phase::RollingBack)?;
    for suffix in SIDECARS {
        move_if_present(
            &with_suffix(&files.damaged, suffix),
            &with_suffix(&files.live, suffix),
        )?;
    }
    move_if_present(&files.damaged, &files.live)?;
    ensure!(
        !record.had_journal || regular_file(&files.live)?,
        "未找到要恢复的原工单库"
    );
    remove_file(&files.staged)?;
    sync_directory(dir)?;
    clear_record(dir)
}

fn clear_record(dir: &Path) -> Result<()> {
    let path = record_path(dir);
    remove_file(&path)?;
    sync_directory(path.parent().expect("restore record has a parent"))?;
    Ok(())
}

fn set_phase(dir: &Path, record: &mut Record, phase: Phase) -> Result<()> {
    if record.phase != phase {
        record.phase = phase;
        write_record(&record_path(dir), record)?;
    }
    Ok(())
}

fn finish_install(
    dir: &Path,
    record: &mut Record,
    cleanup: impl FnOnce(&Path) -> Result<()>,
) -> RestoreOutcome {
    let finish = || -> Result<()> {
        sync_directory(dir)?;
        set_phase(dir, record, Phase::Installed)?;
        if let Some(bytes) = read_optional(&marker_path(dir))? {
            ensure!(
                record.migration_digest == Some(marker_digest(&bytes)),
                JournalError::RestoreCancelled
            );
            cleanup(dir)?;
            sync_directory(&dir.join("backups"))?;
        }
        clear_record(dir)
    };
    match finish() {
        Ok(()) => RestoreOutcome::Installed,
        Err(error) => RestoreOutcome::InstalledMarkerNotRemoved(error),
    }
}

/// When cleanup failed after installation, retry cleanup only. If there is
/// no record, cleanup has already finished.
pub fn retry_restore_cleanup(data_dir: &Path) -> Result<RestoreOutcome> {
    let dir = std::path::absolute(data_dir)?;
    let _lock = lock(&dir)?;
    let Some(mut record) = read_record(&dir)? else {
        return Ok(RestoreOutcome::Installed);
    };
    let installed = record
        .files(&dir)
        .and_then(|files| is_installed(&record, &files));
    match installed {
        Ok(true) => Ok(finish_install(&dir, &mut record, remove_marker)),
        Ok(false) => {
            Err(interrupted(&dir, Some(&record), false, anyhow::anyhow!("快照尚未安装")).into())
        }
        Err(error) => Err(interrupted(&dir, Some(&record), false, error).into()),
    }
}

pub(super) fn resume(dir: &Path) -> Result<()> {
    let Some(mut record) = read_record(dir)? else {
        return Ok(());
    };
    let mut installed = false;
    let mut recover = || -> Result<()> {
        let files = record.files(dir)?;
        let live = regular_file(&files.live)?;
        let staged = regular_file(&files.staged)?;
        let damaged = regular_file(&files.damaged)?;
        installed = is_installed(&record, &files)?;
        if !installed {
            if record.phase == Phase::RollingBack {
                return rollback(dir, &mut record, &files);
            }
            match (live, staged, damaged) {
                (false, true, _) => {
                    check_snapshot(&files.staged)?;
                    install(dir, &mut record, &files)?;
                    installed = true;
                }
                (false, false, true) => return rollback(dir, &mut record, &files),
                (true, _, false) if record.phase == Phase::Installing => {
                    return rollback(dir, &mut record, &files);
                }
                _ => bail!("无法确定恢复使用的工单库文件"),
            }
        }
        match finish_install(dir, &mut record, remove_marker) {
            RestoreOutcome::Installed => Ok(()),
            RestoreOutcome::InstalledMarkerNotRemoved(error) => Err(error),
        }
    };
    recover().map_err(|error| {
        let installed = installed
            || record
                .files(dir)
                .and_then(|files| is_installed(&record, &files))
                .unwrap_or(false);
        interrupted(dir, Some(&record), installed, error).into()
    })
}

fn check_snapshot(path: &Path) -> Result<()> {
    // immutable avoids creating sidecars even if a supplied snapshot used WAL.
    // Inspect the format bytes because immutable connections ignore WAL mode.
    let mut header = [0; 20];
    File::open(path)?.read_exact(&mut header)?;
    ensure!(!header[18..20].contains(&2), "恢复需要独立的工单库快照");
    let conn = open_immutable(path)?;
    let pages: u32 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    ensure!(pages > 0, "恢复需要独立的工单库快照");
    let mut statement = conn.prepare("PRAGMA quick_check")?;
    let mut rows = statement.query([])?;
    let result: String = rows.next()?.context("quick_check 未返回结果")?.get(0)?;
    ensure!(result == "ok" && rows.next()?.is_none(), "{result}");
    Ok(())
}

fn confirm_identity(dir: &Path, expected: &RestoreIdentity) -> Result<()> {
    let matches = match &expected.0 {
        FailureIdentity::Migration(digest) => read_optional(&marker_path(dir))
            .is_ok_and(|contents| contents.is_some_and(|bytes| marker_digest(&bytes) == *digest)),
        FailureIdentity::Corrupt { .. } => {
            marker_path(dir).try_exists().is_ok_and(|exists| !exists)
                && file_identity(&dir.join(DB_FILE)).ok().as_ref() == Some(expected)
        }
    };
    ensure!(matches, JournalError::RestoreCancelled);
    Ok(())
}

/// Restore while all journal connections are closed. The expected identity
/// must come from the startup error or observer inspection shown to the user.
pub fn restore_snapshot(
    data_dir: &Path,
    snapshot: &Path,
    expected: &RestoreIdentity,
) -> Result<RestoreOutcome> {
    restore_with_cleanup(data_dir, snapshot, expected, remove_marker)
}

fn restore_with_cleanup(
    data_dir: &Path,
    snapshot: &Path,
    expected: &RestoreIdentity,
    cleanup: impl FnOnce(&Path) -> Result<()>,
) -> Result<RestoreOutcome> {
    let dir = std::path::absolute(data_dir)?;
    let _lock = lock(&dir)?;
    if let Some(error) = inspect_record(&dir) {
        return Err(error.into());
    }
    confirm_identity(&dir, expected)?;
    let stamp = timestamp(SystemTime::now())?;
    let mut record = Record {
        snapshot_name: PathBuf::from(local_name(snapshot)?),
        staged_name: format!("hub-restore-{stamp}.sqlite3.pending"),
        damaged_name: format!("hub-damaged-{stamp}.sqlite3"),
        had_journal: regular_file(&dir.join(DB_FILE))?,
        migration_digest: match &expected.0 {
            FailureIdentity::Migration(digest) => Some(*digest),
            FailureIdentity::Corrupt { .. } => None,
        },
        phase: Phase::Installing,
    };
    let files = record.files(&dir)?;
    for suffix in ["", "-wal", "-shm", "-journal"] {
        ensure!(
            !with_suffix(&files.damaged, suffix).try_exists()?,
            "工单库归档已存在"
        );
    }
    let invalid = |error: anyhow::Error| JournalError::InvalidSnapshot {
        path: snapshot.to_path_buf(),
        reason: format!("{error:#}"),
    };
    for suffix in SIDECARS {
        if with_suffix(snapshot, suffix).try_exists()? {
            return Err(invalid(anyhow::anyhow!("快照包含 SQLite 附属文件")).into());
        }
    }
    let mut source = File::open(snapshot).map_err(|error| invalid(error.into()))?;
    let mut pending = PendingFile::create(&files.staged)?;
    let mut destination = OpenOptions::new().write(true).open(&files.staged)?;
    io::copy(&mut source, &mut destination)?;
    destination.sync_all()?;
    drop(destination);
    drop(source);
    check_snapshot(&files.staged).map_err(invalid)?;
    confirm_identity(&dir, expected)?;
    sync_directory(&dir)?;
    write_record(&record_path(&dir), &record)?;
    // The published record now owns the staged copy through a process exit.
    pending.disarm();

    if let Err(error) = install(&dir, &mut record, &files) {
        if is_installed(&record, &files)
            .map_err(|reason| interrupted(&dir, Some(&record), false, reason))?
        {
            return Ok(RestoreOutcome::InstalledMarkerNotRemoved(error));
        }
        rollback(&dir, &mut record, &files)
            .with_context(|| format!("无法回滚恢复（{error:#}）"))
            .map_err(|reason| interrupted(&dir, Some(&record), false, reason))?;
        return Err(error).context("无法安装工单库快照");
    }
    Ok(finish_install(&dir, &mut record, cleanup))
}

#[cfg(test)]
#[path = "../tests/journal_restore/mod.rs"]
mod tests;
