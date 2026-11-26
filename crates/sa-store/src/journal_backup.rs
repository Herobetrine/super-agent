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
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug)]
pub struct RestorePlan {
    pub snapshot: PathBuf,
    pub snapshot_time: SystemTime,
}

#[derive(Debug)]
pub struct Restored {
    pub broken_dir: PathBuf,
    pub snapshot_time: SystemTime,
}

#[derive(Debug)]
pub enum RestoreError {
    Busy,
    Untouched { reason: String },
    Moved { reason: String, broken_dir: PathBuf },
}

// The scheduler owns desktop.lock for its entire lifetime, including shutdown.
// Keep the name in sync with sa_engine::owner::LOCK_FILE (sa-engine depends on sa-store).
const RESTORE_LOCKS: [&str; 2] = [".journal-migration.lock", "desktop.lock"];

pub fn journal_busy(data_dir: &Path) -> bool {
    RESTORE_LOCKS
        .iter()
        .any(|name| match lock_file(&data_dir.join(name), false) {
            Ok(_lock) => false,
            Err(error) => error.kind() != io::ErrorKind::NotFound,
        })
}

fn lock_file(path: &Path, create: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .open(path)?;
    file.try_lock().map_err(|error| match error {
        TryLockError::WouldBlock => io::ErrorKind::WouldBlock.into(),
        TryLockError::Error(error) => error,
    })?;
    Ok(file)
}

pub fn restore_plan(data_dir: &Path, error: &JournalError) -> Option<RestorePlan> {
    let (snapshot, snapshot_time) = match error {
        JournalError::MigrationInterrupted { snapshot, .. } => {
            let metadata = fs::symlink_metadata(snapshot).ok()?;
            if !metadata.is_file() {
                return None;
            }
            (snapshot.clone(), metadata.modified().ok()?)
        }
        JournalError::Corrupt { .. } => snapshots(data_dir)
            .ok()?
            .into_iter()
            .find(|(path, _)| quick_check(path).is_ok())?,
        JournalError::InvalidMigrationMarker { .. } => return None,
    };
    Some(RestorePlan {
        snapshot,
        snapshot_time,
    })
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

fn restore_with(
    data_dir: &Path,
    plan: &RestorePlan,
    confirmed_at: SystemTime,
    install: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> Result<Restored, RestoreError> {
    let [migration, owner] = RESTORE_LOCKS.map(|name| data_dir.join(name));
    let claim = |path: &Path| {
        lock_file(path, true).map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                RestoreError::Busy
            } else {
                untouched(format!("无法锁定 {}：{error}", path.display()))
            }
        })
    };
    let _migration_lock = claim(&migration)?;
    let _owner_lock = claim(&owner)?;

    let mut temp = tempfile::Builder::new()
        .prefix(".journal-restore-")
        .tempfile_in(data_dir)
        .map_err(untouched)?;
    let mut snapshot = File::open(&plan.snapshot)
        .with_context(|| format!("无法打开快照 {}", plan.snapshot.display()))
        .map_err(|error| untouched(format!("{error:#}")))?;
    io::copy(&mut snapshot, temp.as_file_mut()).map_err(untouched)?;
    temp.as_file().sync_all().map_err(untouched)?;
    // Close the writable handle before validating and renaming on Windows.
    let temp = temp.into_temp_path();
    quick_check(&temp).map_err(untouched)?;

    let backups = data_dir.join("backups");
    let stamp = restore_backup_stamp(confirmed_at).map_err(untouched)?;
    let broken_dir = backups.join(format!("broken-{stamp}"));
    fs::create_dir(&broken_dir).map_err(|error| {
        untouched(format!(
            "无法创建备份目录 {}：{error}",
            broken_dir.display()
        ))
    })?;
    let live = data_dir.join(DB_FILE);
    let files = [
        live.clone(),
        with_suffix(&live, "-wal"),
        with_suffix(&live, "-shm"),
        with_suffix(&live, "-journal"),
        marker_path(data_dir),
    ];
    let mut moved = Vec::new();
    let staged = files.iter().try_for_each(|path| -> Result<()> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => ensure!(!metadata.is_dir(), "{} 是目录", path.display()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        let target = broken_dir.join(path.file_name().context("缺少文件名")?);
        rename_restore_file(path, &target)
            .with_context(|| format!("无法移走 {}", path.display()))?;
        moved.push((path.as_path(), target));
        Ok(())
    });
    let published = staged.and_then(|()| {
        sync_directory(&broken_dir)?;
        sync_directory(&backups)?;
        sync_directory(data_dir)?;
        install(&temp, &live).context("无法安装快照")
    });
    if let Err(error) = published {
        return Err(rollback_restore(&moved, broken_dir, error));
    }
    let reason = |error: anyhow::Error| RestoreError::Moved {
        reason: format!("{error:#}"),
        broken_dir: broken_dir.clone(),
    };
    let record = RestoreRecord {
        snapshot: &plan.snapshot,
        snapshot_time: plan.snapshot_time,
        files_moved: moved.iter().map(|(_, saved)| saved.as_path()).collect(),
        restored_at: SystemTime::now(),
    };
    sync_directory(data_dir)
        .context("无法同步恢复后的工单库目录")
        .map_err(reason)?;
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
        snapshot_time: plan.snapshot_time,
    })
}

fn rollback_restore(
    moved: &[(&Path, PathBuf)],
    broken_dir: PathBuf,
    error: anyhow::Error,
) -> RestoreError {
    let mut returned = Vec::new();
    for (original, saved) in moved.iter().rev() {
        if let Err(move_error) = rename_restore_file(saved, original) {
            let mut failures = vec![format!("无法将 {} 移回：{move_error}", original.display())];
            // Keep the originals together if only part of the rollback worked.
            for (original, saved) in returned {
                if let Err(error) = rename_restore_file(original, saved) {
                    failures.push(format!("{} 已移回数据文件夹：{error}", original.display()));
                }
            }
            return RestoreError::Moved {
                reason: format!("{error:#}；{}", failures.join("；")),
                broken_dir,
            };
        }
        returned.push((*original, saved.as_path()));
    }
    let _ = fs::remove_dir(&broken_dir);
    untouched(format!("{error:#}"))
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
    let path = marker_path(dir);
    let read = || -> Result<Option<JournalError>> {
        let Some(contents) = read_optional(&path)? else {
            return Ok(None);
        };
        let marker: Marker = serde_json::from_slice(&contents)?;
        let name = local_name(&marker.snapshot)?;
        Ok(Some(JournalError::MigrationInterrupted {
            snapshot: dir.join("backups").join(name),
            from: marker.from,
            to: marker.to,
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
        let plan = fixture(dir.path());
        fs::write(&plan.snapshot, b"invalid SQLite data").unwrap();
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
    fn both_exclusive_locks_prevent_restore_and_clear_when_released() {
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
                    latest_snapshot: None
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
        };
        assert_eq!(
            restore_plan(dir.path(), &error).unwrap().snapshot,
            valid.snapshot
        );
    }
}
