//! Pluggable engine services: project extensions (MCP/skills, `SA_TOOL`) and
//! attachment snapshots. Real MCP lands later; tests supply fakes.

use anyhow::{Context, Result, bail, ensure};
use futures::future::BoxFuture;
use rusqlite::Connection;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
};
use tempfile::NamedTempFile;
use tokio_util::sync::CancellationToken;

/// Approval callback handed to extensions (`store.approve(ctx, action)`).
pub type ApproveFn = Arc<dyn Fn(Value) -> BoxFuture<'static, bool> + Send + Sync>;

/// Arguments of one `SA_TOOL` invocation (D:202-213).
#[derive(Clone)]
pub struct InvokeArgs {
    pub ticket: i64,
    pub job: i64,
    pub turn: i64,
    pub admission: i64,
    pub logical_turn: i64,
    pub profile: String,
    pub cwd: String,
    pub cancel: CancellationToken,
    pub approve: ApproveFn,
    /// The validated `SA_TOOL` object `{id, server, tool, arguments}`.
    pub command: Value,
}

#[derive(Clone, Debug, Default)]
pub struct InvokeResult {
    /// A new durable receipt was written; the job must take another turn.
    pub new_receipt: bool,
}

/// Project extension service (`runtime/extensions.cjs` surface used by the
/// engine). Implementations own their tables; `bind` runs inside the ticket
/// creation transaction.
pub trait Extensions: Send + Sync {
    /// `prepare(params.extensions, dir)` before the create transaction.
    fn prepare(&self, params: Option<Value>, dir: &Path) -> BoxFuture<'static, Result<Value>>;
    /// `bind(ticket, prepared)` inside the create transaction.
    fn bind(&self, conn: &Connection, ticket: i64, prepared: &Value) -> Result<()>;
    /// `context(ticket, profile)` → `{skills, mcp}`.
    fn context(&self, ticket: i64, profile: &str) -> Value;
    /// `results(job)` → receipts for `mcp_results` (roots only).
    fn results(&self, _job: i64) -> Vec<Value> {
        vec![]
    }
    /// Whether `invoke` is available (`store.extensions?.invoke`).
    fn can_invoke(&self) -> bool {
        true
    }
    /// Execute one tool call with durable receipts (X:289-386).
    fn invoke(&self, args: InvokeArgs) -> BoxFuture<'static, Result<InvokeResult>>;
}

/// Extension service with no skills, no MCP servers and no tools.
pub struct NoopExtensions;

impl Extensions for NoopExtensions {
    fn prepare(&self, params: Option<Value>, _dir: &Path) -> BoxFuture<'static, Result<Value>> {
        Box::pin(async move { Ok(params.unwrap_or(Value::Null)) })
    }
    fn bind(&self, _conn: &Connection, _ticket: i64, _prepared: &Value) -> Result<()> {
        Ok(())
    }
    fn context(&self, _ticket: i64, _profile: &str) -> Value {
        json!({"skills": [], "mcp": []})
    }
    fn can_invoke(&self) -> bool {
        false
    }
    fn invoke(&self, _args: InvokeArgs) -> BoxFuture<'static, Result<InvokeResult>> {
        Box::pin(async { bail!("当前工单或执行者不可调用 MCP 工具") })
    }
}

/// Immutable attachment copies (`native snapshot`, native/src/project.rs).
pub trait Snapshotter: Send + Sync {
    /// Returns the manifest `[{name, bytes, sha256, object_path}]`.
    fn snapshot(
        &self,
        project: PathBuf,
        names: Vec<String>,
        directory: PathBuf,
    ) -> BoxFuture<'static, Result<Value>>;
}

/// Filesystem snapshotter ported from `native/src/project.rs::snapshot`.
/// The Windows handle re-check (`GetFinalPathNameByHandleW`) is not ported;
/// containment relies on `canonicalize`.
pub struct FsSnapshotter;

impl Snapshotter for FsSnapshotter {
    fn snapshot(
        &self,
        project: PathBuf,
        names: Vec<String>,
        directory: PathBuf,
    ) -> BoxFuture<'static, Result<Value>> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || snapshot_files(&project, &names, &directory))
                .await
                .context("快照任务异常")?
        })
    }
}

fn snapshot_files(project: &Path, names: &[String], directory: &Path) -> Result<Value> {
    ensure!(names.len() <= 16, "单次最多附加 16 个文件");
    let root = project.canonicalize().context("附件项目目录无法读取")?;
    fs::create_dir_all(directory)?;
    let mut total = 0usize;
    let mut manifest = vec![];
    for name in names {
        let rel = Path::new(name);
        ensure!(
            rel.components()
                .all(|p| matches!(p, Component::Normal(_) | Component::CurDir))
                && !name.contains(':'),
            "只接受项目内相对路径"
        );
        let path = root.join(rel).canonicalize().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!(
                    "附件不存在或链接目标缺失：{name}。SA_SEND.files 仅接受已存在的输入附件；新建文件的输出路径请写在 body 中，不要加入 files。"
                )
            } else {
                anyhow::anyhow!("附件路径无法解析：{name}（{error}）")
            }
        })?;
        ensure!(
            path.starts_with(&root) && path.is_file(),
            "附件位于项目外或并非普通文件"
        );
        let mut src = File::open(&path)?;
        let mut tmp = NamedTempFile::new_in(directory)?;
        let mut hash = Sha256::new();
        let mut size = 0usize;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = src.read(&mut buf)?;
            if n == 0 {
                break;
            }
            total += n;
            if total > 64 * 1024 * 1024 {
                bail!("附件总量超过 64 MiB");
            }
            hash.update(&buf[..n]);
            tmp.write_all(&buf[..n])?;
            size += n;
        }
        let key = format!("{:x}", hash.finalize());
        let dest = directory.join(&key);
        tmp.as_file().sync_all()?;
        tmp.persist(&dest).context("无法提交文件快照")?;
        manifest.push(json!({"name":name,"bytes":size,"sha256":key,"object_path":dest}));
    }
    Ok(json!(manifest))
}

/// `fs.realpathSync` without the Windows verbatim (`\\?\`) prefix.
pub fn realpath(p: &Path) -> Result<PathBuf> {
    let real = p.canonicalize()?;
    Ok(strip_verbatim(real))
}

pub fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\")
        && rest.as_bytes().get(1) == Some(&b':')
    {
        return PathBuf::from(rest);
    }
    p
}
