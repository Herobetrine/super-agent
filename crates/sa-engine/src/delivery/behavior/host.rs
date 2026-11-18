//! Determine the host program independently of the candidate directory. Only
//! the explicitly materialized inputs are supplied as project files. This is
//! a local operator command, with the operator's OS permissions.
use anyhow::{Context, Result, ensure};
use sa_store::delivery::sha256;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    env,
    ffi::OsString,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

use crate::services::realpath;

const MAX_PROGRAM_BYTES: u64 = 512 * 1024 * 1024;

struct HostScope {
    project: PathBuf,
    data: PathBuf,
}

impl HostScope {
    fn new(project: &Path, data: &Path) -> Result<Self> {
        Ok(Self {
            project: realpath(project).or_else(|_| std::path::absolute(project))?,
            data: realpath(data)?,
        })
    }

    fn outside(&self, path: &Path) -> bool {
        !path.starts_with(&self.project) && !path.starts_with(&self.data)
    }

    fn program(&self, name: &str) -> Result<(PathBuf, OsString, fs::File)> {
        let path = env::var_os("PATH").unwrap_or_default();
        let dirs: Vec<_> = env::split_paths(&path)
            .filter(|dir| dir.is_absolute())
            .filter_map(|dir| realpath(&dir).ok())
            .filter(|dir| self.outside(dir) && dir.is_dir())
            .collect();
        let binary = resolve(name, &dirs)?;
        ensure!(self.outside(&binary), "宿主检查程序不得来自当前项目或账本");
        ensure!(binary.to_str().is_some(), "检查程序路径必须可表示为 UTF-8");
        let file = open_program(&binary)?;
        let meta = file.metadata()?;
        ensure!(
            meta.is_file() && meta.len() <= MAX_PROGRAM_BYTES,
            "宿主检查程序必须为不超过 512 MiB 的文件"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                meta.permissions().mode() & 0o111 != 0,
                "宿主检查程序文件没有执行权限"
            );
        }
        Ok((binary, env::join_paths(&dirs)?, file))
    }
}

pub(super) fn preflight(name: &str, project: &Path, data: &Path) -> Result<()> {
    HostScope::new(project, data)?.program(name)
        .map(|_| ()).map_err(|error| anyhow::anyhow!("无法采用此检查入口：{error:#}。请在项目检查设置中改用可用的宿主解释器，并显式固定脚本文件。"))
}

pub(super) fn temp_dir(project: &Path, data: &Path) -> Result<tempfile::TempDir> {
    let parent = realpath(&env::temp_dir())?;
    ensure!(
        HostScope::new(project, data)?.outside(&parent),
        "行为检查临时目录必须位于项目和账本以外"
    );
    Ok(tempfile::tempdir_in(parent)?)
}

pub(super) struct Execution {
    pub argv: Vec<String>,
    pub env: Vec<(OsString, OsString)>,
    binary: PathBuf,
    digest: String,
    // Windows denies writes and replacement until after the child exits.
    _guard: fs::File,
}

impl Execution {
    pub fn prepare(
        temp: &Path,
        project: &Path,
        data: &Path,
        argv: &[String],
        receipt: &mut Value,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Self> {
        let scope = HostScope::new(project, data)?;
        let temp = realpath(temp)?;
        let separator = if cfg!(windows) { ';' } else { ':' };
        ensure!(
            !temp.to_string_lossy().contains(separator),
            "临时路径含 Git 目录列表分隔符，无法限定仓库发现范围"
        );
        ensure!(
            scope.outside(&temp),
            "行为检查临时目录必须位于项目和账本以外"
        );
        let (binary, path, mut guard) = scope.program(&argv[0])?;
        let digest = hash_program(&mut guard, cancel)?;
        let program = binary.to_str().context("检查程序路径必须可表示为 UTF-8")?;
        let mut context = receipt.clone();
        context["program"] = json!(program);
        context["program_sha256"] = json!(digest);
        context["path_sha256"] = json!(sha256(path.as_encoded_bytes()));
        ensure!(context.to_string().len() <= 4096, "宿主执行上下文超限");
        *receipt = context;

        let home = temp.join("home");
        let scratch = temp.join("tmp");
        fs::create_dir(&home)?;
        fs::create_dir(&scratch)?;
        let mut env: Vec<(OsString, OsString)> = vec![
            ("PATH".into(), path),
            ("HOME".into(), home.as_os_str().into()),
            ("USERPROFILE".into(), home.into()),
            ("TMPDIR".into(), scratch.as_os_str().into()),
            ("TEMP".into(), scratch.as_os_str().into()),
            ("TMP".into(), scratch.into()),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_CEILING_DIRECTORIES".into(), temp.as_os_str().into()),
        ];
        #[cfg(windows)]
        {
            let windows = std::env::var_os("SystemRoot").context("宿主缺少 SystemRoot")?;
            let system = realpath(Path::new(&windows))?;
            ensure!(scope.outside(&system), "SystemRoot 不能指向项目或账本");
            env.push(("SystemRoot".into(), system.as_os_str().into()));
            env.push(("WINDIR".into(), system.as_os_str().into()));
            env.push((
                "COMSPEC".into(),
                system.join("System32").join("cmd.exe").into(),
            ));
            env.push(("PATHEXT".into(), ".COM;.EXE;.BAT;.CMD".into()));
        }
        #[cfg(not(windows))]
        env.push(("LC_ALL".into(), "C".into()));
        let mut argv = argv.to_vec();
        argv[0] = program.into();
        Ok(Self {
            argv,
            env,
            binary,
            digest,
            _guard: guard,
        })
    }

    pub fn verify(&self, cancel: &dyn Fn() -> bool) -> Result<()> {
        ensure!(
            hash_program(&mut open_program(&self.binary)?, cancel)? == self.digest,
            "宿主检查程序在执行期间发生变化"
        );
        Ok(())
    }
}

fn resolve(name: &str, dirs: &[PathBuf]) -> Result<PathBuf> {
    let path = Path::new(name);
    #[cfg(windows)]
    let with_suffix = |path: &Path| -> PathBuf {
        if path.extension().is_none() {
            path.with_extension("exe")
        } else {
            path.into()
        }
    };
    #[cfg(not(windows))]
    let with_suffix = |path: &Path| path.to_path_buf();
    let path = if path.is_absolute() {
        with_suffix(path)
    } else {
        ensure!(
            matches!(
                path.components().collect::<Vec<_>>().as_slice(),
                [Component::Normal(_)]
            ),
            "检查程序需要宿主 PATH 中的程序名或绝对路径"
        );
        let leaf = with_suffix(path);
        dirs.iter()
            .map(|dir| dir.join(&leaf))
            .find(|item| item.is_file())
            .context("宿主 PATH 中找不到检查程序")?
    };
    let path = realpath(&path)?;
    #[cfg(windows)]
    ensure!(
        path.extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe") || ext.eq_ignore_ascii_case("com")),
        "请以宿主可执行程序运行脚本，并显式采用对应验证器文件"
    );
    Ok(path)
}

fn open_program(binary: &Path) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1);
    }
    options.open(binary).context("无法核验宿主检查程序")
}

fn hash_program(file: &mut fs::File, cancel: &dyn Fn() -> bool) -> Result<String> {
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.len() <= MAX_PROGRAM_BYTES,
        "宿主检查程序必须为不超过 512 MiB 的文件"
    );
    let mut digest = Sha256::new();
    let mut bytes = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        ensure!(!cancel(), "行为检查已取消");
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        ensure!(bytes <= MAX_PROGRAM_BYTES, "宿主检查程序过大");
        digest.update(&buffer[..n]);
    }
    ensure!(
        bytes == meta.len() && file.metadata()?.modified()? == meta.modified()?,
        "读取期间宿主检查程序已改变"
    );
    Ok(format!("{:x}", digest.finalize()))
}
