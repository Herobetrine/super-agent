//! One-way import of T3 projects and conversation archives.

use crate::projects::{self, Project};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

const DATABASE: &str = "statev2.sqlite";
const PROJECTS: &str = "projection_projects";
const THREADS: &str = "orchestration_v2_projection_threads";
const MESSAGES: &str = "orchestration_v2_projection_messages";
const ITEMS: &str = "orchestration_v2_projection_turn_items";
const LIVE_THREADS: &str = "FROM orchestration_v2_projection_threads t
    JOIN projection_projects p ON p.project_id = t.project_id
    WHERE t.deleted_at IS NULL AND p.deleted_at IS NULL";

pub fn default_folder() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("USERPROFILE")?)
            .join(".t3")
            .join("userdata"),
    )
}

#[derive(Debug, PartialEq, Eq)]
pub enum ScanError {
    NotFound,
    Unreadable(String),
    Unsupported { missing: String },
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "缺少 {DATABASE}"),
            Self::Unreadable(reason) => f.write_str(reason),
            Self::Unsupported { missing } => write!(f, "缺少 {missing}"),
        }
    }
}

impl std::error::Error for ScanError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct T3Project {
    pub id: String,
    pub title: String,
    pub path: String,
    pub exists: bool,
    pub already_listed: bool,
    pub pinned: bool,
    pub updated: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scan {
    pub folder: PathBuf,
    pub projects: Vec<T3Project>,
    pub threads: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub folder: PathBuf,
    pub exported: usize,
    pub total: usize,
    pub failed: Vec<(String, String)>,
    pub stopped: bool,
}

fn unreadable(error: impl fmt::Display) -> ScanError {
    ScanError::Unreadable(error.to_string())
}

struct Source {
    // Close SQLite before releasing its source handles or deleting its snapshot.
    conn: Connection,
    _guards: Vec<File>,
    _snapshot: Option<tempfile::TempDir>,
}

fn read_handle(path: &Path, allow_other_writers: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;
        const FILE_SHARE_WRITE: u32 = 2;
        let share = if allow_other_writers {
            FILE_SHARE_READ | FILE_SHARE_WRITE
        } else {
            FILE_SHARE_READ
        };
        options.share_mode(share);
    }
    #[cfg(not(windows))]
    let _ = allow_other_writers;
    options.open(path)
}

fn open_read_only(file: &Path, option: &str) -> Result<Connection> {
    let absolute = std::path::absolute(file)?;
    let url = Url::from_file_path(&absolute).map_err(|()| anyhow::anyhow!("无法转换数据库路径"))?;
    // SQLite's file URI requires a local authority even for a UNC path.
    let path = match url.host_str() {
        Some(host) => format!("//{host}{}", url.path()),
        None => url.path().to_owned(),
    };
    let uri = format!("file://{path}?mode=ro{option}");
    let conn = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    conn.busy_timeout(Duration::from_secs(2))?;
    conn.execute_batch("PRAGMA query_only = ON; BEGIN;")?;
    // Acquire the SQLite read snapshot here so WAL errors enter the fallback.
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))?;
    Ok(conn)
}

fn open_source(folder: &Path) -> Result<Source, ScanError> {
    let db = folder.join(DATABASE);
    match fs::metadata(&db) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(unreadable(format!("{DATABASE} 不是文件"))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(ScanError::NotFound),
        Err(error) => return Err(unreadable(error)),
    }
    let db = db.canonicalize().map_err(unreadable)?;
    let sidecar = |suffix: &str| {
        let mut path = db.as_os_str().to_os_string();
        path.push(suffix);
        PathBuf::from(path)
    };
    let wal = sidecar("-wal");
    let shm = sidecar("-shm");
    let active = || -> Result<Source> {
        // The Windows VFS can create a missing SHM even for a read-only connection.
        // Retain the existing files without delete sharing while SQLite uses them.
        let guards = vec![
            read_handle(&db, true)?,
            read_handle(&wal, true)?,
            read_handle(&shm, true)?,
        ];
        let conn = open_read_only(&db, "&readonly_shm=1")?;
        Ok(Source {
            conn,
            _guards: guards,
            _snapshot: None,
        })
    };
    if let Ok(source) = active() {
        return Ok(source);
    }

    let offline = || -> Result<Source> {
        // On Windows, opening without write/delete sharing excludes an active writer
        // and prevents one from starting or replacing the source during this read.
        let mut guards = vec![read_handle(&db, false)?];
        match read_handle(&wal, false) {
            Ok(file) => guards.push(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let conn = open_read_only(&db, "&immutable=1")?;
                return Ok(Source {
                    conn,
                    _guards: guards,
                    _snapshot: None,
                });
            }
            Err(error) => return Err(error.into()),
        }

        // An offline WAL can contain committed data missing from the base file.
        // Copy the protected pair and allow SQLite to manage SHM in temp only.
        let temp = safe_output_root(&std::env::temp_dir(), folder)?;
        let snapshot = tempfile::tempdir_in(temp)?;
        let copy = snapshot.path().join(DATABASE);
        fs::copy(&db, &copy)?;
        fs::copy(&wal, snapshot.path().join(format!("{DATABASE}-wal")))?;
        let conn = open_read_only(&copy, "")?;
        Ok(Source {
            conn,
            _guards: Vec::new(),
            _snapshot: Some(snapshot),
        })
    };
    offline().map_err(unreadable)
}

struct Schema {
    project_pinned: bool,
    project_archived: bool,
    thread_archived: bool,
}

fn schema(conn: &Connection) -> Result<Schema, ScanError> {
    let columns = |table: &str, required: &[&str]| -> Result<HashSet<String>, ScanError> {
        let mut stmt = conn
            .prepare("SELECT name FROM pragma_table_info(?1)")
            .map_err(unreadable)?;
        let found: HashSet<String> = stmt
            .query_map([table], |row| row.get(0))
            .map_err(unreadable)?
            .collect::<rusqlite::Result<_>>()
            .map_err(unreadable)?;
        if found.is_empty() {
            return Err(ScanError::Unsupported {
                missing: table.to_owned(),
            });
        }
        for name in required {
            if !found.contains(*name) {
                return Err(ScanError::Unsupported {
                    missing: format!("{table}.{name}"),
                });
            }
        }
        Ok(found)
    };
    let projects = columns(
        PROJECTS,
        &[
            "project_id",
            "title",
            "workspace_root",
            "updated_at",
            "deleted_at",
        ],
    )?;
    let threads = columns(
        THREADS,
        &[
            "thread_id",
            "project_id",
            "title",
            "default_provider",
            "created_at",
            "updated_at",
            "deleted_at",
            "payload_json",
        ],
    )?;
    columns(
        MESSAGES,
        &[
            "message_id",
            "thread_id",
            "role",
            "created_at",
            "payload_json",
        ],
    )?;
    columns(
        ITEMS,
        &["thread_id", "ordinal", "type", "status", "payload_json"],
    )?;
    Ok(Schema {
        project_pinned: projects.contains("pinned_at"),
        project_archived: projects.contains("archived_at"),
        thread_archived: threads.contains("archived_at"),
    })
}

#[derive(Serialize)]
struct ArchiveProject {
    id: String,
    title: String,
    path: String,
    pinned: bool,
    archived: bool,
    updated_at: Option<String>,
}

fn source_projects(conn: &Connection, schema: &Schema) -> Result<Vec<ArchiveProject>> {
    let pinned = if schema.project_pinned {
        "pinned_at IS NOT NULL"
    } else {
        "0"
    };
    let archived = if schema.project_archived {
        "archived_at IS NOT NULL"
    } else {
        "0"
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT project_id, title, workspace_root, {pinned}, {archived}, updated_at
         FROM projection_projects WHERE deleted_at IS NULL
         ORDER BY 4 DESC, updated_at DESC, project_id"
    ))?;
    Ok(stmt
        .query_map([], |row| {
            Ok(ArchiveProject {
                id: row.get(0)?,
                title: row.get(1)?,
                path: row.get(2)?,
                pinned: row.get(3)?,
                archived: row.get(4)?,
                updated_at: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

fn ticket_paths(store: &Connection) -> Result<HashSet<String>> {
    let has_tickets: bool = store.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'tickets')",
        [],
        |row| row.get(0),
    )?;
    if has_tickets {
        projects::path_keys(store, "SELECT DISTINCT cwd FROM tickets")
    } else {
        Ok(HashSet::new())
    }
}

pub fn scan(folder: &Path, store: &Connection) -> Result<Scan, ScanError> {
    let source = open_source(folder)?;
    let conn = &source.conn;
    let schema = schema(conn)?;
    let projects = source_projects(conn, &schema).map_err(unreadable)?;
    let mut listed = ticket_paths(store).map_err(unreadable)?;
    listed.extend(
        projects::path_keys(store, "SELECT path FROM studio_projects").map_err(unreadable)?,
    );
    let threads: i64 = conn
        .query_row(&format!("SELECT count(*) {LIVE_THREADS}"), [], |row| {
            row.get(0)
        })
        .map_err(unreadable)?;
    let threads = usize::try_from(threads).map_err(unreadable)?;
    Ok(Scan {
        folder: folder.to_path_buf(),
        projects: projects
            .into_iter()
            .map(|p| T3Project {
                id: p.id,
                title: p.title,
                exists: Path::new(&p.path).is_dir(),
                already_listed: listed.contains(&projects::path_key(&p.path)),
                path: p.path,
                pinned: p.pinned,
                updated: p.updated_at,
            })
            .collect(),
        threads,
    })
}

pub fn import_projects(store: &Connection, chosen: &[T3Project]) -> Result<usize> {
    let added = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
    let tickets = ticket_paths(store)?;
    let selected: Vec<_> = chosen
        .iter()
        .filter(|p| {
            p.exists && !p.already_listed && !tickets.contains(&projects::path_key(&p.path))
        })
        .map(|p| Project {
            path: p.path.clone(),
            name: p.title.clone(),
            pinned: p.pinned,
            source: "t3".to_owned(),
            source_id: Some(p.id.clone()),
            source_updated: p.updated.clone(),
            added,
        })
        .collect();
    projects::add(store, &selected)
}

fn resolve_output_path(root: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(root)?;
    let mut existing = absolute.as_path();
    let mut tail = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(path) => return Ok(tail.iter().rev().fold(path, |path, part| path.join(part))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                tail.push(existing.file_name().context("无法解析归档目录")?);
                existing = existing.parent().context("无法解析归档目录")?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn safe_output_root(root: &Path, source_folder: &Path) -> Result<PathBuf> {
    let resolved = resolve_output_path(root)?;
    let key = |path: &Path| projects::path_key(&path.to_string_lossy());
    let output_key = key(&resolved);
    let protect = |path: &Path| -> Result<()> {
        let source_key = key(path).trim_end_matches('/').to_owned();
        ensure!(
            output_key != source_key && !output_key.starts_with(&format!("{source_key}/")),
            "归档和临时副本必须保存到 T3 数据目录之外"
        );
        Ok(())
    };
    protect(&source_folder.canonicalize()?)?;
    let source_db = source_folder.join(DATABASE).canonicalize()?;
    protect(source_db.parent().context("无法解析 T3 数据目录")?)?;
    if let Some(home) = default_folder().and_then(|folder| folder.parent().map(Path::to_path_buf)) {
        protect(&resolve_output_path(&home)?)?;
    }
    Ok(resolved)
}

fn new_archive(conn: &Connection, root: &Path, source_folder: &Path) -> Result<PathBuf> {
    let root = safe_output_root(root, source_folder)?;
    fs::create_dir_all(&root)?;
    let stamp: String = conn.query_row(
        "SELECT strftime('%Y%m%d-%H%M%S', 'now', 'localtime')",
        [],
        |row| row.get(0),
    )?;
    let mut folder = root.join(&stamp);
    for suffix in 2u64.. {
        match fs::create_dir(&folder) {
            Ok(()) => return Ok(folder),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                folder = root.join(format!("{stamp}-{suffix}"));
            }
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!()
}

fn slug(text: &str) -> String {
    // Also limit UTF-16 length to keep Windows path components bounded for emoji.
    let mut result = String::new();
    let mut units = 0;
    for ch in text.trim().chars() {
        let ch = if ch.is_control() || "<>:\"/\\|?*".contains(ch) {
            '-'
        } else {
            ch
        };
        units += ch.len_utf16();
        if units > 60 {
            break;
        }
        result.push(ch);
    }
    let result = result.trim_end_matches(['.', ' ']);
    if result.is_empty() {
        return "untitled".to_owned();
    }
    let stem = result.split('.').next().unwrap_or_default().trim_end();
    let upper = stem.to_ascii_uppercase();
    let reserved = matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        upper.strip_prefix(*prefix).is_some_and(|n| {
            matches!(
                n,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    if reserved {
        // Replace one ASCII unit so the prefix does not grow a 60-unit slug.
        format!("_{}", &result[1..])
    } else {
        result.to_owned()
    }
}

fn project_slugs(projects: &[ArchiveProject]) -> BTreeMap<&str, String> {
    let mut taken = HashSet::new();
    let mut slugs = BTreeMap::new();
    for project in projects {
        let base = slug(&project.title);
        let mut chosen = base.clone();
        for index in 2u64.. {
            if taken.insert(chosen.to_lowercase()) {
                break;
            }
            let suffix = format!("-{index}");
            let mut units = 60 - suffix.len();
            chosen = base
                .chars()
                .take_while(|ch| {
                    if let Some(remaining) = units.checked_sub(ch.len_utf16()) {
                        units = remaining;
                        true
                    } else {
                        false
                    }
                })
                .collect();
            chosen.push_str(&suffix);
        }
        slugs.insert(project.id.as_str(), chosen);
    }
    slugs
}

#[derive(Serialize)]
struct ArchiveThread {
    id: String,
    project_id: String,
    title: String,
    provider: String,
    created_at: String,
    updated_at: String,
    archived: bool,
}

fn source_threads(conn: &Connection, schema: &Schema) -> Result<Vec<ArchiveThread>> {
    let archived = if schema.thread_archived {
        "t.archived_at IS NOT NULL"
    } else {
        "0"
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT t.thread_id, t.project_id, t.title, t.default_provider, t.created_at,
            t.updated_at, {archived}
         {LIVE_THREADS} ORDER BY t.updated_at DESC, t.thread_id"
    ))?;
    Ok(stmt
        .query_map([], |row| {
            Ok(ArchiveThread {
                id: row.get(0)?,
                project_id: row.get(1)?,
                title: row.get(2)?,
                provider: row.get(3)?,
                created_at: row.get(4)?,
                updated_at: row.get(5)?,
                archived: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

struct NewFile {
    path: PathBuf,
    writer: Option<BufWriter<File>>,
    keep: bool,
}

impl NewFile {
    fn open(path: PathBuf) -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self {
            path,
            writer: Some(BufWriter::new(file)),
            keep: false,
        })
    }

    fn writer(&mut self) -> &mut BufWriter<File> {
        self.writer.as_mut().expect("new archive file is open")
    }

    fn finish(&mut self) -> Result<()> {
        let writer = self.writer();
        writer.flush()?;
        writer.get_ref().sync_all()?;
        Ok(())
    }
}

impl Drop for NewFile {
    fn drop(&mut self) {
        drop(self.writer.take());
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn write_new(path: PathBuf, body: impl FnOnce(&mut BufWriter<File>) -> Result<()>) -> Result<()> {
    let mut file = NewFile::open(path)?;
    body(file.writer())?;
    file.finish()?;
    file.keep = true;
    Ok(())
}

fn one_line(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

fn markdown_text(value: &str) -> String {
    let mut escaped = String::new();
    for ch in one_line(value).chars() {
        if matches!(
            ch,
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '&' | '#' | '!' | '~'
        ) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

fn link_path(path: &str) -> String {
    let mut encoded = String::new();
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn payload(raw: &str, kind: &str) -> Result<Value> {
    let value: Value = serde_json::from_str(raw).with_context(|| format!("{kind} JSON 无效"))?;
    ensure!(value.is_object(), "{kind} JSON 应为对象");
    Ok(value)
}

fn jsonl(out: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    out.write_all(b"\n")?;
    Ok(())
}

fn attachment_name(attachment: &Value) -> Option<&str> {
    let name = attachment["name"].as_str()?;
    let leaf = name.rsplit(['/', '\\']).next()?;
    (!leaf.is_empty() && !matches!(leaf, "." | "..")).then_some(leaf)
}

struct Message {
    id: String,
    role: String,
    time: String,
    raw: String,
}

fn messages(conn: &Connection, thread: &str) -> Result<Vec<Message>> {
    let mut stmt = conn.prepare(
        "SELECT message_id, role, created_at, payload_json
         FROM orchestration_v2_projection_messages WHERE thread_id = ?1
         ORDER BY created_at, message_id",
    )?;
    Ok(stmt
        .query_map([thread], |row| {
            Ok(Message {
                id: row.get(0)?,
                role: row.get(1)?,
                time: row.get(2)?,
                raw: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

fn write_message(markdown: &mut impl Write, lines: &mut impl Write, msg: &Message) -> Result<()> {
    let body = payload(&msg.raw, "消息")?;
    let text = body["text"].as_str().context("消息缺少文本")?;
    let attachments: Vec<&str> = body["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(attachment_name)
        .collect();
    let label = match msg.role.as_str() {
        "user" => "用户",
        "assistant" => "助手",
        _ => "系统",
    };
    writeln!(
        markdown,
        "\n### {label} · {}\n\n{text}",
        markdown_text(&msg.time)
    )?;
    if !attachments.is_empty() {
        let names = attachments
            .iter()
            .map(|name| markdown_text(name))
            .collect::<Vec<_>>();
        writeln!(markdown, "\n附件：{}", names.join("、"))?;
    }
    jsonl(
        lines,
        &json!({
            "type": "message", "id": msg.id, "role": msg.role, "created_at": msg.time,
            "text": text, "attachments": attachments,
        }),
    )
}

struct Tool {
    kind: String,
    ordinal: i64,
    status: String,
    time: Option<String>,
    summary: String,
    details: serde_json::Map<String, Value>,
}

struct TurnItems {
    message_ordinals: BTreeMap<String, i64>,
    tools: VecDeque<Tool>,
}

fn turn_items(conn: &Connection, thread: &str) -> Result<TurnItems> {
    let mut stmt = conn.prepare(
        "SELECT ordinal, type, status, payload_json
         FROM orchestration_v2_projection_turn_items WHERE thread_id = ?1
         AND type <> 'reasoning' ORDER BY ordinal",
    )?;
    let mut rows = stmt.query([thread])?;
    let mut message_ordinals = BTreeMap::new();
    let mut tools = VecDeque::new();
    while let Some(row) = rows.next()? {
        let ordinal: i64 = row.get(0)?;
        let kind: String = row.get(1)?;
        let raw: String = row.get(3)?;
        let body = payload(&raw, "对话步骤")?;
        if matches!(kind.as_str(), "user_message" | "assistant_message") {
            if let Some(id) = body["messageId"].as_str() {
                message_ordinals.entry(id.to_owned()).or_insert(ordinal);
            }
            continue;
        }
        let time = ["startedAt", "completedAt", "updatedAt"]
            .into_iter()
            .find_map(|key| body[key].as_str())
            .map(str::to_owned);
        let status: String = row.get(2)?;
        let (summary, details) = tool_summary(&kind, &status, &body);
        tools.push_back(Tool {
            kind,
            ordinal,
            status,
            time,
            summary,
            details,
        });
    }
    Ok(TurnItems {
        message_ordinals,
        tools,
    })
}

fn preview(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(|value| value.chars().take(300).collect())
}

fn output_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn tool_summary(
    kind: &str,
    raw_status: &str,
    body: &Value,
) -> (String, serde_json::Map<String, Value>) {
    let mut parts = Vec::new();
    let mut details = serde_json::Map::new();
    let status = match raw_status {
        "idle" => "空闲",
        "pending" => "待处理",
        "running" => "运行中",
        "waiting" => "等待中",
        "completed" => "完成",
        "failed" => "失败",
        "cancelled" => "取消",
        "interrupted" => "中断",
        _ => "未知状态",
    };
    let label = match kind {
        "command_execution" => {
            parts.push(status.to_owned());
            if let Some(code) = body["exitCode"].as_i64() {
                parts.push(format!("退出码 {code}"));
                details.insert("exit_code".to_owned(), json!(code));
            }
            if let Some(input) = preview(&body["input"]) {
                parts.push(format!("输入：{}", one_line(&input)));
                details.insert("input".to_owned(), json!(input));
            }
            if let Some(output) = body["output"].as_str() {
                parts.push(format!("输出 {}", output_size(output.len())));
                details.insert("output_bytes".to_owned(), json!(output.len()));
            }
            "命令"
        }
        "file_change" => {
            if let Some(name) = body["fileName"].as_str() {
                parts.push(one_line(name));
                details.insert("file_name".to_owned(), json!(name));
            }
            let additions = body["additions"].as_u64();
            let deletions = body["deletions"].as_u64();
            let mut changes = Vec::new();
            if let Some(n) = additions {
                changes.push(format!("+{n}"));
                details.insert("additions".to_owned(), json!(n));
            }
            if let Some(n) = deletions {
                changes.push(format!("−{n}"));
                details.insert("deletions".to_owned(), json!(n));
            }
            if !changes.is_empty() {
                parts.push(changes.join(" "));
            }
            if raw_status != "completed" {
                parts.push(status.to_owned());
            }
            "文件修改"
        }
        "file_search" | "web_search" => {
            parts.push(status.to_owned());
            if let Some(results) = body["results"].as_array() {
                parts.push(format!("{} 条结果", results.len()));
                details.insert("result_count".to_owned(), json!(results.len()));
            }
            if kind == "file_search" {
                "文件搜索"
            } else {
                "网页搜索"
            }
        }
        "dynamic_tool" => {
            if let Some(name) = preview(&body["toolName"]) {
                parts.push(one_line(&name));
                details.insert("tool_name".to_owned(), json!(name));
            }
            parts.push(status.to_owned());
            "动态工具"
        }
        other => {
            parts.push(status.to_owned());
            match other {
                "subagent" => "子代理",
                "todo_list" => "待办列表",
                "proposed_plan" => "计划草案",
                "approval_request" => "审批请求",
                "user_input_request" => "用户输入请求",
                "error" => "错误",
                "compaction" => "上下文压缩",
                "checkpoint" => "检查点",
                "run_interrupt_request" => "中断请求",
                "run_interrupt_result" => "中断结果",
                "system_notice" | "notification" => "系统通知",
                "handoff" => "交接",
                "fork" => "分支会话",
                "thread_created" => "创建会话",
                "secret_request" => "机密请求",
                "link_request" => "链接请求",
                _ => "其他步骤",
            }
        }
    };
    let suffix = if parts.is_empty() {
        String::new()
    } else {
        format!(" · {}", parts.join(" · "))
    };
    (format!("{label}{suffix}"), details)
}

fn write_tool(markdown: &mut impl Write, lines: &mut impl Write, tool: &Tool) -> Result<()> {
    writeln!(markdown, "\n> 工具：{}", markdown_text(&tool.summary))?;
    jsonl(
        lines,
        &json!({
            "type": "tool", "kind": tool.kind, "ordinal": tool.ordinal, "status": tool.status,
            "summary": tool.summary, "details": tool.details,
        }),
    )
}

fn write_thread(
    conn: &Connection,
    root: &Path,
    relative: &str,
    thread: &ArchiveThread,
    project: &ArchiveProject,
) -> Result<()> {
    let stem = root.join(relative);
    fs::create_dir_all(stem.parent().context("对话归档缺少父目录")?)?;
    let mut markdown = NewFile::open(root.join(format!("{relative}.md")))?;
    let mut lines = NewFile::open(root.join(format!("{relative}.jsonl")))?;
    let raw: String = conn.query_row(
        "SELECT payload_json FROM orchestration_v2_projection_threads WHERE thread_id = ?1",
        [&thread.id],
        |row| row.get(0),
    )?;
    let header = payload(&raw, "对话信息")?;
    let model = header["modelSelection"]["model"].as_str();
    let md = markdown.writer();
    let jl = lines.writer();
    writeln!(md, "# {}\n", markdown_text(&thread.title))?;
    for (label, value) in [
        ("项目", project.title.as_str()),
        ("项目路径", &project.path),
        ("模型", model.unwrap_or("未知")),
        ("供应商", &thread.provider),
        ("创建时间", &thread.created_at),
        ("更新时间", &thread.updated_at),
        ("已归档", if thread.archived { "是" } else { "否" }),
        ("对话 ID", &thread.id),
    ] {
        writeln!(md, "- {label}：{}", markdown_text(value))?;
    }
    let mut metadata = serde_json::to_value(thread)?;
    metadata["type"] = json!("thread");
    metadata["model"] = json!(model);
    metadata["project_title"] = json!(project.title);
    metadata["project_path"] = json!(project.path);
    jsonl(jl, &metadata)?;

    let TurnItems {
        message_ordinals,
        mut tools,
    } = turn_items(conn, &thread.id)?;
    for msg in messages(conn, &thread.id)? {
        // Use the original turn item as an anchor. Messages with no corresponding
        // item (e.g. a system message) are placed by the tool's persisted time.
        let anchor = message_ordinals.get(&msg.id);
        while tools.front().is_some_and(|tool| match anchor {
            Some(ordinal) => tool.ordinal < *ordinal,
            None => tool.time.as_ref().is_some_and(|time| time <= &msg.time),
        }) {
            if let Some(tool) = tools.pop_front() {
                write_tool(md, jl, &tool)?;
            }
        }
        write_message(md, jl, &msg)?;
    }
    for tool in tools {
        write_tool(md, jl, &tool)?;
    }
    markdown.finish()?;
    lines.finish()?;
    markdown.keep = true;
    lines.keep = true;
    Ok(())
}

fn thread_stem(thread: &ArchiveThread, project_slug: &str) -> Result<String> {
    let date = thread.created_at.get(..10).context("缺少对话创建日期")?;
    ensure!(
        date.bytes().enumerate().all(|(i, b)| if i == 4 || i == 7 {
            b == b'-'
        } else {
            b.is_ascii_digit()
        }),
        "对话创建日期格式无效"
    );
    let id = slug(&thread.id.chars().take(8).collect::<String>());
    Ok(format!(
        "threads/{project_slug}/{date}-{}-{id}",
        slug(&thread.title)
    ))
}

struct IndexEntry<'a> {
    thread: &'a ArchiveThread,
    file: Option<String>,
}

fn write_index(folder: &Path, groups: BTreeMap<(&str, &str), Vec<IndexEntry<'_>>>) -> Result<()> {
    write_new(folder.join("index.md"), |out| {
        writeln!(out, "# T3 Code 对话归档")?;
        for ((title, _), entries) in groups {
            writeln!(out, "\n## {}\n", markdown_text(title))?;
            for IndexEntry { thread, file } in entries {
                let title = markdown_text(&thread.title);
                let updated = markdown_text(&thread.updated_at);
                let archived = if thread.archived { " · 已归档" } else { "" };
                if let Some(file) = file {
                    let encoded = link_path(&file);
                    writeln!(
                        out,
                        "- [{title}]({encoded}.md) · [JSONL]({encoded}.jsonl) · {updated}{archived}"
                    )?;
                } else {
                    writeln!(out, "- {title} · {updated}{archived} · 导出失败")?;
                }
            }
        }
        Ok(())
    })
}

pub fn export_threads(
    t3_folder: &Path,
    archives_root: &Path,
    stop: &AtomicBool,
    progress: &dyn Fn(Progress),
) -> Result<Report> {
    let source = open_source(t3_folder)?;
    let conn = &source.conn;
    let schema = schema(conn)?;
    let projects = source_projects(conn, &schema)?;
    let threads = source_threads(conn, &schema)?;
    let slugs = project_slugs(&projects);
    let by_id: BTreeMap<_, _> = projects.iter().map(|p| (p.id.as_str(), p)).collect();
    let folder = new_archive(conn, archives_root, t3_folder)?;
    let mut report = Report {
        folder,
        exported: 0,
        total: threads.len(),
        failed: Vec::new(),
        stopped: false,
    };
    write_new(report.folder.join("projects.json"), |out| {
        serde_json::to_writer_pretty(&mut *out, &projects)?;
        out.write_all(b"\n")?;
        Ok(())
    })?;

    let mut groups = BTreeMap::<_, Vec<_>>::new();
    for (index, thread) in threads.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            report.stopped = true;
            break;
        }
        let project = by_id[thread.project_id.as_str()];
        let result = (|| {
            let file = thread_stem(thread, &slugs[project.id.as_str()])?;
            write_thread(conn, &report.folder, &file, thread, project)?;
            Ok::<_, anyhow::Error>(file)
        })();
        let file = match result {
            Ok(file) => {
                report.exported += 1;
                Some(file)
            }
            Err(reason) => {
                report
                    .failed
                    .push((thread.id.clone(), format!("{reason:#}")));
                None
            }
        };
        groups
            .entry((project.title.as_str(), project.id.as_str()))
            .or_default()
            .push(IndexEntry { thread, file });
        progress(Progress {
            done: index + 1,
            total: report.total,
        });
    }
    write_index(&report.folder, groups)?;
    if !report.failed.is_empty() {
        write_new(report.folder.join("errors.txt"), |out| {
            for (id, reason) in &report.failed {
                writeln!(out, "{}\t{}", one_line(id), one_line(reason))?;
            }
            Ok(())
        })?;
    }
    Ok(report)
}
