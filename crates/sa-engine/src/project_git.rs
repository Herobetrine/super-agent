//! Local Git bootstrap for working directories, serialized across processes.
use crate::workspaces::{CommandOutput, command_with_cancel, git_argv};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const GIT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Bootstrap {
    pub root: String,
    pub initialized: bool,
    pub initial_commit: Option<String>,
}

pub fn ensure(cwd: &Path) -> Result<Bootstrap> {
    ensure_with_cancel(cwd, &|| false)
}

/// Existing committed repositories remain untouched, including their index.
/// Initialization may leave an unborn repository if a later operation fails.
/// A retry completes its initial commit without reinitializing the repository.
pub fn ensure_with_cancel(cwd: &Path, cancel: &dyn Fn() -> bool) -> Result<Bootstrap> {
    ensure_observed(cwd, cancel, &NoObserver)
}

/// Platform side effects of the bootstrap, reported around each one so the
/// engine can hold an operation receipt for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    Init,
    InitialCommit,
}

pub(crate) trait Observer {
    /// Called with the affected directory before the effect starts. An error
    /// prevents the effect.
    fn before(&self, effect: Effect, path: &Path) -> Result<()>;
    /// Called after the effect's Git command succeeded.
    fn after(&self, effect: Effect) -> Result<()>;
}

struct NoObserver;
impl Observer for NoObserver {
    fn before(&self, _: Effect, _: &Path) -> Result<()> {
        Ok(())
    }
    fn after(&self, _: Effect) -> Result<()> {
        Ok(())
    }
}

pub(crate) fn ensure_observed(cwd: &Path, cancel: &dyn Fn() -> bool, observer: &dyn Observer) -> Result<Bootstrap> {
    ensure!(!cancel(), "Git 项目准备已取消");
    let cwd = fs::canonicalize(cwd).context("无法解析项目目录")?;
    ensure!(cwd.is_dir(), "工作目录必须是已存在的目录");
    // Acquire this even on the HEAD fast path. A competing initializer may
    // already have written HEAD while still finishing the initial commit.
    let _cwd_lock = lock(&cwd, cancel)?;
    let mut initialized = false;
    let discovered = git(&cwd, &["rev-parse", "--is-inside-work-tree"], cancel)?;
    if !discovered.ok {
        ensure!(
            discovered.exit == Some(128),
            "Git 仓库探测失败，项目未初始化"
        );
        ensure!(
            !has_repository_marker(&cwd)?,
            "发现已有 Git 元数据，但仓库不可读取；请先修复仓库"
        );
        // A malformed global/system configuration must not be mistaken for
        // an absent repository. Exit 1 is the normal missing-config-key case.
        let config = git(
            &cwd,
            &["config", "--get", "core.repositoryformatversion"],
            cancel,
        )?;
        ensure!(
            config.ok || config.exit == Some(1),
            "Git 配置不可读取，项目未初始化"
        );
        observer.before(Effect::Init, &cwd)?;
        checked(&cwd, &["init"], cancel)?;
        observer.after(Effect::Init)?;
        initialized = true;
    } else {
        ensure!(
            text(&discovered)? == "true",
            "工作目录位于 bare 仓库或 Git 元数据目录中，不能作为项目工作区"
        );
    }

    let root = fs::canonicalize(checked(&cwd, &["rev-parse", "--show-toplevel"], cancel)?)
        .context("无法解析 Git 工作区根目录")?;
    let _root_lock = if path_key(&root) != path_key(&cwd) {
        Some(lock(&root, cancel)?)
    } else {
        None
    };
    let head = git(&cwd, &["rev-parse", "--verify", "HEAD^{commit}"], cancel)?;
    if head.ok {
        return Ok(Bootstrap {
            root: root.to_string_lossy().into_owned(),
            initialized,
            initial_commit: None,
        });
    }
    ensure!(
        path_key(&root) == path_key(&cwd),
        "祖先 Git 仓库尚无初始提交，请在仓库根目录准备项目，不能从子目录提交祖先文件"
    );
    let branch = checked(&cwd, &["symbolic-ref", "--quiet", "HEAD"], cancel)
        .context("Git HEAD 不可读取，不能自动创建初始提交")?;
    let reference = git(&cwd, &["show-ref", "--verify", "--quiet", &branch], cancel)?;
    ensure!(
        reference.exit == Some(1),
        "Git HEAD 引用损坏，不能自动创建初始提交"
    );
    ensure!(
        checked(&cwd, &["for-each-ref", "--format=%(refname)"], cancel)?.is_empty(),
        "Git HEAD 尚无提交，但仓库已有引用历史；请先人工核对，不能自动创建初始提交"
    );
    let head_log = cwd.join(checked(
        &cwd,
        &["rev-parse", "--git-path", "logs/HEAD"],
        cancel,
    )?);
    match fs::metadata(head_log) {
        Ok(metadata) => ensure!(
            metadata.is_file() && metadata.len() == 0,
            "Git HEAD 尚无提交，但保留了 HEAD reflog 历史；不能自动创建初始提交"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("无法检查 Git HEAD reflog 历史"),
    }
    ensure!(
        checked(&cwd, &["rev-list", "--all", "--reflog", "--count"], cancel)? == "0",
        "Git HEAD 尚无提交，但仓库已有 reflog 历史；不能自动创建初始提交"
    );
    observer.before(Effect::InitialCommit, &cwd)?;
    checked(&cwd, &["add", "--all", "--", "."], cancel)?;
    checked(
        &cwd,
        &[
            "commit",
            "--allow-empty",
            "-m",
            "Initial project checkpoint",
        ],
        cancel,
    )?;
    observer.after(Effect::InitialCommit)?;
    let initial_commit = checked(&cwd, &["rev-parse", "--verify", "HEAD^{commit}"], cancel)?;
    Ok(Bootstrap {
        root: root.to_string_lossy().into_owned(),
        initialized,
        initial_commit: Some(initial_commit),
    })
}

fn path_key(path: &Path) -> String {
    let path = path.to_string_lossy();
    if cfg!(windows) {
        path.to_lowercase()
    } else {
        path.into_owned()
    }
}

fn lock_path(path: &Path) -> PathBuf {
    let digest = format!("{:x}", Sha256::digest(path_key(path).as_bytes()));
    std::env::temp_dir()
        .join("super-agent-project-git-locks")
        .join(format!("{digest}.lock"))
}

fn lock(path: &Path, cancel: &dyn Fn() -> bool) -> Result<File> {
    let target = lock_path(path);
    fs::create_dir_all(target.parent().expect("lock directory"))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(target)
        .context("无法打开项目 Git 初始化锁")?;
    let started = Instant::now();
    loop {
        ensure!(!cancel(), "等待项目 Git 初始化锁时已取消");
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => {
                ensure!(started.elapsed() < GIT_TIMEOUT, "等待项目 Git 初始化锁超时");
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(TryLockError::Error(error)) => {
                return Err(error).context("无法锁定项目 Git 初始化");
            }
        }
    }
}

fn has_repository_marker(cwd: &Path) -> Result<bool> {
    for ancestor in cwd.ancestors() {
        match fs::symlink_metadata(ancestor.join(".git")) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("无法检查祖先 Git 元数据"),
        }
        // Bare repository roots have no .git child. Reject even an invalid
        // bare-shaped directory instead of nesting a new repository inside it.
        if ancestor.join("HEAD").try_exists()?
            && (ancestor.join("objects").try_exists()? || ancestor.join("refs").try_exists()?)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn git(cwd: &Path, args: &[&str], cancel: &dyn Fn() -> bool) -> Result<CommandOutput> {
    ensure!(!cancel(), "Git 项目准备已取消");
    let output = command_with_cancel(cwd, &git_argv(args), GIT_TIMEOUT, Some(cancel))
        .context("无法运行项目 Git 命令，请检查 Git 安装和目录权限")?;
    ensure!(!output.cancelled, "Git 项目准备已取消");
    ensure!(!output.timed_out, "Git 项目准备超时");
    Ok(output)
}

fn text(output: &CommandOutput) -> Result<String> {
    Ok(std::str::from_utf8(&output.output)
        .context("Git 输出不是 UTF-8")?
        .trim()
        .into())
}

fn checked(cwd: &Path, args: &[&str], cancel: &dyn Fn() -> bool) -> Result<String> {
    let output = git(cwd, args, cancel)?;
    if !output.ok {
        bail!(
            "Git {} 操作失败，工作目录已保留",
            args.first().unwrap_or(&"")
        );
    }
    text(&output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    };

    fn run(path: &Path, args: &[&str]) -> String {
        checked(path, args, &|| false).unwrap()
    }

    #[test]
    fn empty_directory_gets_one_commit_and_can_create_a_linked_worktree() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("project");
        fs::create_dir(&project).unwrap();
        let result = ensure(&project).unwrap();
        assert!(result.initialized);
        assert!(result.initial_commit.is_some());
        assert_eq!(run(&project, &["rev-list", "--count", "HEAD"]), "1");
        let linked = home.path().join("linked");
        run(
            &project,
            &[
                "worktree",
                "add",
                "-b",
                "linked-fixture",
                linked.to_str().unwrap(),
            ],
        );
        assert!(linked.join(".git").is_file());
        let reused = ensure(&linked).unwrap();
        assert!(!reused.initialized);
        assert!(reused.initial_commit.is_none());
        assert_eq!(
            PathBuf::from(reused.root),
            fs::canonicalize(linked).unwrap()
        );
    }

    #[test]
    fn initial_commit_includes_existing_files_and_respects_ignore() {
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(project.path().join("kept.txt"), "kept").unwrap();
        fs::write(project.path().join("ignored.txt"), "ignored").unwrap();
        ensure(project.path()).unwrap();
        assert_eq!(run(project.path(), &["ls-files"]), ".gitignore\nkept.txt");
        assert_eq!(run(project.path(), &["status", "--porcelain"]), "");
    }

    #[test]
    fn existing_dirty_repository_preserves_index_head_branch_and_worktree() {
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join("tracked.txt"), "initial").unwrap();
        ensure(project.path()).unwrap();
        fs::write(project.path().join("tracked.txt"), "staged").unwrap();
        run(project.path(), &["add", "--", "tracked.txt"]);
        fs::write(project.path().join("tracked.txt"), "unstaged").unwrap();
        fs::write(project.path().join("untracked.txt"), "untracked").unwrap();
        let before: Vec<String> = [
            &["rev-parse", "HEAD"][..],
            &["symbolic-ref", "HEAD"],
            &["diff", "--cached"],
            &["diff"],
            &["status", "--porcelain"],
        ]
        .into_iter()
        .map(|args| run(project.path(), args))
        .collect();
        let result = ensure(project.path()).unwrap();
        assert!(!result.initialized);
        assert!(result.initial_commit.is_none());
        let after: Vec<String> = [
            &["rev-parse", "HEAD"][..],
            &["symbolic-ref", "HEAD"],
            &["diff", "--cached"],
            &["diff"],
            &["status", "--porcelain"],
        ]
        .into_iter()
        .map(|args| run(project.path(), args))
        .collect();
        assert_eq!(before, after);
        assert_eq!(
            fs::read_to_string(project.path().join("tracked.txt")).unwrap(),
            "unstaged"
        );
    }

    #[test]
    fn subdirectory_reuses_ancestor_and_unborn_ancestor_is_rejected() {
        let project = tempfile::tempdir().unwrap();
        run(project.path(), &["init"]);
        let subdirectory = project.path().join("sub");
        fs::create_dir(&subdirectory).unwrap();
        fs::write(project.path().join("outside.txt"), "outside").unwrap();
        assert!(ensure(&subdirectory).is_err());
        assert!(!subdirectory.join(".git").exists());
        let result = ensure(project.path()).unwrap();
        assert!(!result.initialized);
        assert!(result.initial_commit.is_some());
        let child = ensure(&subdirectory).unwrap();
        assert_eq!(
            PathBuf::from(child.root),
            fs::canonicalize(project.path()).unwrap()
        );
        assert!(child.initial_commit.is_none());
        assert!(!subdirectory.join(".git").exists());
    }

    #[test]
    fn invalid_git_marker_and_bare_repository_are_never_reinitialized() {
        let invalid = tempfile::tempdir().unwrap();
        fs::write(invalid.path().join(".git"), "gitdir: missing-repository\n").unwrap();
        assert!(ensure(invalid.path()).is_err());
        assert_eq!(
            fs::read_to_string(invalid.path().join(".git")).unwrap(),
            "gitdir: missing-repository\n"
        );
        let bare = tempfile::tempdir().unwrap();
        run(bare.path(), &["init", "--bare"]);
        assert!(ensure(bare.path()).is_err());
        assert!(!bare.path().join(".git").exists());
    }

    #[test]
    fn concurrent_initialization_creates_exactly_one_initial_commit() {
        let project = tempfile::tempdir().unwrap();
        fs::write(project.path().join("file.txt"), "initial").unwrap();
        let gate = Arc::new(Barrier::new(4));
        let tasks: Vec<_> = (0..4)
            .map(|_| {
                let path = project.path().to_path_buf();
                let gate = gate.clone();
                std::thread::spawn(move || {
                    gate.wait();
                    ensure(&path).unwrap()
                })
            })
            .collect();
        let results: Vec<_> = tasks.into_iter().map(|task| task.join().unwrap()).collect();
        assert_eq!(
            results.iter().filter(|result| result.initialized).count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| result.initial_commit.is_some())
                .count(),
            1
        );
        assert_eq!(run(project.path(), &["rev-list", "--count", "HEAD"]), "1");
        assert_eq!(run(project.path(), &["status", "--porcelain"]), "");
    }

    #[test]
    fn missing_head_branch_with_refs_or_only_reflog_preserves_index_and_files() {
        for remove_last_reference in [false, true] {
            let project = tempfile::tempdir().unwrap();
            fs::write(project.path().join("tracked.txt"), "initial").unwrap();
            ensure(project.path()).unwrap();
            let original_branch = run(project.path(), &["symbolic-ref", "HEAD"]);
            fs::write(project.path().join("tracked.txt"), "staged").unwrap();
            run(project.path(), &["add", "--", "tracked.txt"]);
            fs::write(project.path().join("tracked.txt"), "unstaged").unwrap();
            fs::write(project.path().join("untracked.txt"), "untracked").unwrap();
            run(
                project.path(),
                &["symbolic-ref", "HEAD", "refs/heads/missing"],
            );
            if remove_last_reference {
                run(project.path(), &["update-ref", "-d", &original_branch]);
                assert!(run(project.path(), &["for-each-ref", "--format=%(refname)"]).is_empty());
                assert_eq!(
                    run(
                        project.path(),
                        &["rev-list", "--all", "--reflog", "--count"]
                    ),
                    "1"
                );
            }
            let index_before = fs::read(project.path().join(".git/index")).unwrap();
            let head_before = fs::read(project.path().join(".git/HEAD")).unwrap();
            let error = ensure(project.path()).unwrap_err();
            assert!(error.to_string().contains("历史"), "{error:#}");
            assert_eq!(
                fs::read(project.path().join(".git/index")).unwrap(),
                index_before
            );
            assert_eq!(
                fs::read(project.path().join(".git/HEAD")).unwrap(),
                head_before
            );
            assert_eq!(
                fs::read_to_string(project.path().join("tracked.txt")).unwrap(),
                "unstaged"
            );
            assert_eq!(
                fs::read_to_string(project.path().join("untracked.txt")).unwrap(),
                "untracked"
            );
        }
    }

    #[test]
    fn cancellation_while_waiting_for_lock_does_not_initialize() {
        let project = tempfile::tempdir().unwrap();
        let canonical = fs::canonicalize(project.path()).unwrap();
        let _owner = lock(&canonical, &|| false).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = cancelled.clone();
        let task = std::thread::spawn(move || {
            ensure_with_cancel(&canonical, &|| signal.load(Ordering::SeqCst))
        });
        std::thread::sleep(Duration::from_millis(50));
        cancelled.store(true, Ordering::SeqCst);
        assert!(task.join().unwrap().is_err());
        assert!(!project.path().join(".git").exists());
        assert!(ensure_with_cancel(project.path(), &|| true).is_err());
    }
}
