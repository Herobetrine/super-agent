//! Single-file checkpoint restore. A preview binds the current bytes; an
//! exclusive Windows file handle is rechecked before mutation. Every mutation
//! has a flushed preimage and intent outside the repository. Git is never run
//! with a write command, and its index is never touched.

use super::*;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};

#[derive(Clone, Debug)]
pub(super) struct Plan {
    root: PathBuf,
    store: PathBuf,
    pub path: String,
    pub action: &'static str,
    pub preview: String,
    pub complete: bool,
    expected: Entry,
    target: Option<Entry>,
    bytes: Vec<u8>,
    captured: String,
}

#[derive(Debug)]
pub(super) struct Receipt {
    pub path: PathBuf,
}

pub(super) fn unfinished(store: &Path) -> Result<Vec<PathBuf>> {
    let mut pending = vec![];
    let entries = match fs::read_dir(store) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(pending),
        Err(e) => return Err(e.into()),
    };
    for entry in entries.take(10_000) {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with("restore-") {
            continue;
        }
        let path = entry.path();
        inspect_ancestors(&path)?;
        if !path.join("intent.json").exists() {
            continue;
        }
        let result = read_bounded(&path.join("result.json"), 16 * 1024)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        if !result.as_ref().is_some_and(|value| {
            matches!(value["state"].as_str(), Some("committed" | "rolled_back"))
        }) {
            pending.push(path);
        }
    }
    Ok(pending)
}

fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

/// Refuse links in every existing ancestor, including junctions on Windows.
fn inspect_ancestors(path: &Path) -> Result<()> {
    for ancestor in path.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) => ensure!(
                !is_link(&meta),
                "恢复路径包含链接或重解析点：{}",
                ancestor.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn safe_relative(path: &str) -> Result<()> {
    validate_path(path)?;
    // Win32 aliases must not reach .git or an alternate stream/device.
    ensure!(
        path.split('/').all(|part| {
            let upper = part.split('.').next().unwrap_or("").to_ascii_uppercase();
            !part.ends_with(['.', ' '])
                && !part.contains([':', '\\'])
                && !matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                && !(upper.len() == 4
                    && (upper.starts_with("COM") || upper.starts_with("LPT"))
                    && upper.as_bytes()[3].is_ascii_digit())
        }),
        "恢复路径包含 Windows 保留名称"
    );
    Ok(())
}

pub(super) fn prepare(root: &Path, point: &Checkpoint, change: &Change) -> Result<Plan> {
    ensure!(
        change.area == Area::Checkpoint,
        "只能恢复检查点比较中的文件"
    );
    safe_relative(&change.path)?;
    inspect_ancestors(&root.join(&change.path))?;
    let (expected, current) = read_file(root, &change.path)?;
    ensure!(
        matches!(expected.kind.as_str(), "file" | "missing"),
        "暂不恢复链接或特殊文件"
    );
    let target = point
        .files
        .get(&change.path)
        .filter(|e| e.kind != "missing")
        .cloned();
    ensure!(
        target.as_ref().is_none_or(|e| e.kind == "file"),
        "暂不恢复链接检查点"
    );
    let before = target.as_ref();
    let after = (expected.kind != "missing").then_some(&expected);
    ensure!(
        digest(format!("{before:?}\0{after:?}").as_bytes()) == change.fingerprint,
        "内容已变化，请刷新后重新预览"
    );
    let bytes = if let Some(target) = &target {
        let object = point.store.join("objects").join(&target.hash);
        inspect_ancestors(&object)?;
        let bytes = read_bounded(&object, FILE_CAP)?;
        ensure!(
            bytes.len() == target.size && digest(&bytes) == target.hash,
            "检查点内容摘要不匹配"
        );
        bytes
    } else {
        vec![]
    };
    let (preview, complete) = bytes_diff(&current, &bytes)?;
    let action = match (&target, expected.kind.as_str()) {
        (None, _) => "删除检查点之后新增的文件",
        (Some(_), "missing") => "重新创建已删除的文件",
        _ => "覆盖为检查点内容",
    };
    Ok(Plan {
        root: root.to_path_buf(),
        store: point.store.clone(),
        path: change.path.clone(),
        action,
        preview,
        complete,
        expected,
        target,
        bytes,
        captured: point.captured.clone(),
    })
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn record(dir: &Path, name: &str, data: Value) -> Result<()> {
    write_new(&dir.join(name), &serde_json::to_vec_pretty(&data)?)
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::c_void;
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};

    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    #[repr(C)]
    struct FileInfo {
        attrs: u32,
        times: [u32; 6],
        volume: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(handle: *mut c_void, info: *mut FileInfo) -> i32;
        fn SetFileInformationByHandle(
            handle: *mut c_void,
            class: i32,
            info: *const c_void,
            size: u32,
        ) -> i32;
    }

    pub fn directory(path: &Path) -> Result<File> {
        // Denying FILE_SHARE_DELETE pins the directory identity while allowing
        // ordinary file reads/writes inside it. OPEN_REPARSE_POINT never follows
        // a junction that appeared between inspection and open.
        let file = OpenOptions::new()
            .read(true)
            .share_mode(3)
            .custom_flags(OPEN_REPARSE_POINT | BACKUP_SEMANTICS)
            .open(path)?;
        let meta = file.metadata()?;
        ensure!(meta.is_dir() && !is_link(&meta), "恢复父目录不是普通目录");
        Ok(file)
    }

    pub fn file(path: &Path, create: bool) -> Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(create)
            .access_mode(0xC001_0000) // GENERIC_READ | GENERIC_WRITE | DELETE
            .share_mode(0)
            .custom_flags(OPEN_REPARSE_POINT)
            .open(path)?;
        // CREATE_NEW plus the pinned parent guarantees a newly created regular
        // file. Return its ownership immediately so every later failure can
        // remove it through this handle and is recorded by the transaction.
        if create {
            return Ok(file);
        }
        let mut info = FileInfo {
            attrs: 0,
            times: [0; 6],
            volume: 0,
            size_high: 0,
            size_low: 0,
            links: 0,
            index_high: 0,
            index_low: 0,
        };
        // SAFETY: file owns a valid kernel handle and info has the Win32 layout.
        ensure!(
            unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0,
            "无法检查恢复文件身份：{}",
            std::io::Error::last_os_error()
        );
        ensure!(
            info.attrs & (0x400 | 0x10) == 0 && info.links == 1,
            "暂不恢复链接、重解析点、目录或硬链接文件"
        );
        Ok(file)
    }

    pub fn delete(file: &File) -> Result<()> {
        let delete: u8 = 1;
        // SAFETY: FileDispositionInfo (4) takes a FILE_DISPOSITION_INFO BOOLEAN.
        ensure!(
            unsafe {
                SetFileInformationByHandle(
                    file.as_raw_handle(),
                    4,
                    (&delete as *const u8).cast(),
                    std::mem::size_of::<u8>() as u32,
                )
            } != 0,
            "无法删除恢复文件：{}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }
}

fn pinned_directories(path: &Path) -> Result<Vec<File>> {
    #[cfg(windows)]
    {
        let mut ancestors: Vec<_> = path
            .ancestors()
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        ancestors.reverse();
        ancestors.into_iter().map(platform::directory).collect()
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        bail!("检查点恢复目前仅支持已验证的 Windows 文件句柄实现")
    }
}

fn file_bytes(file: &mut File) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = vec![];
    file.take(FILE_CAP as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= FILE_CAP, "当前文件超过恢复上限");
    Ok(bytes)
}

fn overwrite(file: &mut File, bytes: &[u8]) -> Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.write_all(bytes)?;
    file.set_len(bytes.len() as u64)?;
    file.sync_all()?;
    ensure!(file_bytes(file)? == bytes, "恢复后内容校验失败");
    Ok(())
}

pub(super) fn apply(plan: Plan) -> Result<Receipt> {
    apply_with(plan, || Ok(()))
}

/// The injected check exercises failure after mutation without global state.
fn apply_with(plan: Plan, after_write: impl FnOnce() -> Result<()>) -> Result<Receipt> {
    #[cfg(not(windows))]
    {
        let _ = (plan, after_write);
        bail!("检查点恢复目前仅支持 Windows");
    }
    #[cfg(windows)]
    {
        safe_relative(&plan.path)?;
        ensure!(
            plan.root.is_absolute() && plan.store.is_absolute(),
            "恢复目录必须为绝对路径"
        );
        ensure!(
            !plan.store.starts_with(&plan.root),
            "恢复备份必须位于项目外部"
        );
        if let Some(target) = &plan.target {
            ensure!(
                target.kind == "file"
                    && target.size == plan.bytes.len()
                    && target.hash == digest(&plan.bytes),
                "恢复对象内容损坏"
            );
        }
        let mut guards = pinned_directories(&plan.root)?;
        guards.extend(pinned_directories(&plan.store)?);
        let full = plan.root.join(&plan.path);
        inspect_ancestors(&full)?;
        // Pin existing descendants before any mutation. Missing directories
        // are created individually only after the durable intent is written.
        let mut parent = plan.root.clone();
        let parts: Vec<_> = Path::new(&plan.path).components().collect();
        let mut missing_dirs = vec![];
        for component in &parts[..parts.len() - 1] {
            parent.push(component.as_os_str());
            match fs::symlink_metadata(&parent) {
                Ok(_) => guards.push(platform::directory(&parent)?),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    missing_dirs.push(parent.clone())
                }
                Err(e) => return Err(e.into()),
            }
        }
        let mut opened = if plan.expected.kind == "file" {
            Some(platform::file(&full, false)?)
        } else {
            None
        };
        let before = if let Some(file) = &mut opened {
            file_bytes(file)?
        } else {
            ensure!(
                matches!(fs::symlink_metadata(&full), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
                "预览后文件已被创建，请重新预览"
            );
            vec![]
        };
        ensure!(
            plan.expected.size == before.len() && plan.expected.hash == digest(&before),
            "预览后内容已变化，恢复已取消"
        );
        let dir = tempfile::Builder::new()
            .prefix("restore-")
            .tempdir_in(&plan.store)?
            .keep();
        guards.push(platform::directory(&dir)?);
        write_new(&dir.join("before.bin"), &before)?;
        write_new(&dir.join("target.bin"), &plan.bytes)?;
        record(
            &dir,
            "intent.json",
            json!({"version":1,"state":"prepared","root":plan.root,
            "path":plan.path,"captured":plan.captured,"before":plan.expected.value(),
            "target":plan.target.as_ref().map(Entry::value),"created_directories":missing_dirs,
            "started":chrono::Utc::now().to_rfc3339()}),
        )?;
        let mut created_dirs = vec![];
        let mut created_file = false;
        let mut mutated = false;
        let result = (|| -> Result<()> {
            for path in &missing_dirs {
                fs::create_dir(path)?;
                created_dirs.push(path.clone());
                guards.push(platform::directory(path)?);
            }
            if opened.is_none() {
                // create_new rejects an editor's file created after the preview.
                opened = Some(platform::file(&full, true)?);
                created_file = true;
            }
            let file = opened.as_mut().ok_or_else(|| anyhow!("未打开恢复文件"))?;
            if plan.target.is_some() {
                mutated = true;
                overwrite(file, &plan.bytes)?;
                after_write()?;
            } else {
                after_write()?;
                platform::delete(file)?;
                mutated = true;
            }
            Ok(())
        })();
        if let Err(error) = result {
            let rollback = if let Some(file) = &mut opened {
                if created_file {
                    platform::delete(file)
                } else if mutated {
                    overwrite(file, &before)
                } else {
                    Ok(())
                }
            } else {
                Ok(())
            };
            drop(opened);
            drop(guards);
            // Only empty directories created by this operation are removed.
            let mut cleanup_errors = vec![];
            for path in created_dirs.iter().rev() {
                if let Err(e) = fs::remove_dir(path) {
                    cleanup_errors.push(e.to_string());
                }
            }
            let state = if rollback.is_ok() && cleanup_errors.is_empty() {
                "rolled_back"
            } else {
                "recovery_required"
            };
            let saved = record(
                &dir,
                "result.json",
                json!({"state":state,"error":error.to_string(),
                "rollback_error":rollback.err().map(|e|e.to_string()),"cleanup_errors":cleanup_errors}),
            );
            bail!(
                "恢复失败（{state}）：{error:#}。备份与回执：{}{}",
                dir.display(),
                saved
                    .err()
                    .map(|e| format!("；结果回执写入失败：{e}"))
                    .unwrap_or_default()
            );
        }
        drop(opened);
        record(
            &dir,
            "result.json",
            json!({"state":"committed","completed":chrono::Utc::now().to_rfc3339()}),
        )
        .with_context(|| {
            format!(
                "文件已恢复，结果回执写入失败；原始备份与 intent 保留于 {}",
                dir.display()
            )
        })?;
        Ok(Receipt { path: dir })
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(root: &Path, args: &[&str]) -> Vec<u8> {
        let exe = sa_adapters::sys::Sys::from_process()
            .which("git.exe")
            .unwrap();
        let out = Command::new(exe)
            .current_dir(root)
            .args([
                "-c",
                "user.name=Restore Test",
                "-c",
                "user.email=restore@example.invalid",
                "-c",
                "core.autocrlf=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    fn fixture() -> (tempfile::TempDir, tempfile::TempDir, PathBuf, Checkpoint) {
        let repo = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q"]);
        fs::write(repo.path().join("file.txt"), b"checkpoint\n").unwrap();
        fs::create_dir(repo.path().join("nested")).unwrap();
        fs::write(repo.path().join("nested/deleted.txt"), b"return me\n").unwrap();
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-qm", "baseline"]);
        let root = repository(repo.path()).unwrap();
        let checkpoint = save_checkpoint(data.path(), &root).unwrap();
        (repo, data, root, checkpoint)
    }

    fn plan(root: &Path, checkpoint: &Checkpoint, path: &str) -> Plan {
        let changes = checkpoint_changes(root, checkpoint).unwrap();
        prepare(
            root,
            checkpoint,
            changes.iter().find(|c| c.path == path).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn restore_overwrite_has_preimage_receipt_and_preserves_index_and_other_edits() {
        let (_repo, _data, root, point) = fixture();
        fs::write(root.join("file.txt"), b"staged\n").unwrap();
        git(&root, &["add", "file.txt"]);
        fs::write(root.join("file.txt"), b"current\n").unwrap();
        fs::write(root.join("other.txt"), b"user edit\n").unwrap();
        let index = fs::read(root.join(".git/index")).unwrap();
        let head = git(&root, &["rev-parse", "HEAD"]);
        let restore = plan(&root, &point, "file.txt");
        assert!(restore.preview.contains("-current") && restore.preview.contains("+checkpoint"));
        let receipt = apply(restore).unwrap();
        assert_eq!(fs::read(root.join("file.txt")).unwrap(), b"checkpoint\n");
        assert_eq!(fs::read(root.join("other.txt")).unwrap(), b"user edit\n");
        assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
        assert_eq!(git(&root, &["rev-parse", "HEAD"]), head);
        assert_eq!(
            fs::read(receipt.path.join("before.bin")).unwrap(),
            b"current\n"
        );
        let result: Value =
            serde_json::from_slice(&fs::read(receipt.path.join("result.json")).unwrap()).unwrap();
        assert_eq!(result["state"], "committed");
    }

    #[test]
    fn restore_preview_rejects_later_edit_or_later_creation() {
        let (_repo, _data, root, point) = fixture();
        fs::write(root.join("file.txt"), b"previewed\n").unwrap();
        let previewed = plan(&root, &point, "file.txt");
        fs::write(root.join("file.txt"), b"new user edit\n").unwrap();
        assert!(
            apply(previewed)
                .unwrap_err()
                .to_string()
                .contains("内容已变化")
        );
        assert_eq!(fs::read(root.join("file.txt")).unwrap(), b"new user edit\n");
        fs::remove_file(root.join("file.txt")).unwrap();
        let deleted = plan(&root, &point, "file.txt");
        fs::write(root.join("file.txt"), b"recreated\n").unwrap();
        assert!(apply(deleted).unwrap_err().to_string().contains("已被创建"));
        assert_eq!(fs::read(root.join("file.txt")).unwrap(), b"recreated\n");
    }

    #[test]
    fn restore_deletes_added_file_and_recreates_deleted_nested_file() {
        let (_repo, _data, root, point) = fixture();
        fs::write(root.join("added.txt"), b"added after checkpoint\n").unwrap();
        let receipt = apply(plan(&root, &point, "added.txt")).unwrap();
        assert!(!root.join("added.txt").exists());
        assert_eq!(
            fs::read(receipt.path.join("before.bin")).unwrap(),
            b"added after checkpoint\n"
        );
        fs::remove_file(root.join("nested/deleted.txt")).unwrap();
        fs::remove_dir(root.join("nested")).unwrap();
        apply(plan(&root, &point, "nested/deleted.txt")).unwrap();
        assert_eq!(
            fs::read(root.join("nested/deleted.txt")).unwrap(),
            b"return me\n"
        );
    }

    #[test]
    fn restore_failure_rolls_back_and_records_it_without_touching_index() {
        let (_repo, _data, root, point) = fixture();
        fs::write(root.join("file.txt"), b"before failed restore\n").unwrap();
        let index = fs::read(root.join(".git/index")).unwrap();
        let err = apply_with(plan(&root, &point, "file.txt"), || {
            bail!("injected failure after write")
        })
        .unwrap_err();
        assert!(err.to_string().contains("rolled_back"), "{err:#}");
        assert_eq!(
            fs::read(root.join("file.txt")).unwrap(),
            b"before failed restore\n"
        );
        assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
        let dir = fs::read_dir(&point.store)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("restore-")
            })
            .unwrap();
        let result: Value =
            serde_json::from_slice(&fs::read(dir.join("result.json")).unwrap()).unwrap();
        assert_eq!(result["state"], "rolled_back");
        assert_eq!(
            fs::read(dir.join("before.bin")).unwrap(),
            b"before failed restore\n"
        );
    }

    #[test]
    fn restore_rejects_tampering_reserved_paths_hardlinks_and_locked_files() {
        use std::os::windows::fs::OpenOptionsExt;
        let (_repo, _data, root, point) = fixture();
        fs::write(root.join("file.txt"), b"current\n").unwrap();
        let prepared = plan(&root, &point, "file.txt");
        let locked = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(root.join("file.txt"))
            .unwrap();
        assert!(apply(prepared).is_err());
        drop(locked);
        let prepared = plan(&root, &point, "file.txt");
        let outside = tempfile::tempdir().unwrap();
        fs::hard_link(root.join("file.txt"), outside.path().join("alias.txt")).unwrap();
        assert!(apply(prepared).unwrap_err().to_string().contains("硬链接"));
        assert_eq!(
            fs::read(outside.path().join("alias.txt")).unwrap(),
            b"current\n"
        );
        fs::remove_file(outside.path().join("alias.txt")).unwrap();
        for invalid in [
            "../escape",
            ".git/config",
            ".git /config",
            "file.txt:stream",
            "CON.txt",
            "a/../b",
        ] {
            assert!(safe_relative(invalid).is_err(), "{invalid}");
        }
        fs::write(
            point
                .store
                .join("objects")
                .join(&point.files["file.txt"].hash),
            b"tampered",
        )
        .unwrap();
        let change = checkpoint_changes(&root, &point)
            .unwrap()
            .into_iter()
            .find(|c| c.path == "file.txt")
            .unwrap();
        assert!(
            prepare(&root, &point, &change)
                .unwrap_err()
                .to_string()
                .contains("摘要")
        );
        assert_eq!(fs::read(root.join("file.txt")).unwrap(), b"current\n");
    }

    #[test]
    fn restore_handle_prevents_concurrent_write_and_parent_rename() {
        let (_repo, _data, root, point) = fixture();
        fs::write(root.join("nested/deleted.txt"), b"current\n").unwrap();
        let file = root.join("nested/deleted.txt");
        let parent = root.join("nested");
        apply_with(plan(&root, &point, "nested/deleted.txt"), || {
            assert!(fs::write(&file, b"racing writer").is_err());
            assert!(fs::rename(&parent, root.join("moved")).is_err());
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::read(file).unwrap(), b"return me\n");
    }

    #[test]
    fn restore_failed_creation_removes_only_its_new_file_and_empty_parent() {
        let (_repo, _data, root, point) = fixture();
        fs::remove_file(root.join("nested/deleted.txt")).unwrap();
        fs::remove_dir(root.join("nested")).unwrap();
        let error = apply_with(plan(&root, &point, "nested/deleted.txt"), || {
            bail!("injected new-file failure")
        })
        .unwrap_err();
        assert!(error.to_string().contains("rolled_back"), "{error:#}");
        assert!(!root.join("nested").exists());
        assert!(unfinished(&point.store).unwrap().is_empty());
    }

    #[test]
    fn restore_restart_surfaces_unfinished_or_damaged_receipts() {
        let (_repo, _data, _root, point) = fixture();
        let dir = point.store.join("restore-interrupted-fixture");
        fs::create_dir(&dir).unwrap();
        record(&dir, "intent.json", json!({"state":"prepared"})).unwrap();
        assert_eq!(
            unfinished(&point.store).unwrap().as_slice(),
            std::slice::from_ref(&dir)
        );
        fs::write(dir.join("result.json"), b"partial JSON").unwrap();
        assert_eq!(
            unfinished(&point.store).unwrap().as_slice(),
            std::slice::from_ref(&dir)
        );
        fs::write(dir.join("result.json"), br#"{"state":"recovery_required"}"#).unwrap();
        assert_eq!(
            unfinished(&point.store).unwrap().as_slice(),
            std::slice::from_ref(&dir)
        );
        fs::write(dir.join("result.json"), br#"{"state":"committed"}"#).unwrap();
        assert!(unfinished(&point.store).unwrap().is_empty());
    }

    #[test]
    fn restore_rejects_parent_replaced_by_junction() {
        let (_repo, _data, root, point) = fixture();
        fs::write(root.join("nested/deleted.txt"), b"current\n").unwrap();
        let prepared = plan(&root, &point, "nested/deleted.txt");
        fs::remove_file(root.join("nested/deleted.txt")).unwrap();
        fs::remove_dir(root.join("nested")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("deleted.txt"), b"outside protected\n").unwrap();
        let link = root.join("nested");
        // mklink /J creates an unprivileged directory junction; both inputs
        // are generated temporary paths. Cleanup uses native RemoveDirectory.
        let out = Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "junction fixture: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let err = apply(prepared).unwrap_err();
        assert!(err.to_string().contains("重解析点"), "{err:#}");
        assert_eq!(
            fs::read(outside.path().join("deleted.txt")).unwrap(),
            b"outside protected\n"
        );
        fs::remove_dir(link).unwrap();
    }
}
