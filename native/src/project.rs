use anyhow::{Context, Result, bail, ensure};
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};
use tempfile::NamedTempFile;

#[derive(Deserialize)]
pub struct Search {
    cwd: PathBuf,
    #[serde(default)]
    q: String,
}

pub fn files(req: Search) -> Result<Value> {
    let root = req.cwd.canonicalize()?;
    ensure!(root.is_dir(), "项目目录不存在");
    let query = req.q.to_lowercase();
    let mut names = vec![];
    let walker = WalkBuilder::new(&root)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|ent| {
            !matches!(
                ent.file_name().to_str(),
                Some("node_modules" | "target" | ".git" | "__pycache__" | ".venv")
            )
        })
        .build();
    for e in walker.filter_map(Result::ok) {
        if e.file_type().is_some_and(|t| t.is_file()) {
            let rel = e
                .path()
                .strip_prefix(&root)?
                .to_string_lossy()
                .replace('\\', "/");
            if rel.to_lowercase().contains(&query) {
                names.push(rel);
                if names.len() == 100 {
                    break;
                }
            }
        }
    }
    names.sort_unstable();
    Ok(json!(names))
}

#[derive(Deserialize)]
pub struct Snapshot {
    project: PathBuf,
    directory: PathBuf,
    names: Vec<String>,
}

pub fn snapshot(req: Snapshot) -> Result<Value> {
    ensure!(req.names.len() <= 16, "单次最多附加 16 个文件");
    let root = req.project.canonicalize()?;
    fs::create_dir_all(&req.directory)?;
    let mut total = 0;
    let mut manifest = vec![];
    for name in req.names {
        let rel = Path::new(&name);
        ensure!(
            rel.components()
                .all(|p| matches!(p, Component::Normal(_) | Component::CurDir))
                && !name.contains(':'),
            "只接受项目内相对路径"
        );
        let path = root.join(rel).canonicalize()?;
        ensure!(
            path.starts_with(&root) && path.is_file(),
            "附件位于项目外或并非普通文件"
        );
        let mut src = File::open(&path)?;
        #[cfg(windows)]
        ensure!(
            opened_path(&src)?.starts_with(&root),
            "打开的附件位于项目外"
        );
        let mut tmp = NamedTempFile::new_in(&req.directory)?;
        let mut hash = Sha256::new();
        let mut size = 0;
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
        let dest = req.directory.join(&key);
        tmp.as_file().sync_all()?;
        tmp.persist(&dest).context("无法提交文件快照")?;
        manifest.push(json!({"name":name,"bytes":size,"sha256":key,"object_path":dest}));
    }
    Ok(json!(manifest))
}

#[cfg(windows)]
fn opened_path(f: &File) -> Result<PathBuf> {
    use std::{
        ffi::OsString,
        os::windows::{ffi::OsStringExt, io::AsRawHandle},
    };
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
    let mut buf = vec![0u16; 32768];
    let n = unsafe {
        GetFinalPathNameByHandleW(f.as_raw_handle(), buf.as_mut_ptr(), buf.len() as u32, 0)
    };
    ensure!(
        n > 0 && (n as usize) < buf.len(),
        "无法核验已打开附件的路径"
    );
    Ok(PathBuf::from(OsString::from_wide(&buf[..n as usize])))
}
