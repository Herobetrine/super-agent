//! Isolated writer workspaces and validation-gated local integration.
//! Git commands use argv, never a shell. No push, force reset or forced cleanup.
use crate::db::{Q, int_of, str_of};
use crate::protocol::digest;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

mod process;

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS job_directories(job INTEGER PRIMARY KEY,cwd TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS workspace_policy(ticket INTEGER PRIMARY KEY,isolated INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS job_workspaces(job INTEGER PRIMARY KEY,ticket INTEGER NOT NULL,source TEXT NOT NULL,path TEXT NOT NULL,branch TEXT NOT NULL,base TEXT NOT NULL DEFAULT '',head TEXT NOT NULL DEFAULT '',state TEXT NOT NULL DEFAULT 'planned',validation TEXT NOT NULL DEFAULT '{}',detail TEXT NOT NULL DEFAULT '');
CREATE TABLE IF NOT EXISTS integration_policy(ticket INTEGER PRIMARY KEY,enabled INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS integration_queue(job INTEGER PRIMARY KEY,ticket INTEGER NOT NULL,turn INTEGER NOT NULL,
 state TEXT NOT NULL,attempts INTEGER NOT NULL DEFAULT 0,version TEXT NOT NULL,commands TEXT NOT NULL,
 detail TEXT NOT NULL DEFAULT '',validation TEXT NOT NULL DEFAULT '{}');";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Workspace {
    pub job: i64,
    pub ticket: i64,
    pub source: String,
    pub path: String,
    pub branch: String,
    pub base: String,
    pub head: String,
    pub state: String,
    pub validation: serde_json::Value,
    pub detail: String,
}

pub fn get(q: &Q, job: i64) -> Result<Option<Workspace>> {
    q.one("SELECT * FROM job_workspaces WHERE job=?", &[&job])?
        .map(|r| {
            Ok(Workspace {
                job: int_of(&r, "job"),
                ticket: int_of(&r, "ticket"),
                source: str_of(&r, "source").into(),
                path: str_of(&r, "path").into(),
                branch: str_of(&r, "branch").into(),
                base: str_of(&r, "base").into(),
                head: str_of(&r, "head").into(),
                state: str_of(&r, "state").into(),
                detail: str_of(&r, "detail").into(),
                validation: serde_json::from_str(str_of(&r, "validation"))?,
            })
        })
        .transpose()
}

pub fn plan(q: &Q, data: &Path, job: i64, ticket: i64, source: &str) -> Result<()> {
    let namespace = &digest(&data.to_string_lossy())[..12];
    let path = data.join("workspaces").join(format!("job-{job}"));
    let branch = format!("codex/sa-{namespace}-{job}");
    q.run(
        "INSERT INTO job_workspaces(job,ticket,source,path,branch) VALUES(?,?,?,?,?)",
        &[
            &job,
            &ticket,
            &source,
            &path.to_string_lossy().as_ref(),
            &branch,
        ],
    )?;
    Ok(())
}

/// Path overlap, including nested projects, is a conflict. Windows paths are
/// case insensitive; canonicalize before use when the directory exists.
pub fn scope(path: &str) -> String {
    let p = crate::services::realpath(Path::new(path)).unwrap_or_else(|_| PathBuf::from(path));
    let s = p
        .to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();
    if cfg!(windows) { s.to_lowercase() } else { s }
}
pub fn overlaps(a: &str, b: &str) -> bool {
    a == b
        || a.strip_prefix(b).is_some_and(|s| s.starts_with('/'))
        || b.strip_prefix(a).is_some_and(|s| s.starts_with('/'))
}

/// File-backed output prevents child pipe deadlocks. A bounded command cannot
/// leave a validation process tree running indefinitely.
fn command(cwd: &Path, argv: &[String], timeout: Duration) -> Result<(bool, Vec<u8>)> {
    let result = run_command(cwd, argv, timeout, None, false, Some(1024 * 1024))?;
    ensure!(!result.timed_out, "命令超时，已终止本次验证；变更未合入");
    Ok((result.ok, result.output))
}

pub(crate) struct CommandOutput {
    pub ok: bool,
    pub exit: Option<i32>,
    pub output: Vec<u8>,
    pub cancelled: bool,
    pub timed_out: bool,
}

/// Completion checks need an exit code and a combined tail even on timeout.
/// Existing validation callers retain their stdout and timeout-error contract.
pub(crate) fn command_with_cancel(
    cwd: &Path, argv: &[String], timeout: Duration, cancel: Option<&dyn Fn() -> bool>,
) -> Result<CommandOutput> {
    run_command(cwd, argv, timeout, cancel, true, Some(4000))
}

/// Delivery checks can emit arbitrary amounts of output. Drain one combined
/// pipe continuously and retain only its tail, including on timeout.
pub(crate) fn delivery_command(
    cwd: &Path, argv: &[String], env: &[(std::ffi::OsString, std::ffi::OsString)],
    timeout: Duration, cancel: &dyn Fn() -> bool,
) -> Result<CommandOutput> {
    use std::{collections::VecDeque, io::Read, sync::mpsc};
    const TAIL: usize = 4000;
    let (mut reader, writer) = std::io::pipe()?;
    let mut command = check_command(cwd, argv)?;
    command.env_clear().envs(env.iter().map(|(key, value)| (key, value)))
        .stdout(writer.try_clone()?).stderr(writer);
    let (send, receive) = mpsc::sync_channel(1);
    std::thread::Builder::new().name("delivery-check-output".into()).spawn(move || {
        let result = (|| -> std::io::Result<Vec<u8>> {
            let mut tail = VecDeque::with_capacity(TAIL);
            let mut chunk = [0; 8192];
            loop {
                let n = reader.read(&mut chunk)?;
                if n == 0 { return Ok(tail.into()); }
                let keep = &chunk[n.saturating_sub(TAIL)..n];
                tail.drain(..(tail.len() + keep.len()).saturating_sub(TAIL));
                tail.extend(keep);
            }
        })();
        let _ = send.send(result);
    })?;
    let mut result = wait_command(&mut command, timeout, Some(cancel))?;
    drop(command);
    result.output = receive.recv_timeout(Duration::from_secs(5))??;
    Ok(result)
}

fn check_command(cwd: &Path, argv: &[String]) -> Result<Command> {
    ensure!(!argv.is_empty() && !argv[0].trim().is_empty(), "验证命令不能为空");
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).current_dir(cwd).stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR").env_remove("GIT_WORK_TREE").env_remove("GIT_INDEX_FILE");
    Ok(cmd)
}

fn run_command(
    cwd: &Path, argv: &[String], timeout: Duration, cancel: Option<&dyn Fn() -> bool>, combined: bool, limit: Option<u64>,
) -> Result<CommandOutput> {
    let output = tempfile::NamedTempFile::new()?;
    let errors = tempfile::NamedTempFile::new()?;
    let stdout = output.reopen()?;
    // A cloned handle shares the file offset, preserving stdout/stderr order.
    let stderr = if combined { stdout.try_clone()? } else { errors.reopen()? };
    let mut cmd = check_command(cwd, argv)?;
    cmd.stdout(stdout).stderr(stderr);
    let mut result = wait_command(&mut cmd, timeout, cancel)?;
    use std::io::{Read, Seek, SeekFrom};
    let mut reader = output.reopen()?;
    if combined {
        let len = reader.metadata()?.len();
        reader.seek(SeekFrom::Start(len.saturating_sub(4000)))?;
    }
    reader.take(limit.unwrap_or(u64::MAX)).read_to_end(&mut result.output)?;
    Ok(result)
}

fn wait_command(cmd: &mut Command, timeout: Duration, cancel: Option<&dyn Fn() -> bool>) -> Result<CommandOutput> {
    if cancel.is_some_and(|check| check()) {
        return Ok(CommandOutput { ok: false, exit: None, output: vec![], cancelled: true, timed_out: false });
    }
    let mut child = process::OwnedProcess::spawn(cmd)?;
    let start = Instant::now();
    let mut cancelled = false;
    let mut timed_out = false;
    let status = loop {
        if cancel.is_some_and(|check| check()) {
            cancelled = true;
            break None;
        }
        if let Some(status) = child.try_wait()? { break Some(status); }
        if start.elapsed() >= timeout {
            timed_out = true;
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    child.shutdown()?;
    Ok(CommandOutput {
        ok: status.is_some_and(|status| status.success()),
        exit: status.and_then(|status| status.code()),
        output: vec![], cancelled, timed_out,
    })
}

/// No fingerprint is available outside a work tree or before its first commit.
/// Hash complete raw Git outputs with framing. `--binary` covers binary edits
/// to tracked files; untracked files are not in any Git output, so their
/// contents are hashed directly. Otherwise editing a new, uncommitted file
/// would look like an unchanged tree and reuse a stale pass.
pub(crate) fn fingerprint(cwd: &Path, cancel: &dyn Fn() -> bool) -> Result<Option<String>> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    /// Larger untracked files are identified by size and modification time.
    const CONTENT_LIMIT: u64 = 16 * 1024 * 1024;
    let mut hash = Sha256::new();
    let mut untracked = vec![];
    for (index, args) in [
        vec!["rev-parse", "HEAD"],
        vec!["status", "--porcelain=v1", "--untracked-files=all"],
        vec!["diff", "--binary", "HEAD"],
        vec!["ls-files", "--others", "--exclude-standard", "-z"],
    ].iter().enumerate() {
        let argv = git_argv(args);
        let result = run_command(cwd, &argv, Duration::from_secs(60), Some(cancel), false, None)?;
        ensure!(!result.cancelled, "任务已中止");
        if !result.ok { return Ok(None); }
        hash.update((result.output.len() as u64).to_le_bytes());
        hash.update(&result.output);
        if index == 3 { untracked = result.output; }
    }
    for name in untracked.split(|byte| *byte == 0).filter(|name| !name.is_empty()) {
        ensure!(!cancel(), "任务已中止");
        let path = cwd.join(String::from_utf8_lossy(name).as_ref());
        // A file removed or replaced by a link while we look still changes the hash.
        let Ok(meta) = fs::symlink_metadata(&path) else {
            hash.update([0u8]);
            continue;
        };
        hash.update(meta.len().to_le_bytes());
        if !meta.is_file() {
            hash.update([1u8]);
        } else if meta.len() > CONTENT_LIMIT {
            let modified = meta.modified().ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|age| age.as_nanos()).unwrap_or(0);
            hash.update([2u8]);
            hash.update(modified.to_le_bytes());
        } else {
            hash.update([3u8]);
            let mut file = fs::File::open(&path)?;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let read = file.read(&mut buffer)?;
                if read == 0 { break; }
                hash.update(&buffer[..read]);
            }
        }
    }
    Ok(Some(format!("{:x}", hash.finalize())))
}

pub(crate) fn git_argv(args: &[&str]) -> Vec<String> {
    [
        "git",
        "-c",
        "core.hooksPath=",
        "-c",
        "gc.auto=0",
        "-c",
        "maintenance.auto=false",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "user.name=Super Agent",
        "-c",
        "user.email=super-agent@localhost",
    ]
    .into_iter()
    .chain(args.iter().copied())
    .map(str::to_string)
    .collect()
}

pub(crate) fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let argv = git_argv(args);
    let (ok, out) = command(cwd, &argv, Duration::from_secs(60))?;
    ensure!(ok, "Git 操作失败；工作区保留，未强制重置或删除");
    Ok(String::from_utf8(out)?.trim().to_string())
}

pub(crate) fn clean(root: &Path) -> Result<()> {
    ensure!(
        git(root, &["status", "--porcelain=v1", "--untracked-files=all"])?.is_empty(),
        "工作区有未提交修改；请先保存或提交再执行"
    );
    Ok(())
}
pub(crate) fn head(root: &Path) -> Result<String> {
    git(root, &["rev-parse", "--verify", "HEAD"])
}
fn root(root: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(git(root, &["rev-parse", "--show-toplevel"])?))
}

pub fn identity(w: &Workspace) -> Result<Workspace> {
    let mut w = w.clone();
    let source = root(Path::new(&w.source))?;
    ensure!(
        scope(&source.to_string_lossy()) == scope(&w.source),
        "隔离任务需要选择 Git 仓库根目录"
    );
    let current = json!({"source_real":scope(&w.source),"source_ref":git(&source,&["symbolic-ref","HEAD"])?,"common_dir":git(&source,&["rev-parse","--path-format=absolute","--git-common-dir"])?});
    if w.validation.get("identity").is_some() {
        ensure!(
            w.validation["identity"] == current,
            "原仓库分支或身份已改变，请人工核对工作区"
        );
    } else {
        w.validation["identity"] = current;
    }
    Ok(w)
}

pub(crate) fn verify(w: &Workspace) -> Result<()> {
    ensure!(
        w.validation.get("identity").is_some(),
        "工作区缺少身份记录，请先重新准备"
    );
    identity(w)?;
    let path = Path::new(&w.path);
    let literal = w.path.replace('\\', "/").trim_end_matches('/').to_string();
    let literal = if cfg!(windows) {
        literal.to_lowercase()
    } else {
        literal
    };
    ensure!(scope(&w.path) == literal, "工作区路径被重定向");
    ensure!(
        scope(&root(path)?.to_string_lossy()) == literal,
        "工作区根目录不匹配"
    );
    ensure!(
        git(path, &["symbolic-ref", "--short", "HEAD"])? == w.branch,
        "工作区分支不匹配"
    );
    ensure!(
        git(
            path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"]
        )? == w.validation["identity"]["common_dir"],
        "工作区仓库身份不匹配"
    );
    Ok(())
}

pub fn provision(w: &Workspace, data: &Path) -> Result<Workspace> {
    let mut w = identity(w)?;
    let source = Path::new(&w.source);
    let dest = PathBuf::from(&w.path);
    let home = data.join("workspaces");
    fs::create_dir_all(&home)?;
    ensure!(
        dest.parent() == Some(home.as_path()) && dest.file_name().is_some(),
        "工作区位置无效"
    );
    if dest.exists() {
        ensure!(
            scope(&root(&dest)?.to_string_lossy()) == scope(&w.path),
            "工作区路径与记录不一致"
        );
        ensure!(
            git(&dest, &["symbolic-ref", "--short", "HEAD"])? == w.branch,
            "工作区分支与记录不一致"
        );
        ensure!(
            git(
                &dest,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"]
            )? == git(
                source,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"]
            )?,
            "工作区不属于原仓库"
        );
        // A crash between git worktree add and receipt persistence is recoverable.
        if w.base.is_empty() {
            w.base = head(&dest)?;
        }
    } else {
        ensure!(
            w.state == "planned",
            "已记录工作区缺失；请恢复目录，不能静默重新创建"
        );
        clean(source)?;
        w.base = head(source)?;
        git(
            source,
            &["worktree", "add", "-b", &w.branch, &w.path, &w.base],
        )?;
    }
    w.head = head(&dest)?;
    verify(&w)?;
    if w.state == "planned" {
        w.state = "ready".into();
    }
    Ok(w)
}

pub fn save(q: &Q, w: &Workspace) -> Result<()> {
    q.run(
        "UPDATE job_workspaces SET base=?,head=?,state=?,validation=?,detail=? WHERE job=?",
        &[
            &w.base,
            &w.head,
            &w.state,
            &w.validation.to_string(),
            &w.detail,
            &w.job,
        ],
    )?;
    Ok(())
}

pub(crate) fn validate_command(argv: &[String]) -> Result<()> {
    ensure!(
        !argv.is_empty()
            && !argv[0].trim().is_empty()
            && argv.len() <= 64
            && argv.iter().all(|s| s.len() < 8192 && !s.contains('\0')),
        "验证命令需要 JSON 参数数组，最多 64 项"
    );
    Ok(())
}

/// Operator-triggered checkpoint commit and test. Validation refers to the
/// exact resulting Git tree; commands that modify it invalidate their receipt.
pub fn validate(w: &Workspace, argv: &[String]) -> Result<Workspace> {
    validate_command(argv)?;
    ensure!(
        !matches!(w.state.as_str(), "planned" | "merged"),
        "工作区当前不可验证"
    );
    let mut w = w.clone();
    let path = Path::new(&w.path);
    verify(&w)?;
    ensure!(
        git(path, &["symbolic-ref", "--short", "HEAD"])? == w.branch,
        "工作区分支已改变"
    );
    git(path, &["add", "--all", "--", "."])?;
    if !git(path, &["diff", "--cached", "--name-only"])?.is_empty() {
        git(
            path,
            &["commit", "-m", "Checkpoint isolated Super Agent task"],
        )?;
    }
    clean(path)?;
    let before = head(path)?;
    let started = Instant::now();
    let (ok, out) = command(path, argv, Duration::from_secs(600))?;
    let unchanged = head(path)? == before && clean(path).is_ok();
    w.head = before;
    w.state = if ok && unchanged {
        "validated"
    } else {
        "failed"
    }
    .into();
    w.detail = if !unchanged {
        "验证命令改变了代码，需重新验证"
    } else if !ok {
        "验证命令失败"
    } else {
        "验证通过，尚未合入"
    }
    .into();
    let prerequisite_heads = w.validation.get("prerequisite_heads").cloned();
    w.validation = json!({"identity":w.validation["identity"],"command":argv,"head":w.head,"passed":ok && unchanged,"elapsed_ms":started.elapsed().as_millis() as u64,"output_sha256":digest(&String::from_utf8_lossy(&out))});
    if let Some(heads) = prerequisite_heads { w.validation["prerequisite_heads"] = heads; }
    Ok(w)
}

/// Build a merge candidate in a separate worktree, validate it, then publish
/// via fast-forward. Failed candidates are retained for inspection.
pub fn integrate(w: &Workspace, data: &Path) -> Result<Workspace> {
    ensure!(
        w.state == "validated" && w.validation["passed"] == true,
        "先验证该任务的当前版本"
    );
    let argv: Vec<String> = serde_json::from_value(w.validation["command"].clone())?;
    validate_command(&argv)?;
    let source = Path::new(&w.source);
    let path = Path::new(&w.path);
    verify(w)?;
    clean(path)?;
    clean(source)?;
    ensure!(
        head(path)? == w.head && w.validation["head"] == w.head,
        "验证后代码已变化，请重新验证"
    );
    let baseline = head(source)?;
    let directory = data.join("integrations");
    fs::create_dir_all(&directory)?;
    let candidate = tempfile::Builder::new()
        .prefix(&format!("job-{}-", w.job))
        .tempdir_in(&directory)?
        .keep();
    let candidate_text = candidate.to_string_lossy();
    git(
        source,
        &["worktree", "add", "--detach", &candidate_text, &baseline],
    )?;
    let mut result = w.clone();
    result.validation["candidate"] = json!(candidate_text);
    if git(&candidate, &["merge", "--no-ff", "--no-edit", &w.head]).is_err() {
        result.state = "conflict".into();
        result.detail = "合并冲突；原项目未改变，候选工作区已保留".into();
        return Ok(result);
    }
    let merged = head(&candidate)?;
    let (ok, out) = command(&candidate, &argv, Duration::from_secs(600))?;
    result.validation["integration_output_sha256"] = json!(digest(&String::from_utf8_lossy(&out)));
    if !ok || head(&candidate)? != merged || clean(&candidate).is_err() {
        result.state = "failed".into();
        result.detail = "合并候选未通过验证；原项目未改变".into();
        return Ok(result);
    }
    clean(source)?;
    verify(w)?;
    ensure!(
        head(source)? == baseline,
        "验证期间原项目有新提交，请重新验证后合入"
    );
    // Git itself refuses overwriting conflicting local changes; no hard reset.
    git(
        source,
        &["merge", "--no-overwrite-ignore", "--ff-only", &merged],
    )?;
    result.state = "merged".into();
    result.detail = "合并候选验证通过，已本地合入".into();
    result.validation["integrated_head"] = json!(merged);
    let _ = git(source, &["worktree", "remove", &candidate_text]);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_git(path: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(["-c", "user.name=Fixture", "-c", "user.email=fixture@localhost", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    /// The completion gate reuses a pass only for an identical tree, so the
    /// fingerprint must move when an uncommitted new file is edited again.
    #[test]
    fn fingerprint_covers_untracked_and_binary_content() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(fingerprint(root, &|| false).unwrap(), None);
        fixture_git(root, &["init"]);
        fixture_git(root, &["config", "core.autocrlf", "false"]);
        fs::write(root.join("base.txt"), "base\n").unwrap();
        fs::write(root.join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
        fixture_git(root, &["add", "."]);
        fixture_git(root, &["commit", "-m", "fixture"]);
        let print = || fingerprint(root, &|| false).unwrap().expect("git work tree");
        let clean = print();
        assert_eq!(clean, print());

        fs::write(root.join("new.txt"), "first\n").unwrap();
        let created = print();
        assert_ne!(created, clean);
        // Same path and same length, different bytes.
        fs::write(root.join("new.txt"), "other\n").unwrap();
        let edited = print();
        assert_ne!(edited, created);
        fs::write(root.join("new.txt"), "first\n").unwrap();
        assert_eq!(print(), created);
        fs::remove_file(root.join("new.txt")).unwrap();
        assert_eq!(print(), clean);

        fs::write(root.join("blob.bin"), [0u8, 1, 2, 4]).unwrap();
        let binary = print();
        assert_ne!(binary, clean);
        fs::write(root.join("blob.bin"), [0u8, 1, 2, 5]).unwrap();
        assert_ne!(print(), binary);
    }
}
