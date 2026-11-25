use super::JournalError;
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
