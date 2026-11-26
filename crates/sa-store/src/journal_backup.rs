use super::{JournalError, open_immutable};
use crate::DB_FILE;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, MAIN_DB, OpenFlags, backup::NO_PROGRESS};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug)]
pub struct RestorePlan {
    pub snapshot: PathBuf,
    pub snapshot_time: SystemTime,
    snapshot_stamp: FileStamp,
    observed: JournalObservation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct FileStamp {
    len: u64,
    modified: SystemTime,
}

fn file_stamp(path: &Path) -> io::Result<Option<FileStamp>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Ok(Some(FileStamp {
            len: meta.len(),
            modified: meta.modified()?,
        })),
        Ok(_) => Err(io::Error::new(io::ErrorKind::InvalidData, "不是普通文件")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JournalObservation {
    live: Option<FileStamp>,
    migration: Option<FileStamp>,
}

pub(super) fn observe_journal(dir: &Path) -> io::Result<JournalObservation> {
    Ok(JournalObservation {
        live: file_stamp(&dir.join(DB_FILE))?,
        migration: file_stamp(&marker_path(dir))?,
    })
}

#[derive(Debug)]
pub enum SnapshotCheck {
    Available(RestorePlan),
    Missing,
    Rejected,
}

#[derive(Debug)]
pub struct Restored {
    pub broken_dir: PathBuf,
    pub snapshot_time: SystemTime,
}

#[derive(Debug)]
pub enum RestoreError {
    Busy,
    Stale,
    Untouched { reason: String },
    Moved { reason: String, broken_dir: PathBuf },
}

// The scheduler owns desktop.lock for its entire lifetime, including shutdown.
// Keep the name in sync with sa_engine::owner::LOCK_FILE (sa-engine depends on sa-store).
const READERS_LOCK: &str = ".journal-readers.lock";
const RESTORE_LOCKS: [&str; 3] = [".journal-migration.lock", "desktop.lock", READERS_LOCK];

pub fn lock_journal_reader(data_dir: &Path) -> Result<File> {
    let file = open_lock(&data_dir.join(READERS_LOCK), true)?;
    file.lock_shared()?;
    if let Some(error) = inspect_restore(data_dir) {
        return Err(error.into());
    }
    Ok(file)
}

pub fn journal_busy(data_dir: &Path) -> bool {
    RESTORE_LOCKS
        .iter()
        .any(|name| match lock_file(&data_dir.join(name), false) {
            Ok(_lock) => false,
            Err(error) => error.kind() != io::ErrorKind::NotFound,
        })
}

fn open_lock(path: &Path, create: bool) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .open(path)
}

fn lock_file(path: &Path, create: bool) -> io::Result<File> {
    let file = open_lock(path, create)?;
    file.try_lock().map_err(|error| match error {
        TryLockError::WouldBlock => io::ErrorKind::WouldBlock.into(),
        TryLockError::Error(error) => error,
    })?;
    Ok(file)
}

pub fn restore_plan(data_dir: &Path, error: &JournalError) -> Option<RestorePlan> {
    match check_restore_plan(data_dir, error) {
        SnapshotCheck::Available(plan) => Some(plan),
        SnapshotCheck::Missing | SnapshotCheck::Rejected => None,
    }
}

pub fn check_restore_plan(data_dir: &Path, error: &JournalError) -> SnapshotCheck {
    let (candidates, observed) = match error {
        JournalError::MigrationInterrupted {
            snapshot, observed, ..
        } => (vec![snapshot.clone()], observed),
        JournalError::Corrupt { observed, .. } => (
            snapshots(data_dir)
                .unwrap_or_default()
                .into_iter()
                .map(|(path, _)| path)
                .collect(),
            observed,
        ),
        _ => return SnapshotCheck::Missing,
    };
    let Some(observed) = observed else {
        return SnapshotCheck::Missing;
    };
    let mut rejected = false;
    for snapshot in candidates {
        let snapshot_stamp = match file_stamp(&snapshot) {
            Ok(Some(stamp)) => stamp,
            Ok(None) => continue,
            Err(_) => {
                rejected = true;
                continue;
            }
        };
        if quick_check(&snapshot).is_ok()
            && file_stamp(&snapshot).is_ok_and(|after| after.as_ref() == Some(&snapshot_stamp))
        {
            return SnapshotCheck::Available(RestorePlan {
                snapshot,
                snapshot_time: snapshot_stamp.modified,
                snapshot_stamp,
                observed: observed.clone(),
            });
        }
        rejected = true;
    }
    if rejected {
        SnapshotCheck::Rejected
    } else {
        SnapshotCheck::Missing
    }
}

fn quick_check(path: &Path) -> Result<()> {
    let conn = open_immutable(path)?;
    let pages: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    ensure!(pages > 0, "快照没有数据库页面");
    let result: String = conn.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    ensure!(result == "ok", "快照校验失败：{result}");
    Ok(())
}

/// The folder stamp uses UTC and can be shown before the user confirms.
pub fn restore_backup_stamp(time: SystemTime) -> Result<String> {
    let seconds = i64::try_from(time.duration_since(UNIX_EPOCH)?.as_secs())?;
    let conn = Connection::open_in_memory()?;
    Ok(conn.query_row(
        "SELECT strftime('%Y%m%d-%H%M%S', ?1, 'unixepoch')",
        [seconds],
        |row| row.get(0),
    )?)
}

pub fn restore_from_snapshot(
    data_dir: &Path,
    plan: &RestorePlan,
) -> Result<Restored, RestoreError> {
    restore_from_snapshot_at(data_dir, plan, SystemTime::now())
}

/// Use the timestamp displayed in the confirmation so the directory name agrees
/// even when copying the snapshot takes longer than a second.
pub fn restore_from_snapshot_at(
    data_dir: &Path,
    plan: &RestorePlan,
    confirmed_at: SystemTime,
) -> Result<Restored, RestoreError> {
    restore_with(data_dir, plan, confirmed_at, rename_restore_file)
}

fn untouched(error: impl std::fmt::Display) -> RestoreError {
    RestoreError::Untouched {
        reason: error.to_string(),
    }
}

#[derive(Serialize)]
struct RestoreRecord<'a> {
    snapshot: &'a Path,
    snapshot_time: SystemTime,
    files_moved: Vec<&'a Path>,
    restored_at: SystemTime,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingRestore {
    snapshot: String,
    snapshot_stamp: FileStamp,
    confirmed_at: SystemTime,
    broken_dir: String,
    originals: [Option<FileStamp>; 5],
    files_moved: Vec<String>,
    copy: InstallCopy,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct InstallCopy {
    name: String,
    stamp: FileStamp,
}

struct OriginalLocation {
    active: PathBuf,
    backup: PathBuf,
    archived: bool,
}

fn pending_restore_path(dir: &Path) -> PathBuf {
    dir.join("backups").join("restore-pending.json")
}

fn original_paths(dir: &Path) -> [PathBuf; 5] {
    let live = dir.join(DB_FILE);
    [
        live.clone(),
        with_suffix(&live, "-wal"),
        with_suffix(&live, "-shm"),
        with_suffix(&live, "-journal"),
        marker_path(dir),
    ]
}

impl PendingRestore {
    fn directory(&self, dir: &Path) -> PathBuf {
        dir.join("backups").join(&self.broken_dir)
    }

    fn snapshot_path(&self, dir: &Path) -> PathBuf {
        dir.join("backups").join(&self.snapshot)
    }

    fn save(&self, dir: &Path) -> Result<()> {
        write_record(&pending_restore_path(dir), self)
    }

    fn locations(&self, dir: &Path) -> Result<Vec<OriginalLocation>> {
        let broken = self.directory(dir);
        ensure!(fs::symlink_metadata(&broken)?.is_dir(), "备份目录无效");
        let mut locations = Vec::new();
        for (active, expected) in original_paths(dir).into_iter().zip(&self.originals) {
            let backup = broken.join(active.file_name().context("缺少原文件名称")?);
            let live_stamp = file_stamp(&active)?;
            let backup_stamp = file_stamp(&backup)?;
            let archived = live_stamp.is_none() && expected.is_some() && &backup_stamp == expected;
            ensure!(
                archived || (backup_stamp.is_none() && &live_stamp == expected),
                "无法确认原文件的位置：{}",
                active.display()
            );
            if expected.is_some() {
                locations.push(OriginalLocation {
                    active,
                    backup,
                    archived,
                });
            }
        }
        Ok(locations)
    }

    fn installed(&self, dir: &Path) -> Result<bool> {
        if file_stamp(&dir.join(DB_FILE))?.as_ref() != Some(&self.copy.stamp)
            || file_stamp(&dir.join(&self.copy.name))?.is_some()
        {
            return Ok(false);
        }
        let broken = self.directory(dir);
        ensure!(fs::symlink_metadata(&broken)?.is_dir(), "备份目录无效");
        let originals = original_paths(dir);
        for path in &originals[1..] {
            if file_stamp(path)?.is_some() {
                return Ok(false);
            }
        }
        let originals_archived =
            originals
                .iter()
                .zip(&self.originals)
                .all(|(original, expected)| {
                    file_stamp(&broken.join(original.file_name().unwrap()))
                        .is_ok_and(|saved| &saved == expected)
                });
        Ok(originals_archived)
    }
}

fn load_pending_restore(dir: &Path) -> Result<Option<PendingRestore>> {
    let Some(bytes) = read_optional(&pending_restore_path(dir))? else {
        return Ok(None);
    };
    let pending: PendingRestore = serde_json::from_slice(&bytes)?;
    ensure!(
        local_name(Path::new(&pending.snapshot))? == pending.snapshot
            && pending.broken_dir
                == format!("broken-{}", restore_backup_stamp(pending.confirmed_at)?)
            && local_name(Path::new(&pending.copy.name))? == pending.copy.name
            && pending.copy.name.starts_with(".journal-restore-")
            && pending.copy.name.ends_with(".tmp"),
        "恢复标记中的路径无效"
    );
    Ok(Some(pending))
}

fn inspect_restore(dir: &Path) -> Option<JournalError> {
    match load_pending_restore(dir) {
        Ok(Some(pending)) => Some(JournalError::RestoreInterrupted {
            broken_dir: pending.directory(dir),
            pending: Box::new(pending),
            can_put_back: matches!(file_stamp(&dir.join(DB_FILE)), Ok(None)),
        }),
        Ok(None) => None,
        Err(error) => Some(JournalError::InvalidRestoreMarker {
            path: pending_restore_path(dir),
            reason: format!("{error:#}"),
        }),
    }
}

fn restore_locks(data_dir: &Path) -> Result<Vec<File>, RestoreError> {
    RESTORE_LOCKS
        .iter()
        .map(|name| {
            let path = data_dir.join(name);
            lock_file(&path, true).map_err(|error| {
                if error.kind() == io::ErrorKind::WouldBlock {
                    RestoreError::Busy
                } else {
                    untouched(format!("无法锁定 {}：{error}", path.display()))
                }
            })
        })
        .collect()
}

fn same_stamp(path: &Path, expected: &FileStamp) -> bool {
    file_stamp(path).is_ok_and(|actual| actual.as_ref() == Some(expected))
}

struct RestoreCopy(PathBuf);

impl Drop for RestoreCopy {
    fn drop(&mut self) {
        let _ = remove_file(&self.0);
    }
}

fn copy_snapshot(dir: &Path, snapshot: &Path) -> Result<(RestoreCopy, InstallCopy)> {
    static NEXT_COPY: AtomicU64 = AtomicU64::new(0);
    let start = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let (copy, mut file) = loop {
        let nanos = start + u128::from(NEXT_COPY.fetch_add(1, Ordering::Relaxed));
        let name = format!(".journal-restore-{}-{nanos}.tmp", std::process::id());
        let path = dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => break (RestoreCopy(path), file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    };
    let mut source =
        File::open(snapshot).with_context(|| format!("无法打开快照 {}", snapshot.display()))?;
    io::copy(&mut source, &mut file)?;
    file.sync_all()?;
    drop(file);
    quick_check(&copy.0)?;
    let install = InstallCopy {
        name: local_name(&copy.0)?.to_owned(),
        stamp: file_stamp(&copy.0)?.context("临时快照不存在")?,
    };
    Ok((copy, install))
}

fn clear_restore_marker(dir: &Path) -> Result<()> {
    retire_marker(&pending_restore_path(dir))?;
    sync_directory(&dir.join("backups"))?;
    Ok(())
}

fn sync_restore_directories(dir: &Path, pending: &PendingRestore) -> io::Result<()> {
    sync_directory(&pending.directory(dir))?;
    sync_directory(&dir.join("backups"))?;
    sync_directory(dir)
}

fn stage_originals(dir: &Path, pending: &mut PendingRestore) -> Result<()> {
    pending.files_moved.clear();
    for file in pending.locations(dir)? {
        if !file.archived {
            rename_restore_file(&file.active, &file.backup)
                .with_context(|| format!("无法移走 {}", file.active.display()))?;
            sync_restore_directories(dir, pending)?;
        }
        // A crash can happen between renaming and updating the record. The
        // original names and stamps let locations() recover that last move.
        pending
            .files_moved
            .push(local_name(&file.backup)?.to_owned());
        pending.save(dir)?;
    }
    Ok(())
}

fn move_originals_back(dir: &Path, pending: &PendingRestore) -> Result<()> {
    let mut returned: Vec<OriginalLocation> = Vec::new();
    for file in pending.locations(dir)?.into_iter().rev() {
        if !file.archived {
            continue;
        }
        if let Err(move_error) = rename_restore_file(&file.backup, &file.active) {
            let mut failures = vec![format!(
                "无法将 {} 移回：{move_error}",
                file.active.display()
            )];
            for file in returned.into_iter().rev() {
                if let Err(error) = rename_restore_file(&file.active, &file.backup) {
                    failures.push(format!(
                        "{} 已移回数据文件夹：{error}",
                        file.active.display()
                    ));
                }
            }
            anyhow::bail!(failures.join("；"));
        }
        sync_restore_directories(dir, pending)?;
        returned.push(file);
    }
    clear_restore_marker(dir)
}

fn moved_error(dir: &Path, pending: &PendingRestore, error: anyhow::Error) -> RestoreError {
    RestoreError::Moved {
        reason: format!("{error:#}"),
        broken_dir: pending.directory(dir),
    }
}

fn finish_restore(dir: &Path, pending: &PendingRestore) -> Result<Restored> {
    sync_directory(dir).context("无法同步恢复后的工单库目录")?;
    clear_restore_marker(dir).context("无法清除恢复标记")?;
    let broken_dir = pending.directory(dir);
    let snapshot = pending.snapshot_path(dir);
    let files: Vec<_> = original_paths(dir)
        .iter()
        .zip(&pending.originals)
        .filter(|(_, stamp)| stamp.is_some())
        .map(|(path, _)| broken_dir.join(path.file_name().unwrap()))
        .collect();
    let record = RestoreRecord {
        snapshot: &snapshot,
        snapshot_time: pending.snapshot_stamp.modified,
        files_moved: files.iter().map(PathBuf::as_path).collect(),
        restored_at: SystemTime::now(),
    };
    let record_path = broken_dir.join("restore.json");
    if let Err(error) = write_record(&record_path, &record) {
        let _ = writeln!(
            io::stderr().lock(),
            "无法写入恢复记录 {}：{error:#}",
            record_path.display()
        );
    }
    Ok(Restored {
        broken_dir,
        snapshot_time: pending.snapshot_stamp.modified,
    })
}

fn require_pending(dir: &Path, expected: &PendingRestore) -> Result<PendingRestore, RestoreError> {
    match load_pending_restore(dir).map_err(untouched)? {
        Some(pending) if &pending == expected => Ok(pending),
        _ => Err(RestoreError::Stale),
    }
}

fn remove_previous_copy(dir: &Path, pending: &PendingRestore) -> io::Result<()> {
    let copy = dir.join(&pending.copy.name);
    if same_stamp(&copy, &pending.copy.stamp) {
        remove_file(&copy)?;
    }
    Ok(())
}

pub fn continue_restore(dir: &Path, expected: &PendingRestore) -> Result<Restored, RestoreError> {
    let _locks = restore_locks(dir)?;
    let mut pending = require_pending(dir, expected)?;
    let installed = pending
        .installed(dir)
        .map_err(|error| moved_error(dir, &pending, error))?;
    let snapshot = pending.snapshot_path(dir);
    if !installed
        && (!same_stamp(&snapshot, &pending.snapshot_stamp) || pending.locations(dir).is_err())
    {
        return Err(RestoreError::Stale);
    }
    let mut action = || -> Result<Restored> {
        let live = dir.join(DB_FILE);
        if installed {
            quick_check(&live)?;
        } else {
            let (copy, install) = copy_snapshot(dir, &snapshot)?;
            ensure!(
                same_stamp(&snapshot, &pending.snapshot_stamp),
                "记录的快照已经变化"
            );
            remove_previous_copy(dir, &pending)?;
            pending.copy = install;
            pending.save(dir)?;
            stage_originals(dir, &mut pending)?;
            rename_restore_file(&copy.0, &live).context("无法安装快照")?;
        }
        finish_restore(dir, &pending)
    };
    action().map_err(|error| moved_error(dir, &pending, error))
}

pub fn put_back_originals(dir: &Path, expected: &PendingRestore) -> Result<(), RestoreError> {
    let _locks = restore_locks(dir)?;
    let pending = require_pending(dir, expected)?;
    if !matches!(file_stamp(&dir.join(DB_FILE)), Ok(None)) {
        return Err(RestoreError::Stale);
    }
    remove_previous_copy(dir, &pending)
        .context("无法清理临时快照")
        .and_then(|()| move_originals_back(dir, &pending))
        .map_err(|error| moved_error(dir, &pending, error))
}

fn restore_with(
    data_dir: &Path,
    plan: &RestorePlan,
    confirmed_at: SystemTime,
    install: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> Result<Restored, RestoreError> {
    let _locks = restore_locks(data_dir)?;
    if inspect_restore(data_dir).is_some()
        || !observe_journal(data_dir).is_ok_and(|observed| observed == plan.observed)
        || !same_stamp(&plan.snapshot, &plan.snapshot_stamp)
    {
        return Err(RestoreError::Stale);
    }
    let (copy, install_copy) =
        copy_snapshot(data_dir, &plan.snapshot).map_err(|error| untouched(format!("{error:#}")))?;
    if !observe_journal(data_dir).is_ok_and(|observed| observed == plan.observed)
        || !same_stamp(&plan.snapshot, &plan.snapshot_stamp)
    {
        return Err(RestoreError::Stale);
    }

    let snapshot = local_name(&plan.snapshot).map_err(untouched)?.to_owned();
    let backups = data_dir.join("backups");
    if backups.join(&snapshot) != plan.snapshot {
        return Err(RestoreError::Stale);
    }
    let mut originals = std::array::from_fn(|_| None);
    for (stamp, path) in originals.iter_mut().zip(original_paths(data_dir)) {
        *stamp = file_stamp(&path).map_err(untouched)?;
    }
    if originals[0] != plan.observed.live || originals[4] != plan.observed.migration {
        return Err(RestoreError::Stale);
    }
    let stamp = restore_backup_stamp(confirmed_at).map_err(untouched)?;
    let mut pending = PendingRestore {
        snapshot,
        snapshot_stamp: plan.snapshot_stamp.clone(),
        confirmed_at,
        broken_dir: format!("broken-{stamp}"),
        originals,
        files_moved: Vec::new(),
        copy: install_copy,
    };
    let broken_dir = pending.directory(data_dir);
    fs::create_dir(&broken_dir).map_err(|error| {
        untouched(format!(
            "无法创建备份目录 {}：{error}",
            broken_dir.display()
        ))
    })?;
    if let Err(error) = pending.save(data_dir) {
        if matches!(load_pending_restore(data_dir), Ok(None)) {
            let _ = fs::remove_dir(&broken_dir);
        }
        return Err(untouched(format!("{error:#}")));
    }

    let published = stage_originals(data_dir, &mut pending)
        .and_then(|()| install(&copy.0, &data_dir.join(DB_FILE)).context("无法安装快照"));
    if let Err(error) = published {
        return match move_originals_back(data_dir, &pending) {
            Ok(()) => {
                let _ = fs::remove_dir(&broken_dir);
                Err(untouched(format!("{error:#}")))
            }
            Err(rollback) => Err(moved_error(
                data_dir,
                &pending,
                error.context(format!("无法放回原文件：{rollback:#}")),
            )),
        };
    }
    finish_restore(data_dir, &pending).map_err(|error| moved_error(data_dir, &pending, error))
}

#[cfg(windows)]
fn rename_restore_file(from: &Path, to: &Path) -> io::Result<()> {
    durable_rename(from, to, false)
}

// The caller tracks each completed move before syncing directories.
#[cfg(not(windows))]
fn rename_restore_file(from: &Path, to: &Path) -> io::Result<()> {
    match fs::symlink_metadata(to) {
        Ok(_) => Err(io::ErrorKind::AlreadyExists.into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::rename(from, to),
        Err(error) => Err(error),
    }
}

#[derive(Deserialize, Serialize)]
struct Marker {
    from: u32,
    to: u32,
    snapshot: PathBuf,
    started_at: SystemTime,
}

pub(super) fn marker_path(dir: &Path) -> PathBuf {
    dir.join("backups").join("migration.json")
}

pub(super) fn inspect_marker(dir: &Path) -> Option<JournalError> {
    if let Some(error) = inspect_restore(dir) {
        return Some(error);
    }
    let path = marker_path(dir);
    let read = || -> Result<Option<JournalError>> {
        let observed = observe_journal(dir).ok();
        let Some(contents) = read_optional(&path)? else {
            return Ok(None);
        };
        let marker: Marker = serde_json::from_slice(&contents)?;
        let name = local_name(&marker.snapshot)?;
        Ok(Some(JournalError::MigrationInterrupted {
            snapshot: dir.join("backups").join(name),
            from: marker.from,
            to: marker.to,
            observed,
        }))
    };
    read().unwrap_or_else(|error| {
        Some(JournalError::InvalidMigrationMarker {
            path,
            reason: format!("{error:#}"),
        })
    })
}

pub(super) fn check_marker(dir: &Path) -> Result<()> {
    match inspect_marker(dir) {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}

fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

// Return a basename even for legacy absolute paths, including Windows paths
// read on a different platform.
fn local_name(path: &Path) -> Result<&str> {
    let text = path.to_str().context("快照文件名无法解码")?;
    let name = text.rsplit(['/', '\\']).next().unwrap_or_default();
    ensure!(
        !name.is_empty() && !name.ends_with(['.', ' ']) && !name.contains(':'),
        "快照文件名无效"
    );
    Ok(name)
}

pub(super) fn remove_marker(dir: &Path) -> Result<()> {
    remove_marker_with_sync(dir, sync_directory)
}

pub(super) fn remove_marker_with_sync(
    dir: &Path,
    sync: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<()> {
    let path = marker_path(dir);
    retire_marker(&path).with_context(|| format!("移除迁移标记 {}", path.display()))?;
    let backups = dir.join("backups");
    sync(&backups).with_context(|| format!("同步迁移备份目录 {}", backups.display()))
}

#[cfg(not(windows))]
fn retire_marker(path: &Path) -> Result<()> {
    Ok(remove_file(path)?)
}

#[cfg(windows)]
fn retire_marker(path: &Path) -> Result<()> {
    // Use a write-through rename to remove the active name. Windows does not
    // expose the same directory-fsync contract as Unix.
    let stamp = timestamp(SystemTime::now())?;
    let retired = with_suffix(path, &format!("-{stamp}.pending"));
    match durable_rename(path, &retired, false) {
        Ok(()) => Ok(remove_file(&retired)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

// Fixed-width Unix time in UTC, in nanoseconds.
fn timestamp(time: SystemTime) -> Result<String> {
    Ok(format!(
        "{:020}",
        time.duration_since(UNIX_EPOCH)?.as_nanos()
    ))
}

fn is_snapshot_name(name: &std::ffi::OsStr, suffix: &str) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix("hub-"))
        .and_then(|name| name.strip_suffix(suffix))
        .is_some_and(|stamp| stamp.len() == 20 && stamp.bytes().all(|byte| byte.is_ascii_digit()))
}

fn snapshots(dir: &Path) -> io::Result<Vec<(PathBuf, SystemTime)>> {
    let mut snapshots = Vec::new();
    for entry in fs::read_dir(dir.join("backups"))? {
        let entry = entry?;
        if is_snapshot_name(&entry.file_name(), ".sqlite3") && entry.file_type()?.is_file() {
            snapshots.push((entry.path(), entry.metadata()?.modified()?));
        }
    }
    snapshots.sort_unstable_by(|left, right| right.0.cmp(&left.0));
    Ok(snapshots)
}

pub(super) fn clean_pending_snapshots(dir: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(dir.join("backups")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        if is_snapshot_name(&entry.file_name(), ".sqlite3.pending") && entry.file_type()?.is_file()
        {
            remove_file(&entry.path())?;
        }
    }
    Ok(())
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
    publish_snapshot(&snapshot, |pending| {
        // BEGIN IMMEDIATE excludes writers while a separate read connection
        // takes a backup of the last committed state.
        let reader = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        reader.backup(MAIN_DB, pending, NO_PROGRESS)?;
        let copy = Connection::open(pending)?;
        let mode: String = copy.query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))?;
        ensure!(mode == "delete", "工单库快照无法切换为独立文件");
        copy.close().map_err(|(_, error)| error)?;
        Ok(())
    })?;

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
        snapshot: PathBuf::from(local_name(&snapshot)?),
        started_at,
    };
    write_record(&marker_path(&dir), &marker)
}

pub(super) fn publish_snapshot(
    path: &Path,
    backup: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let pending = with_suffix(path, ".pending");
    let mut cleanup = PendingFile::create(&pending)?;
    backup(&pending)?;
    OpenOptions::new().write(true).open(&pending)?.sync_all()?;
    durable_rename(&pending, path, false)?;
    cleanup.disarm();
    Ok(())
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path);
    name.push(suffix);
    name.into()
}

struct PendingFile<'a>(Option<&'a Path>);

impl<'a> PendingFile<'a> {
    fn create(path: &'a Path) -> io::Result<Self> {
        File::create_new(path)?;
        Ok(Self(Some(path)))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for PendingFile<'_> {
    fn drop(&mut self) {
        if let Some(path) = self.0 {
            let _ = remove_file(path);
        }
    }
}

fn write_record(path: &Path, record: &impl Serialize) -> Result<()> {
    let directory = path.parent().context("迁移标记缺少目录")?;
    fs::create_dir_all(directory)?;
    // Persist the backups directory itself before publishing a record in it.
    sync_directory(directory.parent().context("缺少工单库目录")?)?;
    let stamp = timestamp(SystemTime::now())?;
    let pending = with_suffix(path, &format!("-{stamp}.pending"));
    let mut cleanup = PendingFile::create(&pending)?;
    let mut file = OpenOptions::new().write(true).open(&pending)?;
    serde_json::to_writer(&mut file, record)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    durable_rename(&pending, path, true)?;
    cleanup.disarm();
    Ok(())
}

#[cfg(windows)]
fn durable_rename(from: &Path, to: &Path, replace: bool) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let wide = |path: &Path| -> io::Result<Vec<u16>> {
        let path = std::path::absolute(path)?;
        let mut units: Vec<u16> = path.as_os_str().encode_wide().collect();
        if units.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains NUL",
            ));
        }
        units.push(0);
        Ok(units)
    };
    let from = wide(from)?;
    let to = wide(to)?;
    let flags = MOVEFILE_WRITE_THROUGH
        | if replace {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    // Both NUL-terminated buffers live until the call returns.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), flags) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
fn durable_rename(from: &Path, to: &Path, replace: bool) -> io::Result<()> {
    if !replace && to.try_exists()? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "rename destination exists",
        ));
    }
    fs::rename(from, to)?;
    sync_directory(to.parent().unwrap_or_else(|| Path::new(".")))?;
    if from.parent() != to.parent() {
        sync_directory(from.parent().unwrap_or_else(|| Path::new(".")))?;
    }
    Ok(())
}

#[cfg(windows)]
fn sync_directory(_dir: &Path) -> io::Result<()> {
    // Marker publication and removal both use write-through renames.
    Ok(())
}

#[cfg(not(windows))]
fn sync_directory(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(test)]
mod restore_tests {
    use super::*;
    use crate::journal::{inspect, open_journal};

    fn fixture(dir: &Path) -> RestorePlan {
        let live = dir.join(DB_FILE);
        let db = open_journal(dir).unwrap();
        db.execute_batch("CREATE TABLE fixture (value); INSERT INTO fixture VALUES (42);")
            .unwrap();
        drop(db);
        prepare_migration(dir, &live, 1, 2).unwrap();
        let db = Connection::open(live).unwrap();
        db.execute_batch("UPDATE fixture SET value = 99").unwrap();
        drop(db);
        for (suffix, bytes) in [("-wal", "wal"), ("-shm", "shm"), ("-journal", "journal")] {
            fs::write(with_suffix(&dir.join(DB_FILE), suffix), bytes).unwrap();
        }
        restore_plan(dir, &inspect(dir).unwrap()).unwrap()
    }

    fn originals(dir: &Path) -> [PathBuf; 5] {
        [
            dir.join(DB_FILE),
            dir.join("hub.sqlite3-wal"),
            dir.join("hub.sqlite3-shm"),
            dir.join("hub.sqlite3-journal"),
            marker_path(dir),
        ]
    }

    #[test]
    fn restores_the_snapshot_and_preserves_every_original() {
        let dir = tempfile::tempdir().unwrap();
        let plan = fixture(dir.path());
        let originals = originals(dir.path());
        let bytes = originals.each_ref().map(|path| fs::read(path).unwrap());
        let restored = restore_from_snapshot(dir.path(), &plan).unwrap();
        assert_eq!(restored.snapshot_time, plan.snapshot_time);
        for (path, bytes) in originals.iter().zip(bytes) {
            let saved = restored.broken_dir.join(path.file_name().unwrap());
            assert_eq!(fs::read(saved).unwrap(), bytes);
        }
        assert!(originals[1..].iter().all(|path| !path.exists()));
        assert_eq!(
            fs::read(&originals[0]).unwrap(),
            fs::read(&plan.snapshot).unwrap()
        );
        let record: serde_json::Value =
            serde_json::from_slice(&fs::read(restored.broken_dir.join("restore.json")).unwrap())
                .unwrap();
        assert_eq!(record["files_moved"].as_array().unwrap().len(), 5);
        assert_eq!(
            record["snapshot"],
            serde_json::to_value(&plan.snapshot).unwrap()
        );
        assert_eq!(
            record["snapshot_time"],
            serde_json::to_value(plan.snapshot_time).unwrap()
        );
        assert!(record["restored_at"].is_object());
        let journal = open_journal(dir.path()).unwrap();
        let value: i64 = journal
            .query_row("SELECT value FROM fixture", [], |r| r.get(0))
            .unwrap();
        assert_eq!(value, 42);
    }

    #[test]
    fn a_record_write_failure_still_leaves_a_successful_restore() {
        let dir = tempfile::tempdir().unwrap();
        let plan = fixture(dir.path());
        let live = dir.path().join(DB_FILE);
        let original = fs::read(&live).unwrap();
        let confirmed_at = SystemTime::now();
        let stamp = restore_backup_stamp(confirmed_at).unwrap();
        let broken_dir = dir.path().join("backups").join(format!("broken-{stamp}"));
        let record_path = broken_dir.join("restore.json");
        let restored = restore_with(dir.path(), &plan, confirmed_at, |temp, live| {
            fs::create_dir(&record_path)?;
            rename_restore_file(temp, live)
        })
        .unwrap();
        assert_eq!(restored.broken_dir, broken_dir);
        assert_eq!(restored.snapshot_time, plan.snapshot_time);
        assert!(record_path.is_dir());
        assert_eq!(fs::read(broken_dir.join(DB_FILE)).unwrap(), original);
        assert_eq!(fs::read(live).unwrap(), fs::read(&plan.snapshot).unwrap());
        let journal = open_journal(dir.path()).unwrap();
        let value: i64 = journal
            .query_row("SELECT value FROM fixture", [], |r| r.get(0))
            .unwrap();
        assert_eq!(value, 42);
    }

    #[test]
    fn rejects_a_corrupt_copy_before_moving_anything() {
        let dir = tempfile::tempdir().unwrap();
        let mut plan = fixture(dir.path());
        fs::write(&plan.snapshot, b"invalid SQLite data").unwrap();
        // Model corruption with matching observations so the final copied-file
        // check, rather than stale-plan detection, must reject these bytes.
        plan.snapshot_stamp = file_stamp(&plan.snapshot).unwrap().unwrap();
        let originals = originals(dir.path());
        let bytes = originals.each_ref().map(|path| fs::read(path).unwrap());
        assert!(matches!(
            restore_from_snapshot(dir.path(), &plan),
            Err(RestoreError::Untouched { .. })
        ));
        assert_eq!(
            originals.each_ref().map(|path| fs::read(path).unwrap()),
            bytes
        );
        assert_eq!(fs::read_dir(dir.path().join("backups")).unwrap().count(), 2);
        assert!(fs::read_dir(dir.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".journal-restore-")
        }));
    }

    #[test]
    fn each_exclusive_lock_prevents_restore_and_clears_when_released() {
        let dir = tempfile::tempdir().unwrap();
        let plan = fixture(dir.path());
        for name in RESTORE_LOCKS {
            let lock = lock_file(&dir.path().join(name), true).unwrap();
            assert!(journal_busy(dir.path()));
            assert!(matches!(
                restore_from_snapshot(dir.path(), &plan),
                Err(RestoreError::Busy)
            ));
            drop(lock);
            assert!(!journal_busy(dir.path()));
        }
        assert_eq!(fs::read_dir(dir.path().join("backups")).unwrap().count(), 2);
    }

    #[test]
    fn a_shared_reader_blocks_restore_until_the_connection_releases_its_lock() {
        let dir = tempfile::tempdir().unwrap();
        let plan = fixture(dir.path());
        let first = lock_journal_reader(dir.path()).unwrap();
        let second = lock_journal_reader(dir.path()).unwrap();
        assert!(journal_busy(dir.path()));
        assert!(matches!(
            restore_from_snapshot(dir.path(), &plan),
            Err(RestoreError::Busy)
        ));
        assert!(!pending_restore_path(dir.path()).exists());
        drop(first);
        assert!(journal_busy(dir.path()));
        drop(second);
        assert!(!journal_busy(dir.path()));
        restore_from_snapshot(dir.path(), &plan).unwrap();
    }

    #[test]
    fn stale_plan_cannot_replace_a_restored_journal_with_new_records() {
        let dir = tempfile::tempdir().unwrap();
        let plan = fixture(dir.path());
        let first = restore_from_snapshot(dir.path(), &plan).unwrap();
        let journal = open_journal(dir.path()).unwrap();
        journal
            .execute("INSERT INTO fixture VALUES (100)", [])
            .unwrap();
        drop(journal);
        let live = dir.path().join(DB_FILE);
        let bytes = fs::read(&live).unwrap();
        assert!(matches!(
            restore_from_snapshot(dir.path(), &plan),
            Err(RestoreError::Stale)
        ));
        assert_eq!(fs::read(live).unwrap(), bytes);
        assert!(!pending_restore_path(dir.path()).exists());
        assert!(first.broken_dir.join(DB_FILE).is_file());
    }

    #[test]
    fn changes_to_each_observed_path_make_the_plan_stale() {
        for name in [DB_FILE, "migration.json", "snapshot"] {
            let dir = tempfile::tempdir().unwrap();
            let plan = fixture(dir.path());
            let path = match name {
                DB_FILE => dir.path().join(DB_FILE),
                "migration.json" => marker_path(dir.path()),
                _ => plan.snapshot.clone(),
            };
            fs::write(&path, b"new bytes").unwrap();
            let original = fs::read(dir.path().join(DB_FILE)).unwrap();
            assert!(
                matches!(
                    restore_from_snapshot(dir.path(), &plan),
                    Err(RestoreError::Stale)
                ),
                "{name}"
            );
            assert_eq!(fs::read(path).unwrap(), b"new bytes");
            assert_eq!(fs::read(dir.path().join(DB_FILE)).unwrap(), original);
            assert!(!pending_restore_path(dir.path()).exists());
        }
    }

    #[test]
    fn snapshot_modified_time_alone_can_make_a_plan_stale() {
        let dir = tempfile::tempdir().unwrap();
        let plan = fixture(dir.path());
        let file = OpenOptions::new().write(true).open(&plan.snapshot).unwrap();
        let modified = plan.snapshot_time + std::time::Duration::from_secs(120);
        file.set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        drop(file);
        let actual = file_stamp(&plan.snapshot).unwrap().unwrap();
        assert_eq!(actual.len, plan.snapshot_stamp.len);
        assert_ne!(actual.modified, plan.snapshot_stamp.modified);
        assert!(matches!(
            restore_from_snapshot(dir.path(), &plan),
            Err(RestoreError::Stale)
        ));
        assert!(marker_path(dir.path()).exists());
        assert!(!pending_restore_path(dir.path()).exists());
    }

    fn interrupted_restore(dir: &Path, plan: &RestorePlan, installed: bool) -> Box<PendingRestore> {
        let crash = std::panic::catch_unwind(|| {
            restore_with(dir, plan, SystemTime::now(), |copy, live| {
                assert!(pending_restore_path(dir).is_file());
                assert!(!live.exists());
                if installed {
                    rename_restore_file(copy, live).unwrap();
                }
                panic!("simulated stop before clearing the pending marker");
            })
        });
        assert!(crash.is_err());
        let Some(JournalError::RestoreInterrupted {
            pending,
            can_put_back,
            ..
        }) = inspect(dir)
        else {
            panic!("startup should diagnose the pending restore");
        };
        assert_eq!(can_put_back, !installed);
        assert!(matches!(
            open_journal(dir)
                .unwrap_err()
                .downcast_ref::<JournalError>(),
            Some(JournalError::RestoreInterrupted { .. })
        ));
        assert_eq!(dir.join(DB_FILE).is_file(), installed);
        pending
    }

    #[test]
    fn pending_restore_blocks_startup_and_continues_with_or_without_an_installed_copy() {
        for installed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let plan = fixture(dir.path());
            let mut pending = interrupted_restore(dir.path(), &plan, installed);
            assert_eq!(pending.files_moved.len(), 5);
            if !installed {
                // The rename can reach disk before the progress update.
                pending.files_moved.pop();
                pending.save(dir.path()).unwrap();
            }
            let result = continue_restore(dir.path(), &pending).unwrap();
            assert_eq!(result.snapshot_time, plan.snapshot_time);
            assert!(!pending_restore_path(dir.path()).exists());
            assert_eq!(
                fs::read(dir.path().join(DB_FILE)).unwrap(),
                fs::read(plan.snapshot).unwrap()
            );
            assert!(result.broken_dir.join(DB_FILE).exists());
            assert!(result.broken_dir.join("restore.json").exists());
            assert!(open_journal(dir.path()).is_ok());
        }
    }

    #[test]
    fn pending_restore_can_put_back_every_original_and_cannot_overwrite_a_live_journal() {
        let dir = tempfile::tempdir().unwrap();
        let plan = fixture(dir.path());
        let originals = originals(dir.path());
        let bytes = originals.each_ref().map(|path| fs::read(path).unwrap());
        let pending = interrupted_restore(dir.path(), &plan, false);
        fs::write(&originals[0], b"another journal").unwrap();
        assert!(matches!(
            put_back_originals(dir.path(), &pending),
            Err(RestoreError::Stale)
        ));
        assert_eq!(fs::read(&originals[0]).unwrap(), b"another journal");
        fs::remove_file(&originals[0]).unwrap();

        put_back_originals(dir.path(), &pending).unwrap();
        assert!(!pending_restore_path(dir.path()).exists());
        assert_eq!(
            originals.each_ref().map(|path| fs::read(path).unwrap()),
            bytes
        );
    }

    #[test]
    fn an_unreadable_pending_record_still_blocks_journal_creation() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("backups")).unwrap();
        fs::write(pending_restore_path(dir.path()), "truncated").unwrap();
        assert!(matches!(
            inspect(dir.path()),
            Some(JournalError::InvalidRestoreMarker { .. })
        ));
        assert!(matches!(
            open_journal(dir.path())
                .unwrap_err()
                .downcast_ref::<JournalError>(),
            Some(JournalError::InvalidRestoreMarker { .. })
        ));
        assert!(!dir.path().join(DB_FILE).exists());
    }

    #[cfg(windows)]
    #[test]
    fn installed_journal_has_no_temporary_attribute() {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_TEMPORARY;

        let dir = tempfile::tempdir().unwrap();
        let plan = fixture(dir.path());
        restore_from_snapshot(dir.path(), &plan).unwrap();
        let attrs = fs::metadata(dir.path().join(DB_FILE))
            .unwrap()
            .file_attributes();
        assert_eq!(attrs & FILE_ATTRIBUTE_TEMPORARY, 0);
    }

    #[test]
    fn a_failed_install_rolls_back_every_move() {
        let dir = tempfile::tempdir().unwrap();
        let plan = fixture(dir.path());
        let originals = originals(dir.path());
        let bytes = originals.each_ref().map(|path| fs::read(path).unwrap());
        let result = restore_with(dir.path(), &plan, SystemTime::now(), |temp, live| {
            assert!(temp.is_file());
            assert!(!live.exists());
            assert!(originals.iter().all(|path| !path.exists()));
            Err(io::Error::other("forced install failure"))
        });
        assert!(
            matches!(result, Err(RestoreError::Untouched { reason }) if reason.contains("forced install failure"))
        );
        assert_eq!(
            originals.each_ref().map(|path| fs::read(path).unwrap()),
            bytes
        );
        assert_eq!(fs::read_dir(dir.path().join("backups")).unwrap().count(), 2);
    }

    #[test]
    fn a_blocked_rollback_keeps_the_originals_in_the_backup() {
        let dir = tempfile::tempdir().unwrap();
        let plan = fixture(dir.path());
        let originals = originals(dir.path());
        let bytes = originals.each_ref().map(|path| fs::read(path).unwrap());
        let result = restore_with(dir.path(), &plan, SystemTime::now(), |_, live| {
            fs::write(live, b"conflicting file")?;
            Err(io::Error::other("forced install failure"))
        });
        let Err(RestoreError::Moved { reason, broken_dir }) = result else {
            panic!("expected a rollback failure, got {result:?}");
        };
        assert!(reason.contains("forced install failure"));
        for (original, bytes) in originals.iter().zip(bytes) {
            let saved = broken_dir.join(original.file_name().unwrap());
            assert_eq!(fs::read(saved).unwrap(), bytes);
        }
        assert_eq!(fs::read(&originals[0]).unwrap(), b"conflicting file");
    }

    #[test]
    fn plan_requires_a_snapshot_and_a_recognized_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            restore_plan(
                dir.path(),
                &JournalError::Corrupt {
                    path: dir.path().join(DB_FILE),
                    code: 26,
                    latest_snapshot: None,
                    observed: observe_journal(dir.path()).ok(),
                }
            )
            .is_none()
        );
        let plan = fixture(dir.path());
        let interrupted = inspect(dir.path()).unwrap();
        fs::write(marker_path(dir.path()), b"invalid JSON").unwrap();
        let invalid = inspect(dir.path()).unwrap();
        assert!(matches!(
            invalid,
            JournalError::InvalidMigrationMarker { .. }
        ));
        assert!(restore_plan(dir.path(), &invalid).is_none());
        fs::remove_file(&plan.snapshot).unwrap();
        assert!(restore_plan(dir.path(), &interrupted).is_none());
    }

    #[test]
    fn corruption_uses_the_newest_valid_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let valid = fixture(dir.path());
        let newer = valid
            .snapshot
            .with_file_name("hub-99999999999999999999.sqlite3");
        fs::write(&newer, b"invalid SQLite data").unwrap();
        let error = JournalError::Corrupt {
            path: dir.path().join(DB_FILE),
            code: 26,
            latest_snapshot: Some((newer, SystemTime::now())),
            observed: observe_journal(dir.path()).ok(),
        };
        assert_eq!(
            restore_plan(dir.path(), &error).unwrap().snapshot,
            valid.snapshot
        );
    }
}
