//! Read-only operations that need no scheduler and work in observer windows
//! too: Git changes of a project (`studio.diff`, ST:72-111, 345-358) and the
//! hash-checked ticket export (`studio.exportTo`, ST:376-406).

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

const GIT_CAP: usize = 256 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(15);

/// A background result polled once per frame.
pub struct Pending<T> {
    rx: mpsc::Receiver<Result<T, String>>,
}

impl<T: Send + 'static> Pending<T> {
    pub fn spawn(ctx: &egui::Context, f: impl FnOnce() -> Result<T> + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(f().map_err(|e| format!("{e:#}")));
            ctx.request_repaint();
        });
        Self { rx }
    }

    /// `Some` once the work has finished.
    pub fn poll(&self) -> Option<Result<T, String>> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err("后台任务意外结束".into())),
        }
    }
}

fn git(exe: &Path, cwd: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new(exe);
    cmd.args(["--no-pager", "--no-optional-locks", "-c", "core.fsmonitor=false", "-c", "color.ui=false"])
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().context("无法启动 Git")?;
    let mut out = child.stdout.take().expect("piped stdout");
    let mut err = child.stderr.take().expect("piped stderr");
    let cut = Arc::new(AtomicBool::new(false));
    let reader = {
        let cut = cut.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 16 * 1024];
            while let Ok(n) = out.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let space = GIT_CAP.saturating_sub(buf.len());
                buf.extend_from_slice(&chunk[..n.min(space)]);
                if n > space {
                    cut.store(true, Ordering::SeqCst);
                    break;
                }
            }
            buf
        })
    };
    let errs = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = err.by_ref().take(4096).read_to_end(&mut s);
        String::from_utf8_lossy(&s).trim().to_string()
    });
    let end = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        // Over the cap: stop the child instead of letting it block on a full pipe.
        if cut.load(Ordering::SeqCst) {
            let _ = child.kill();
        }
        match child.try_wait()? {
            Some(s) => break Some(s),
            None if Instant::now() >= end => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let buf = reader.join().map_err(|_| anyhow!("Git 检查中断"))?;
    let cut = cut.load(Ordering::SeqCst);
    let stderr = errs.join().unwrap_or_default();
    if !cut && status.is_none_or(|s| !s.success()) {
        bail!(if stderr.is_empty() { "Git 检查中断".to_string() } else { stderr });
    }
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if cut {
        text.push_str("\n…预览已截断");
    }
    Ok(text)
}

/// Working tree status, staged and unstaged changes (ST:345-358).
pub fn diff(root: &Path) -> Result<String> {
    let exe = sa_adapters::sys::Sys::from_process()
        .which(if cfg!(windows) { "git.exe" } else { "git" })
        .ok_or_else(|| anyhow!("未在 PATH 中找到 Git"))?;
    let status = git(&exe, root, &["status", "--short", "--"])?;
    let staged = git(&exe, root, &["diff", "--cached", "--no-ext-diff", "--no-textconv", "--"])?;
    let changes = git(&exe, root, &["diff", "--no-ext-diff", "--no-textconv", "--"])?;
    let text = [("工作区状态", status), ("已暂存", staged), ("未暂存", changes)]
        .into_iter()
        .filter(|(_, body)| !body.is_empty())
        .map(|(title, body)| format!("{title}\n{body}"))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(if text.is_empty() { "工作区干净".into() } else { text })
}

fn cell(v: ValueRef<'_>) -> serde_json::Value {
    match v {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(i) => i.into(),
        // JSON.stringify prints integral doubles without a fraction.
        ValueRef::Real(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => (f as i64).into(),
        ValueRef::Real(f) => f.into(),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned().into(),
        ValueRef::Blob(b) => String::from_utf8_lossy(b).into_owned().into(),
    }
}

/// Write every event of `ticket` as JSON Lines to a NEW file, verifying each
/// payload digest; a partial file is removed on failure (ST:376-406).
pub fn export(data: &Path, ticket: i64, file: &Path) -> Result<PathBuf> {
    let db = Connection::open_with_flags(
        data.join(sa_store::DB_FILE),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("无法打开工单数据库")?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.execute_batch("BEGIN")?;
    let exists: bool = db
        .query_row("SELECT count(*) FROM tickets WHERE id=?", [ticket], |r| r.get::<_, i64>(0))?
        > 0;
    if !exists {
        bail!("工单不存在");
    }
    let out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(file)
        .with_context(|| format!("无法创建导出文件（请选择新文件）：{}", file.display()))?;
    let written = (|| -> Result<()> {
        let mut w = std::io::BufWriter::new(out);
        let mut stmt = db.prepare("SELECT * FROM events WHERE ticket=? ORDER BY seq")?;
        let names: Vec<String> = stmt.column_names().map(|s| s.to_string()).collect();
        let mut rows = stmt.query([ticket])?;
        while let Some(r) = rows.next()? {
            let mut obj = serde_json::Map::new();
            for (i, n) in names.iter().enumerate() {
                obj.insert(n.clone(), cell(r.get_ref(i)?));
            }
            let payload = obj.get("payload").and_then(|v| v.as_str()).unwrap_or_default();
            let digest = hex(&Sha256::digest(payload.as_bytes()));
            if obj.get("sha256").and_then(|v| v.as_str()) != Some(digest.as_str()) {
                bail!("事件摘要校验失败 #{}", obj.get("seq").cloned().unwrap_or_default());
            }
            serde_json::to_writer(&mut w, &obj)?;
            w.write_all(b"\n")?;
        }
        w.flush()?;
        w.get_ref().sync_all()?;
        Ok(())
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(file);
        return Err(e);
    }
    Ok(file.to_path_buf())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_verifies_digests_and_never_overwrites() {
        let data = tempfile::tempdir().unwrap();
        let db = Connection::open(data.path().join(sa_store::DB_FILE)).unwrap();
        db.execute_batch(
            "CREATE TABLE tickets(id INTEGER PRIMARY KEY);
             CREATE TABLE events(seq INTEGER PRIMARY KEY, ts REAL, ticket INTEGER, kind TEXT, payload TEXT, sha256 TEXT);
             INSERT INTO tickets VALUES(1);",
        )
        .unwrap();
        for (ts, p) in [(1.0, "\"你好\""), (2.5, "{\"a\":1}")] {
            let h = hex(&Sha256::digest(p.as_bytes()));
            db.execute(
                "INSERT INTO events(ts,ticket,kind,payload,sha256) VALUES(?,1,'user',?,?)",
                rusqlite::params![ts, p, h],
            )
            .unwrap();
        }
        let out = data.path().join("t.jsonl");
        export(data.path(), 1, &out).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("{\"seq\":1,\"ts\":1,\"ticket\":1,\"kind\":\"user\",\"payload\":\"\\\"你好\\\"\""));
        assert!(lines[1].contains("\"ts\":2.5"));
        // Existing files are never overwritten.
        assert!(export(data.path(), 1, &out).is_err());
        assert_eq!(std::fs::read_to_string(&out).unwrap(), text);
        // A tampered payload fails and leaves no partial file.
        db.execute("UPDATE events SET payload='\"x\"' WHERE seq=2", []).unwrap();
        let bad = data.path().join("bad.jsonl");
        let e = export(data.path(), 1, &bad).unwrap_err();
        assert_eq!(e.to_string(), "事件摘要校验失败 #2");
        assert!(!bad.exists());
        assert_eq!(export(data.path(), 9, &bad).unwrap_err().to_string(), "工单不存在");
    }

    #[test]
    fn diff_reports_status_and_changes() {
        let Some(exe) = sa_adapters::sys::Sys::from_process().which(if cfg!(windows) { "git.exe" } else { "git" })
        else {
            eprintln!("git not on PATH; skipped");
            return;
        };
        let repo = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let ok = Command::new(&exe).args(args).current_dir(repo.path()).output().unwrap();
            assert!(ok.status.success(), "{args:?}: {}", String::from_utf8_lossy(&ok.stderr));
        };
        run(&["init", "-q"]);
        assert_eq!(diff(repo.path()).unwrap(), "工作区干净");
        std::fs::write(repo.path().join("a.txt"), "one\n").unwrap();
        run(&["add", "a.txt"]);
        let text = diff(repo.path()).unwrap();
        assert!(text.starts_with("工作区状态\nA  a.txt"), "{text}");
        assert!(text.contains("已暂存\ndiff --git a/a.txt b/a.txt"), "{text}");
        let outside = tempfile::tempdir().unwrap();
        assert!(diff(outside.path()).is_err());
    }
}

/// Conversation transcript as Markdown: goal, operator messages and every
/// reply (control lines removed), written to a NEW file.
pub fn export_markdown(data: &Path, ticket: i64, file: &Path) -> Result<PathBuf> {
    let db = Connection::open_with_flags(
        data.join(sa_store::DB_FILE),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("无法打开工单数据库")?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.execute_batch("BEGIN")?;
    let (goal, cwd, lead, created): (String, String, String, f64) = db
        .query_row("SELECT goal,cwd,lead,created FROM tickets WHERE id=?", [ticket], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .map_err(|_| anyhow!("工单不存在"))?;
    let title = goal.lines().next().unwrap_or_default().chars().take(80).collect::<String>();
    let mut out = format!(
        "# {title}\n\n- 工单：#{ticket}\n- 项目：`{cwd}`\n- 主控：{}\n- 创建：{}\n\n",
        crate::labels::agent_name(&lead),
        crate::labels::activity_time(created)
    );
    let mut stmt = db.prepare(
        "SELECT e.kind,e.ts,e.payload,j.agent,j.parent,j.id FROM events e LEFT JOIN jobs j ON j.id=e.job
         WHERE e.ticket=? AND e.kind IN ('user','user.follow','reply','note') ORDER BY e.seq",
    )?;
    let mut rows = stmt.query([ticket])?;
    while let Some(r) = rows.next()? {
        let kind: String = r.get(0)?;
        let ts: f64 = r.get(1)?;
        let payload: String = r.get(2)?;
        let agent: Option<String> = r.get(3)?;
        let parent: Option<i64> = r.get(4)?;
        let job: Option<i64> = r.get(5)?;
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null);
        let time = crate::labels::activity_time(ts);
        match kind.as_str() {
            "user" | "user.follow" => {
                let text = v.as_str().map(str::to_string).unwrap_or_else(|| v["text"].as_str().unwrap_or_default().to_string());
                let who = if kind == "user" { "目标" } else { "追加指令" };
                out.push_str(&format!("---\n\n## 你 · {who} · {time}\n\n{text}\n\n"));
            }
            "note" => {
                if v["sender"].as_str() == Some("user") {
                    out.push_str(&format!("> 定向留言（{time}）：{}\n\n", v["body"].as_str().unwrap_or_default()));
                }
            }
            _ => {
                let (body, _) = crate::feed::split_reply(v["text"].as_str().unwrap_or_default());
                if body.trim().is_empty() {
                    continue;
                }
                let name = crate::labels::agent_name(agent.as_deref().unwrap_or_default());
                let role = if parent.is_some() { "协作回报" } else { "主控" };
                let id = job.map(|j| format!(" #{j}")).unwrap_or_default();
                out.push_str(&format!("## {name}{id} · {role} · {time}\n\n{body}\n\n"));
            }
        }
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(file)
        .with_context(|| format!("无法创建导出文件（请选择新文件）：{}", file.display()))?;
    f.write_all(out.as_bytes())?;
    Ok(file.to_path_buf())
}
