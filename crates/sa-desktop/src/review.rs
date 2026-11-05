//! Per-file Git review and immutable worktree checkpoints. Git is invoked
//! without a shell, with literal pathspecs and NUL-delimited machine output.
//! Checkpoints live under the app data directory, outside the repository.

use crate::ops::{Pending, git_bytes};
use crate::theme::{self, Palette};
use crate::widgets;
use anyhow::{Context, Result, anyhow, bail, ensure};
use egui::{Label, RichText, Ui};
use egui_phosphor::regular as icon;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

const FILE_CAP: usize = 4 * 1024 * 1024;
const TOTAL_CAP: usize = 64 * 1024 * 1024;
const LIST_CAP: usize = 4 * 1024 * 1024;
const PREVIEW_CAP: usize = 256 * 1024;
const FILE_COUNT: usize = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Area {
    Staged,
    Worktree,
    Untracked,
    Checkpoint,
}

impl Area {
    fn label(self) -> &'static str {
        match self {
            Self::Staged => "已暂存",
            Self::Worktree => "未暂存",
            Self::Untracked => "未跟踪",
            Self::Checkpoint => "检查点之后",
        }
    }
}

#[derive(Clone, Debug)]
struct Change {
    path: String,
    old_path: Option<String>,
    area: Area,
    status: String,
    fingerprint: String,
    problem: Option<String>,
}

impl Change {
    fn key(&self) -> String {
        format!("{:?}\0{}", self.area, self.path)
    }
    fn label(&self) -> String {
        let path = display_path(&self.path);
        match &self.old_path {
            Some(old) => format!("{} → {path}", display_path(old)),
            None => path,
        }
    }

    fn status_label(&self) -> &'static str {
        if matches!(
            self.status.as_str(),
            "DD" | "AU" | "UD" | "UA" | "DU" | "AA" | "UU"
        ) {
            return "冲突";
        }
        let code = match self.area {
            Area::Staged | Area::Checkpoint => self.status.chars().next(),
            Area::Worktree => self.status.chars().last(),
            Area::Untracked => Some('A'),
        };
        match code {
            Some('A') => "新增",
            Some('M') => "修改",
            Some('D') => "删除",
            Some('R') => "重命名",
            Some('C') => "复制",
            Some('T') => "类型变化",
            Some('U') => "冲突",
            _ => "变更",
        }
    }
}

/// Escape control characters for display only. The original path is used for
/// every filesystem and Git operation.
fn display_path(path: &str) -> String {
    path.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn display_time(captured: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(captured)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| captured.to_string())
}

fn git_full(root: &Path, args: &[&str], cap: usize) -> Result<Vec<u8>> {
    let result = git_bytes(root, args, cap, false)?;
    ensure!(
        !result.truncated,
        "Git 文件清单超过读取上限，未加载不完整结果"
    );
    Ok(result.bytes)
}

fn repository(cwd: &Path) -> Result<PathBuf> {
    let bytes = git_full(
        cwd,
        &["rev-parse", "--path-format=absolute", "--show-toplevel"],
        LIST_CAP,
    )?;
    let text = std::str::from_utf8(&bytes).context("仓库路径不是 UTF-8")?;
    let text = text.strip_suffix('\n').unwrap_or(text);
    let text = if cfg!(windows) {
        text.strip_suffix('\r').unwrap_or(text)
    } else {
        text
    };
    fs::canonicalize(text).context("无法解析 Git 仓库目录")
}

fn validate_path(path: &str) -> Result<()> {
    ensure!(!path.is_empty() && !path.contains('\0'), "无效文件路径");
    ensure!(
        Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_))),
        "文件路径越出项目目录"
    );
    ensure!(
        path.split('/')
            .all(|p| !p.is_empty() && p != "." && p != ".." && !p.eq_ignore_ascii_case(".git")),
        "无效项目文件路径"
    );
    #[cfg(windows)]
    ensure!(!path.contains(['\\', ':']), "无效 Windows 项目文件路径");
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    kind: String,
    hash: String,
    size: usize,
    executable: bool,
}

impl Entry {
    fn fingerprint(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.kind, self.hash, self.size, self.executable
        )
    }
    fn value(&self) -> Value {
        json!({"kind": self.kind, "sha256": self.hash, "size": self.size, "executable": self.executable})
    }
}

fn read_file(root: &Path, path: &str) -> Result<(Entry, Vec<u8>)> {
    validate_path(path)?;
    let full = root.join(path);
    // Check the resolved parent before opening a regular file. A leaf symlink
    // is captured as its target text and is never followed.
    let mut parent = full.parent().ok_or_else(|| anyhow!("无效父目录"))?;
    while !parent.exists() {
        parent = parent.parent().ok_or_else(|| anyhow!("无法解析父目录"))?;
    }
    ensure!(
        fs::canonicalize(parent)?.starts_with(root),
        "文件父目录链接指向项目外部"
    );
    let meta = match fs::symlink_metadata(&full) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((
                Entry {
                    kind: "missing".into(),
                    hash: digest(&[]),
                    size: 0,
                    executable: false,
                },
                Vec::new(),
            ));
        }
        Err(e) => return Err(e.into()),
    };
    let (kind, bytes) = if meta.file_type().is_symlink() {
        let target = fs::read_link(&full)?;
        let target = target
            .to_str()
            .ok_or_else(|| anyhow!("链接目标不是 UTF-8"))?;
        ("symlink", target.as_bytes().to_vec())
    } else {
        ensure!(meta.is_file(), "目录、子模块或特殊文件无法捕获为文件检查点");
        ensure!(
            meta.len() <= FILE_CAP as u64,
            "文件超过 4 MiB，未读取完整内容"
        );
        ensure!(
            fs::canonicalize(&full)?.starts_with(root),
            "文件链接指向项目外部"
        );
        let mut bytes = Vec::new();
        fs::File::open(&full)?
            .take(FILE_CAP as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= FILE_CAP, "文件读取期间超过 4 MiB");
        let after = fs::symlink_metadata(&full)?;
        ensure!(
            after.is_file()
                && !after.file_type().is_symlink()
                && fs::canonicalize(&full)?.starts_with(root),
            "读取期间文件路径发生变化"
        );
        ensure!(
            meta.len() == after.len() && meta.modified()? == after.modified()?,
            "读取期间文件内容发生变化"
        );
        ("file", bytes)
    };
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = false;
    let entry = Entry {
        kind: kind.into(),
        hash: digest(&bytes),
        size: bytes.len(),
        executable,
    };
    Ok((entry, bytes))
}

fn parse_status(bytes: &[u8]) -> Result<Vec<Change>> {
    ensure!(
        bytes.is_empty() || bytes.last() == Some(&0),
        "Git 状态缺少 NUL 结尾"
    );
    let text = std::str::from_utf8(bytes).context("Git 文件名不是 UTF-8，无法安全显示")?;
    let mut records = text.split_terminator('\0');
    let mut changes = Vec::new();
    while let Some(record) = records.next() {
        let first = record
            .as_bytes()
            .first()
            .copied()
            .ok_or_else(|| anyhow!("空 Git 状态记录"))?;
        if first == b'?' {
            let path = record
                .strip_prefix("? ")
                .ok_or_else(|| anyhow!("无效未跟踪状态"))?;
            validate_path(path)?;
            changes.push(Change {
                path: path.into(),
                old_path: None,
                area: Area::Untracked,
                status: "??".into(),
                fingerprint: digest(record.as_bytes()),
                problem: None,
            });
            continue;
        }
        let fields: Vec<_> = match first {
            b'1' => record.splitn(9, ' ').collect(),
            b'2' => record.splitn(10, ' ').collect(),
            b'u' => record.splitn(11, ' ').collect(),
            _ => bail!("未知 Git 状态格式"),
        };
        let count = match first {
            b'1' => 9,
            b'2' => 10,
            _ => 11,
        };
        ensure!(
            fields.len() == count && fields[1].len() == 2,
            "无效 Git 状态字段"
        );
        let path = fields[count - 1];
        validate_path(path)?;
        let old_path = if first == b'2' {
            let old = records
                .next()
                .ok_or_else(|| anyhow!("重命名记录缺少原路径"))?;
            validate_path(old)?;
            Some(old.to_string())
        } else {
            None
        };
        let mut stamp = record.as_bytes().to_vec();
        stamp.extend_from_slice(old_path.as_deref().unwrap_or_default().as_bytes());
        let template = Change {
            path: path.into(),
            old_path,
            area: Area::Worktree,
            status: fields[1].into(),
            fingerprint: digest(&stamp),
            problem: None,
        };
        if first == b'u' {
            changes.push(template);
        } else {
            if fields[1].as_bytes()[0] != b'.' {
                changes.push(Change {
                    area: Area::Staged,
                    ..template.clone()
                });
            }
            if fields[1].as_bytes()[1] != b'.' {
                changes.push(Change {
                    area: Area::Worktree,
                    ..template
                });
            }
        }
    }
    ensure!(changes.len() <= FILE_COUNT, "变更文件超过 10000 项");
    Ok(changes)
}

fn status(root: &Path) -> Result<Vec<Change>> {
    let bytes = git_full(
        root,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
            "--",
        ],
        LIST_CAP,
    )?;
    let mut changes = parse_status(&bytes)?;
    let mut total = 0usize;
    for change in &mut changes {
        if change.area == Area::Staged {
            continue;
        }
        let entry = if total >= TOTAL_CAP {
            Err(anyhow!("变更内容超过 64 MiB 读取上限"))
        } else {
            read_file(root, &change.path)
        };
        match entry {
            Ok((entry, _)) => {
                total += entry.size;
                change.fingerprint =
                    digest(format!("{}:{}", change.fingerprint, entry.fingerprint()).as_bytes());
            }
            Err(e) => change.problem = Some(format!("{e:#}")),
        }
    }
    Ok(changes)
}

fn all_paths(root: &Path) -> Result<Vec<String>> {
    let bytes = git_full(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
        ],
        LIST_CAP,
    )?;
    ensure!(
        bytes.is_empty() || bytes.last() == Some(&0),
        "Git 文件清单缺少 NUL 结尾"
    );
    let text = std::str::from_utf8(&bytes).context("Git 文件名不是 UTF-8")?;
    let paths: BTreeSet<String> = text.split_terminator('\0').map(str::to_owned).collect();
    ensure!(paths.len() <= FILE_COUNT, "检查点最多覆盖 10000 个文件");
    for path in &paths {
        validate_path(path)?;
    }
    Ok(paths.into_iter().collect())
}

fn store_dir(data: &Path, root: &Path) -> Result<PathBuf> {
    let data = fs::canonicalize(data).context("无法解析应用数据目录")?;
    ensure!(!data.starts_with(root), "检查点数据目录必须位于项目外部");
    let root_text = root.to_str().ok_or_else(|| anyhow!("项目路径不是 UTF-8"))?;
    Ok(data.join("review").join(digest(root_text.as_bytes())))
}

#[derive(Clone, Debug)]
struct Checkpoint {
    captured: String,
    files: BTreeMap<String, Entry>,
    store: PathBuf,
}

fn load_checkpoint(data: &Path, root: &Path) -> Result<Option<Checkpoint>> {
    let store = store_dir(data, root)?;
    let file = match fs::File::open(store.join("latest.json")) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut bytes = Vec::new();
    file.take(LIST_CAP as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= LIST_CAP, "检查点清单过大");
    let value: Value = serde_json::from_slice(&bytes).context("检查点清单损坏")?;
    ensure!(
        value["version"] == 1 && value["root"].as_str() == root.to_str(),
        "检查点版本或项目目录不匹配"
    );
    let captured = value["captured"]
        .as_str()
        .ok_or_else(|| anyhow!("检查点缺少时间"))?
        .to_string();
    let items = value["files"]
        .as_object()
        .ok_or_else(|| anyhow!("检查点缺少文件清单"))?;
    ensure!(items.len() <= FILE_COUNT, "检查点文件数过多");
    let mut files = BTreeMap::new();
    let mut total = 0usize;
    for (path, item) in items {
        validate_path(path)?;
        let kind = item["kind"]
            .as_str()
            .ok_or_else(|| anyhow!("检查点文件类型无效"))?;
        ensure!(
            matches!(kind, "file" | "symlink" | "missing"),
            "检查点文件类型无效"
        );
        let hash = item["sha256"]
            .as_str()
            .ok_or_else(|| anyhow!("检查点缺少摘要"))?;
        ensure!(
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
            "检查点摘要无效"
        );
        let size = item["size"]
            .as_u64()
            .filter(|n| *n <= FILE_CAP as u64)
            .ok_or_else(|| anyhow!("检查点文件大小无效"))? as usize;
        total += size;
        ensure!(total <= TOTAL_CAP, "检查点总量超过 64 MiB");
        files.insert(
            path.clone(),
            Entry {
                kind: kind.into(),
                hash: hash.into(),
                size,
                executable: item["executable"].as_bool().unwrap_or(false),
            },
        );
    }
    Ok(Some(Checkpoint {
        captured,
        files,
        store,
    }))
}

fn snapshot(root: &Path, store: Option<&Path>) -> Result<BTreeMap<String, Entry>> {
    let mut files = BTreeMap::new();
    let mut total = 0usize;
    for path in all_paths(root)? {
        let (entry, bytes) =
            read_file(root, &path).with_context(|| format!("无法捕获 {}", display_path(&path)))?;
        total += bytes.len();
        ensure!(
            total <= TOTAL_CAP,
            "检查点总内容超过 64 MiB，未保存不完整检查点"
        );
        if let Some(store) = store {
            let object = store.join(&entry.hash);
            if !object.exists() {
                let mut temp = tempfile::NamedTempFile::new_in(store)?;
                temp.write_all(&bytes)?;
                temp.as_file().sync_all()?;
                match temp.persist_noclobber(&object) {
                    Ok(_) => {}
                    Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e.into()),
                }
            }
            ensure!(
                digest(&read_bounded(&object, FILE_CAP)?) == entry.hash,
                "检查点对象摘要不匹配"
            );
        }
        files.insert(path, entry);
    }
    Ok(files)
}

fn read_bounded(path: &Path, cap: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= cap, "文件超过读取上限");
    Ok(bytes)
}

fn save_checkpoint(data: &Path, cwd: &Path) -> Result<Checkpoint> {
    let root = repository(cwd)?;
    let store = store_dir(data, &root)?;
    let objects = store.join("objects");
    fs::create_dir_all(&objects)?;
    let files = snapshot(&root, Some(&objects))?;
    publish_checkpoint(&root, &store, files)
}

fn publish_checkpoint(
    root: &Path,
    store: &Path,
    files: BTreeMap<String, Entry>,
) -> Result<Checkpoint> {
    // A second complete scan checks names and bytes, including clean tracked
    // files. Concurrent writes abort before publishing the manifest.
    ensure!(
        files == snapshot(root, None)?,
        "捕获期间项目文件发生变化，请稍后重试"
    );
    let captured = chrono::Utc::now().to_rfc3339();
    let entries: serde_json::Map<String, Value> =
        files.iter().map(|(p, e)| (p.clone(), e.value())).collect();
    let manifest =
        json!({"version": 1, "root": root.to_str(), "captured": captured, "files": entries});
    let bytes = serde_json::to_vec(&manifest)?;
    ensure!(bytes.len() <= LIST_CAP, "检查点清单超过 4 MiB");
    let mut temp = tempfile::NamedTempFile::new_in(store)?;
    temp.write_all(&bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(store.join("latest.json"))
        .map_err(|e| e.error)?;
    Ok(Checkpoint {
        captured,
        files,
        store: store.to_path_buf(),
    })
}

fn checkpoint_changes(root: &Path, point: &Checkpoint) -> Result<Vec<Change>> {
    let mut current = snapshot(root, None)?;
    let mut total: usize = current.values().map(|e| e.size).sum();
    // A captured path stays in scope even if it was subsequently untracked
    // or ignored. Compare its actual current bytes instead of declaring it deleted.
    for path in point.files.keys() {
        if let std::collections::btree_map::Entry::Vacant(slot) = current.entry(path.clone()) {
            let (entry, _) = read_file(root, path)?;
            total += entry.size;
            ensure!(total <= TOTAL_CAP, "检查点比较内容超过 64 MiB");
            slot.insert(entry);
        }
    }
    let paths: BTreeSet<_> = point.files.keys().chain(current.keys()).cloned().collect();
    let mut changes = Vec::new();
    for path in paths {
        let before = point.files.get(&path).filter(|e| e.kind != "missing");
        let after = current.get(&path).filter(|e| e.kind != "missing");
        if before == after {
            continue;
        }
        let status = match (before, after) {
            (None, _) => "A",
            (_, None) => "D",
            _ => "M",
        };
        let fingerprint = digest(format!("{before:?}\0{after:?}").as_bytes());
        changes.push(Change {
            path,
            old_path: None,
            area: Area::Checkpoint,
            status: status.into(),
            fingerprint,
            problem: None,
        });
    }
    Ok(changes)
}

#[derive(Clone)]
struct Overview {
    root: PathBuf,
    changes: Vec<Change>,
    checkpoint: Option<Checkpoint>,
    checkpoint_changes: Result<Vec<Change>, String>,
}

fn overview(cwd: &Path, data: &Path) -> Result<Overview> {
    let root = repository(cwd)?;
    let changes = status(&root)?;
    let checkpoint = load_checkpoint(data, &root)?;
    let checkpoint_changes = match &checkpoint {
        Some(point) => checkpoint_changes(&root, point).map_err(|e| format!("{e:#}")),
        None => Ok(Vec::new()),
    };
    Ok(Overview {
        root,
        changes,
        checkpoint,
        checkpoint_changes,
    })
}

#[derive(Clone, Debug)]
struct Detail {
    key: String,
    fingerprint: String,
    text: String,
    complete: bool,
}

fn bytes_diff(before: &[u8], after: &[u8]) -> Result<(String, bool)> {
    if before.contains(&0)
        || after.contains(&0)
        || std::str::from_utf8(before).is_err()
        || std::str::from_utf8(after).is_err()
    {
        return Ok((
            format!(
                "二进制或非 UTF-8 文件\n原内容 · {} 字节 · SHA-256 {}\n当前内容 · {} 字节 · SHA-256 {}",
                before.len(),
                digest(before),
                after.len(),
                digest(after)
            ),
            true,
        ));
    }
    let dir = tempfile::tempdir()?;
    fs::write(dir.path().join("checkpoint"), before)?;
    fs::write(dir.path().join("current"), after)?;
    let result = git_bytes(
        dir.path(),
        &[
            "diff",
            "--no-index",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--",
            "checkpoint",
            "current",
        ],
        PREVIEW_CAP,
        true,
    )?;
    let mut text = String::from_utf8_lossy(&result.bytes).into_owned();
    if result.truncated {
        text.push_str("\n…差异超过 256 KiB，预览已截断");
    }
    Ok((text, !result.truncated))
}

fn detail(root: &Path, change: &Change, point: Option<&Checkpoint>) -> Result<Detail> {
    if let Some(problem) = &change.problem {
        bail!("{problem}");
    }
    let before = match change.area {
        Area::Checkpoint => {
            let point = point.ok_or_else(|| anyhow!("尚未保存检查点"))?;
            if let Some(entry) = point.files.get(&change.path) {
                let bytes = read_bounded(&point.store.join("objects").join(&entry.hash), FILE_CAP)?;
                ensure!(
                    digest(&bytes) == entry.hash && bytes.len() == entry.size,
                    "检查点内容摘要不匹配"
                );
                bytes
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    };
    let (text, complete) = match change.area {
        Area::Checkpoint | Area::Untracked => {
            let (entry, after) = read_file(root, &change.path)?;
            let (diff, complete) = bytes_diff(&before, &after)?;
            let previous = point
                .and_then(|p| p.files.get(&change.path))
                .map(|e| format!("{} · 可执行 {}", e.kind, e.executable))
                .unwrap_or_else(|| "不存在".into());
            (
                format!(
                    "{}\n{}\n原文件 · {previous}\n当前文件 · {} · 可执行 {}\n{diff}",
                    change.label(),
                    change.area.label(),
                    entry.kind,
                    entry.executable
                ),
                complete,
            )
        }
        Area::Staged | Area::Worktree => {
            let mut args = vec![
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--no-relative",
            ];
            if change.area == Area::Staged {
                args.push("--cached");
            }
            args.extend(["--", &change.path]);
            if let Some(old) = &change.old_path {
                args.push(old);
            }
            let result = git_bytes(root, &args, PREVIEW_CAP, false)?;
            let mut text = String::from_utf8_lossy(&result.bytes).into_owned();
            if result.truncated {
                text.push_str("\n…差异超过 256 KiB，预览已截断");
            }
            (text, !result.truncated)
        }
    };
    // Revalidate after generating the preview. An already stale selection is
    // refreshed instead of attaching a review mark to a different version.
    let fresh = match change.area {
        Area::Checkpoint => {
            checkpoint_changes(root, point.ok_or_else(|| anyhow!("检查点不存在"))?)?
        }
        _ => status(root)?,
    };
    ensure!(
        fresh.iter().any(|c| c.key() == change.key()
            && c.fingerprint == change.fingerprint
            && c.problem.is_none()),
        "内容已变化，请刷新后重新审阅"
    );
    let complete = complete && text.lines().count() <= 4000;
    Ok(Detail {
        key: change.key(),
        fingerprint: change.fingerprint.clone(),
        text,
        complete,
    })
}

#[derive(Default)]
pub struct ReviewState {
    cwd: Option<String>,
    data: Option<PathBuf>,
    result: Option<Result<Overview, String>>,
    pending: Option<Pending<Overview>>,
    detail: Option<Result<Detail, String>>,
    detail_pending: Option<Pending<Detail>>,
    saving: Option<Pending<Checkpoint>>,
    selected: Option<String>,
    reviewed: HashMap<String, String>,
    compare: bool,
    notice: Option<String>,
    refreshed: Option<Instant>,
}

impl ReviewState {
    pub fn refresh(&mut self, ctx: &egui::Context, cwd: String, data: PathBuf) {
        if self.cwd.as_ref() != Some(&cwd) {
            // Dropping receivers discards results from the previous project.
            *self = Self::default();
        }
        if self.pending.is_some() || self.saving.is_some() {
            return;
        }
        self.cwd = Some(cwd.clone());
        self.data = Some(data.clone());
        self.pending = Some(Pending::spawn(ctx, move || {
            overview(Path::new(&cwd), &data)
        }));
        self.refreshed = Some(Instant::now());
    }

    pub fn poll(&mut self) {
        if let Some(result) = self.pending.as_ref().and_then(Pending::poll) {
            self.pending = None;
            if let Ok(view) = &result {
                let mut fingerprints: HashMap<String, String> = view
                    .changes
                    .iter()
                    .map(|c| (c.key(), c.fingerprint.clone()))
                    .collect();
                if let Ok(changes) = &view.checkpoint_changes {
                    fingerprints.extend(changes.iter().map(|c| (c.key(), c.fingerprint.clone())));
                }
                self.reviewed.retain(|k, v| fingerprints.get(k) == Some(v));
                let stale_detail = match &self.detail {
                    Some(Ok(d)) => fingerprints.get(&d.key) != Some(&d.fingerprint),
                    Some(Err(_)) => true,
                    None => false,
                };
                if stale_detail {
                    self.detail = None;
                    self.detail_pending = None;
                }
            } else {
                self.reviewed.clear();
            }
            self.result = Some(result);
        }
        if let Some(result) = self.detail_pending.as_ref().and_then(Pending::poll) {
            self.detail_pending = None;
            self.detail = Some(result);
        }
        if let Some(result) = self.saving.as_ref().and_then(Pending::poll) {
            self.saving = None;
            self.notice = Some(match result {
                Ok(point) => format!(
                    "已保存检查点 · {} 个路径 · {}",
                    point.files.len(),
                    display_time(&point.captured)
                ),
                Err(e) => e,
            });
            self.result = None;
            self.detail = None;
            self.detail_pending = None;
            self.selected = None;
            self.reviewed
                .retain(|key, _| !key.starts_with("Checkpoint\0"));
            self.refreshed = None;
        }
    }

    pub fn show(&mut self, ui: &mut Ui, pal: &Palette, cwd: String, data: PathBuf) {
        self.poll();
        if self.cwd.as_ref() != Some(&cwd)
            || self
                .refreshed
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(3))
        {
            self.refresh(ui.ctx(), cwd.clone(), data.clone());
        }
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        ui.horizontal(|ui| {
            if widgets::quiet_button(ui, icon::ARROWS_CLOCKWISE, "刷新变更", pal, false).clicked()
            {
                self.refresh(ui.ctx(), cwd.clone(), data.clone());
            }
            let ready = self.pending.is_none()
                && self.saving.is_none()
                && matches!(self.result, Some(Ok(_)));
            if ui
                .add_enabled(ready, egui::Button::new("保存检查点"))
                .clicked()
            {
                let root = PathBuf::from(&cwd);
                self.saving = Some(Pending::spawn(ui.ctx(), move || {
                    save_checkpoint(&data, &root)
                }));
            }
            if self.pending.is_some() || self.saving.is_some() {
                ui.spinner();
            }
        });
        if let Some(notice) = &self.notice {
            ui.add(Label::new(RichText::new(notice).color(pal.mute)).wrap());
        }
        let view = match &self.result {
            Some(Ok(view)) => view.clone(),
            Some(Err(error)) => {
                ui.add(Label::new(RichText::new(error).color(pal.fail.fg)).wrap());
                return;
            }
            None => {
                ui.label("正在读取文件变更…");
                return;
            }
        };
        let before_compare = self.compare;
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.compare, false, "Git 变更");
            ui.add_enabled_ui(view.checkpoint.is_some(), |ui| {
                ui.selectable_value(&mut self.compare, true, "与检查点比较");
            });
        });
        if self.compare != before_compare {
            self.selected = None;
            self.detail = None;
            self.detail_pending = None;
        }
        ui.add(Label::new(RichText::new("检查点覆盖已跟踪与未忽略文件（含清洁文件），最多 10000 项、单文件 4 MiB、总计 64 MiB。").font(theme::body(theme::TINY)).color(pal.mute)).wrap());
        if self.compare
            && let Some(point) = &view.checkpoint
        {
            ui.label(
                RichText::new(format!(
                    "捕获于 {} · {} 个路径",
                    display_time(&point.captured),
                    point.files.len()
                ))
                .color(pal.mute),
            );
        }
        let changes = if self.compare {
            match &view.checkpoint_changes {
                Ok(changes) => changes,
                Err(e) => {
                    ui.add(Label::new(RichText::new(e).color(pal.fail.fg)).wrap());
                    return;
                }
            }
        } else {
            &view.changes
        };
        if changes.is_empty() {
            ui.label(if self.compare {
                "当前文件与检查点一致"
            } else {
                "工作区干净"
            });
            return;
        }
        if self
            .selected
            .as_ref()
            .is_none_or(|key| !changes.iter().any(|c| &c.key() == key))
        {
            self.selected = Some(changes[0].key());
            self.detail = None;
            self.detail_pending = None;
        }
        egui::ScrollArea::vertical()
            .id_salt("review-files")
            .max_height(180.0)
            .show(ui, |ui| {
                for change in changes {
                    let key = change.key();
                    let reviewed = self.reviewed.get(&key) == Some(&change.fingerprint);
                    let label = format!(
                        "{} {} · {} · {}",
                        if reviewed { "✓" } else { "○" },
                        change.area.label(),
                        change.status_label(),
                        change.label()
                    );
                    if ui
                        .selectable_label(self.selected.as_ref() == Some(&key), label)
                        .clicked()
                    {
                        self.selected = Some(key);
                        self.detail = None;
                        self.detail_pending = None;
                    }
                }
            });
        let Some(change) = changes.iter().find(|c| Some(c.key()) == self.selected) else {
            return;
        };
        if self.detail.is_none() && self.detail_pending.is_none() {
            let root = view.root.clone();
            let selected = change.clone();
            let point = view.checkpoint.clone();
            self.detail_pending = Some(Pending::spawn(ui.ctx(), move || {
                detail(&root, &selected, point.as_ref())
            }));
        }
        widgets::rule(ui, pal.line_soft);
        match &self.detail {
            None => {
                ui.label("正在读取文件差异…");
            }
            Some(Err(e)) => {
                ui.add(Label::new(RichText::new(e).color(pal.fail.fg)).wrap());
            }
            Some(Ok(detail)) => {
                let current =
                    detail.key == change.key() && detail.fingerprint == change.fingerprint;
                let mut reviewed = self.reviewed.get(&detail.key) == Some(&detail.fingerprint);
                if ui
                    .add_enabled(
                        current && detail.complete && self.pending.is_none(),
                        egui::Checkbox::new(&mut reviewed, "已审阅此版本"),
                    )
                    .changed()
                {
                    if reviewed {
                        self.reviewed
                            .insert(detail.key.clone(), detail.fingerprint.clone());
                    } else {
                        self.reviewed.remove(&detail.key);
                    }
                }
                egui::ScrollArea::horizontal()
                    .id_salt("review-patch")
                    .show(ui, |ui| {
                        for line in detail.text.lines().take(4000) {
                            let color = if line.starts_with('+') && !line.starts_with("+++") {
                                pal.ok.fg
                            } else if line.starts_with('-') && !line.starts_with("---") {
                                pal.fail.fg
                            } else if line.starts_with("@@") {
                                pal.accent
                            } else {
                                pal.ink
                            };
                            ui.add(
                                Label::new(RichText::new(line).monospace().size(12.0).color(color))
                                    .wrap_mode(egui::TextWrapMode::Extend),
                            );
                        }
                        if detail.text.lines().count() > 4000 {
                            ui.label("…仅显示前 4000 行");
                        }
                    });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(root: &Path, args: &[&str]) -> Vec<u8> {
        let exe = sa_adapters::sys::Sys::from_process()
            .which(if cfg!(windows) { "git.exe" } else { "git" })
            .expect("Git is required for review tests");
        let out = Command::new(exe)
            .current_dir(root)
            .args([
                "-c",
                "user.name=Review Test",
                "-c",
                "user.email=review@example.invalid",
                "-c",
                "core.autocrlf=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        fs::write(dir.path().join("tracked.txt"), "base\n").unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
        git(dir.path(), &["add", "--", "tracked.txt", ".gitignore"]);
        git(dir.path(), &["commit", "-qm", "initial"]);
        dir
    }

    #[test]
    fn nul_status_preserves_unicode_newlines_renames_and_rejects_bad_paths() {
        let source = "2 R. N... 100644 100644 100644 aaaa bbbb R100 新\n名字.txt\0旧 名字.txt\0? --untracked.txt\0";
        let changes = parse_status(source.as_bytes()).unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, "新\n名字.txt");
        assert_eq!(changes[0].old_path.as_deref(), Some("旧 名字.txt"));
        assert_eq!(changes[0].area, Area::Staged);
        assert_eq!(changes[0].status_label(), "重命名");
        assert_eq!(changes[1].path, "--untracked.txt");
        assert_eq!(changes[1].status_label(), "新增");
        let changed = parse_status(b"1 AM N... 000000 100644 100644 aaaa bbbb file.txt\0").unwrap();
        assert_eq!(changed[0].status_label(), "新增");
        assert_eq!(changed[1].status_label(), "修改");
        let conflict =
            parse_status(b"u AA N... 000000 100644 100644 100644 aaaa bbbb cccc conflict.txt\0")
                .unwrap();
        assert_eq!(conflict[0].status_label(), "冲突");
        assert!(changes[0].label().contains("新\\n名字.txt"));
        assert!(parse_status(b"? ../escape\0").is_err());
        assert!(parse_status(b"? .git/config\0").is_err());
        assert!(parse_status(b"? broken").is_err());
        assert!(parse_status(b"2 R. N... 100644 100644 100644 aaaa bbbb R100 new\0").is_err());
    }

    #[test]
    fn file_review_separates_index_worktree_and_literal_paths() {
        let dir = repo();
        let root = repository(dir.path()).unwrap();
        fs::write(root.join("tracked.txt"), "staged\n").unwrap();
        git(&root, &["add", "--", "tracked.txt"]);
        fs::write(root.join("tracked.txt"), "working\n").unwrap();
        fs::write(root.join("中文 [x].txt"), "literal\n").unwrap();
        fs::write(root.join("中文 x.txt"), "must not appear\n").unwrap();
        git(&root, &["add", "--", "中文 [x].txt", "中文 x.txt"]);
        fs::write(root.join("--new.txt"), "untracked body\n").unwrap();
        let changes = status(&root).unwrap();
        let staged = changes
            .iter()
            .find(|c| c.path == "tracked.txt" && c.area == Area::Staged)
            .unwrap();
        let unstaged = changes
            .iter()
            .find(|c| c.path == "tracked.txt" && c.area == Area::Worktree)
            .unwrap();
        let a = detail(&root, staged, None).unwrap();
        let b = detail(&root, unstaged, None).unwrap();
        assert!(a.text.contains("+staged"));
        assert!(!a.text.contains("+working"));
        assert!(b.text.contains("-staged") && b.text.contains("+working"));
        let literal = changes.iter().find(|c| c.path == "中文 [x].txt").unwrap();
        let text = detail(&root, literal, None).unwrap().text;
        assert!(text.contains("+literal"));
        assert!(
            text.lines()
                .any(|line| line.starts_with("diff --git ") && line.contains("中文 [x].txt")),
            "{text}"
        );
        assert!(!text.contains("must not appear"));
        let new = changes.iter().find(|c| c.path == "--new.txt").unwrap();
        assert!(
            detail(&root, new, None)
                .unwrap()
                .text
                .contains("+untracked body")
        );
        fs::write(root.join("tracked.txt"), "changed while reviewing\n").unwrap();
        assert!(
            detail(&root, unstaged, None)
                .unwrap_err()
                .to_string()
                .contains("内容已变化")
        );
        assert_ne!(
            status(&root)
                .unwrap()
                .iter()
                .find(|c| c.key() == unstaged.key())
                .unwrap()
                .fingerprint,
            unstaged.fingerprint
        );
    }

    #[test]
    fn staged_rename_preview_keeps_both_paths() {
        let dir = repo();
        let root = repository(dir.path()).unwrap();
        git(&root, &["mv", "--", "tracked.txt", "重命名 [x].txt"]);
        let changes = status(&root).unwrap();
        let rename = changes.iter().find(|c| c.path == "重命名 [x].txt").unwrap();
        assert_eq!(rename.old_path.as_deref(), Some("tracked.txt"));
        let preview = detail(&root, rename, None).unwrap();
        assert!(
            preview.text.contains("rename from tracked.txt"),
            "{}",
            preview.text
        );
        assert!(
            preview.text.contains("rename to 重命名 [x].txt"),
            "{}",
            preview.text
        );
    }

    #[test]
    fn checkpoints_reopen_compare_clean_files_additions_deletions_and_keep_git_unchanged() {
        let dir = repo();
        let data = tempfile::tempdir().unwrap();
        let root = repository(dir.path()).unwrap();
        fs::write(root.join("remove.txt"), "remove me\n").unwrap();
        fs::create_dir(root.join("ignored")).unwrap();
        fs::write(root.join("ignored/private.txt"), "excluded\n").unwrap();
        let index_before = fs::read(root.join(".git/index")).unwrap();
        let head_before = git(&root, &["rev-parse", "HEAD"]);
        let saved = save_checkpoint(data.path(), &root).unwrap();
        assert!(saved.files.contains_key("tracked.txt"));
        assert!(!saved.files.contains_key("ignored/private.txt"));
        let reopened = load_checkpoint(data.path(), &root).unwrap().unwrap();
        assert_eq!(saved.files, reopened.files);
        fs::write(root.join("tracked.txt"), "after capture\n").unwrap();
        fs::remove_file(root.join("remove.txt")).unwrap();
        fs::write(root.join("empty.txt"), "").unwrap();
        let changes = checkpoint_changes(&root, &reopened).unwrap();
        assert_eq!(
            changes
                .iter()
                .map(|c| (c.path.as_str(), c.status.as_str()))
                .collect::<Vec<_>>(),
            [
                ("empty.txt", "A"),
                ("remove.txt", "D"),
                ("tracked.txt", "M")
            ]
        );
        let tracked = changes.iter().find(|c| c.path == "tracked.txt").unwrap();
        let preview = detail(&root, tracked, Some(&reopened)).unwrap();
        assert!(preview.text.contains("-base") && preview.text.contains("+after capture"));
        assert_eq!(fs::read(root.join(".git/index")).unwrap(), index_before);
        assert_eq!(git(&root, &["rev-parse", "HEAD"]), head_before);
        assert!(!root.join(".superagent").exists());
    }

    #[test]
    fn captured_files_remain_comparable_after_ignore_rules_change() {
        let dir = repo();
        let data = tempfile::tempdir().unwrap();
        let root = repository(dir.path()).unwrap();
        fs::write(root.join("later-ignored.txt"), "captured\n").unwrap();
        let saved = save_checkpoint(data.path(), &root).unwrap();
        fs::write(root.join(".gitignore"), "ignored/\nlater-ignored.txt\n").unwrap();
        fs::write(root.join("later-ignored.txt"), "still exists\n").unwrap();
        let changes = checkpoint_changes(&root, &saved).unwrap();
        let change = changes
            .iter()
            .find(|c| c.path == "later-ignored.txt")
            .unwrap();
        assert_eq!(change.status, "M");
        assert!(
            detail(&root, change, Some(&saved))
                .unwrap()
                .text
                .contains("+still exists")
        );
    }

    #[test]
    fn desktop_review_selects_marks_and_reopens_checkpoint_comparison() {
        use egui_kittest::{Harness, kittest::Queryable};
        use std::sync::{Arc, Mutex};
        let dir = repo();
        let data = tempfile::tempdir().unwrap();
        let root = repository(dir.path()).unwrap();
        fs::write(root.join("tracked.txt"), "first edit\n").unwrap();
        let ui_root = root.clone();
        let ui_data = data.path().to_path_buf();
        let mut h = Harness::builder()
            .with_size(egui::vec2(1440.0, 900.0))
            .build_eframe(move |cc| {
                crate::theme::install_fonts(&cc.egui_ctx);
                let ctx = cc.egui_ctx.clone();
                let host = crate::host::Host::start_with(
                    ui_data.clone(),
                    crate::host::tests::fakes,
                    move || ctx.request_repaint(),
                )
                .map_err(|e| format!("{e:#}"));
                let worker = crate::worker::spawn(
                    crate::data::StoreSource::opener(ui_data.clone()),
                    cc.egui_ctx.clone(),
                );
                let opts = crate::cli::Options {
                    data: Some(ui_data),
                    ..Default::default()
                };
                let mut app =
                    crate::app::App::new(cc, opts, worker, Arc::new(Mutex::new(None)), host);
                app.composer.project = Some(ui_root);
                app.inspector_open = true;
                app.tab = crate::app::Tab::Diff;
                app
            });
        let wait = |h: &mut Harness<'_, crate::app::App>, predicate: fn(&ReviewState) -> bool| {
            let end = Instant::now() + Duration::from_secs(15);
            while Instant::now() < end {
                h.step();
                if predicate(&h.state().diff) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("review UI timed out");
        };
        wait(&mut h, |s| {
            matches!(s.detail, Some(Ok(_))) && s.pending.is_none()
        });
        h.run_steps(3);
        h.get_by_label("已审阅此版本").click();
        h.run_steps(3);
        assert_eq!(h.state().diff.reviewed.len(), 1);
        h.get_by_label("保存检查点").click();
        wait(&mut h, |s| {
            s.result
                .as_ref()
                .and_then(|r| r.as_ref().ok())
                .is_some_and(|v| v.checkpoint.is_some())
                && s.saving.is_none()
        });
        h.run_steps(3);
        h.get_by_label("与检查点比较").click();
        h.run_steps(3);
        assert!(h.state().diff.compare);
        fs::write(root.join("tracked.txt"), "after checkpoint\n").unwrap();
        let ctx = h.state().ctx.clone();
        // Reopening the panel uses only the persisted manifest and objects.
        h.state_mut().diff = ReviewState::default();
        h.state_mut().diff.refresh(
            &ctx,
            root.to_string_lossy().into_owned(),
            data.path().to_path_buf(),
        );
        wait(&mut h, |s| {
            s.result
                .as_ref()
                .and_then(|r| r.as_ref().ok())
                .is_some_and(|v| v.checkpoint.is_some())
        });
        h.get_by_label("与检查点比较").click();
        wait(&mut h, |s| {
            s.compare && matches!(&s.detail, Some(Ok(d)) if d.text.contains("+after checkpoint"))
        });
        assert!(h.state().diff.reviewed.is_empty());
        assert!(
            h.state()
                .diff
                .detail
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap()
                .text
                .contains("-first edit")
        );
    }

    #[test]
    fn checkpoint_failure_preserves_previous_manifest_and_detects_tampering() {
        let dir = repo();
        let data = tempfile::tempdir().unwrap();
        let root = repository(dir.path()).unwrap();
        let saved = save_checkpoint(data.path(), &root).unwrap();
        let manifest = fs::read(saved.store.join("latest.json")).unwrap();
        let first_scan = snapshot(&root, Some(&saved.store.join("objects"))).unwrap();
        fs::write(root.join("tracked.txt"), "concurrent edit\n").unwrap();
        let failure = publish_checkpoint(&root, &saved.store, first_scan).unwrap_err();
        assert!(failure.to_string().contains("捕获期间"));
        assert_eq!(fs::read(saved.store.join("latest.json")).unwrap(), manifest);
        let huge = fs::File::create(root.join("huge.bin")).unwrap();
        huge.set_len(FILE_CAP as u64 + 1).unwrap();
        assert!(save_checkpoint(data.path(), &root).is_err());
        assert_eq!(fs::read(saved.store.join("latest.json")).unwrap(), manifest);
        drop(huge);
        fs::remove_file(root.join("huge.bin")).unwrap();
        let change = checkpoint_changes(&root, &saved)
            .unwrap()
            .into_iter()
            .find(|c| c.path == "tracked.txt")
            .unwrap();
        fs::write(
            saved
                .store
                .join("objects")
                .join(&saved.files["tracked.txt"].hash),
            "tampered",
        )
        .unwrap();
        assert!(
            detail(&root, &change, Some(&saved))
                .unwrap_err()
                .to_string()
                .contains("摘要")
        );
        assert!(save_checkpoint(&root, &root).is_err());
    }

    #[test]
    fn binary_and_large_previews_are_bounded() {
        let (binary, complete) = bytes_diff(&[0, 1], &[0, 2]).unwrap();
        assert!(binary.contains("二进制") && binary.contains("SHA-256") && complete);
        let before = "before\n".repeat(50000);
        let after = "after\n".repeat(50000);
        let (text, complete) = bytes_diff(before.as_bytes(), after.as_bytes()).unwrap();
        assert!(!complete);
        assert!(text.len() < PREVIEW_CAP + 100);
    }

    #[test]
    fn refresh_invalidates_reviewed_content_and_discards_other_project_results() {
        let dir = repo();
        let data = tempfile::tempdir().unwrap();
        let root = repository(dir.path()).unwrap();
        fs::write(root.join("tracked.txt"), "first edit\n").unwrap();
        let first = overview(&root, data.path()).unwrap();
        let change = first
            .changes
            .iter()
            .find(|c| c.path == "tracked.txt")
            .unwrap();
        let mut state = ReviewState::default();
        state
            .reviewed
            .insert(change.key(), change.fingerprint.clone());
        state.detail = Some(Ok(detail(&root, change, None).unwrap()));
        fs::write(root.join("tracked.txt"), "second edit\n").unwrap();
        let ctx = egui::Context::default();
        let newer = overview(&root, data.path()).unwrap();
        state.pending = Some(Pending::spawn(&ctx, move || Ok(newer)));
        let deadline = Instant::now() + Duration::from_secs(2);
        while state.pending.is_some() && Instant::now() < deadline {
            state.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(state.pending.is_none());
        assert!(state.reviewed.is_empty());
        assert!(state.detail.is_none());
        state.cwd = Some(root.to_string_lossy().into_owned());
        let other = repo();
        state.refresh(
            &ctx,
            other.path().to_string_lossy().into_owned(),
            data.path().to_path_buf(),
        );
        assert!(state.result.is_none());
        assert!(state.selected.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_targets_are_captured_without_reading_outside_files() {
        use std::os::unix::fs::symlink;
        let dir = repo();
        let outside = tempfile::tempdir().unwrap();
        let root = repository(dir.path()).unwrap();
        fs::write(outside.path().join("secret"), "not captured").unwrap();
        symlink(outside.path().join("secret"), root.join("link")).unwrap();
        let (entry, bytes) = read_file(&root, "link").unwrap();
        assert_eq!(entry.kind, "symlink");
        assert_ne!(bytes, b"not captured");
        symlink(outside.path(), root.join("outside")).unwrap();
        assert!(read_file(&root, "outside/secret").is_err());
    }
}
