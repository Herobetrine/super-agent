//! Child process plumbing shared by every harness (port of AC:152-318).
//!
//! - Spawned without a console window and without a shell.
//! - stderr goes to a bounded in-memory tail used only for classification;
//!   it is never journaled (AC:183 intent).
//! - Stopping kills the whole process tree (`taskkill /T /F` on Windows,
//!   the process group elsewhere), also when the handle is dropped.

use crate::sys::Sys;
use sa_engine::adapter::{AdapterError, ErrorClass};
use sa_engine::resilience::classify::{StderrTail, classified};
use serde_json::Value;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio_util::sync::CancellationToken;

/// Largest single NDJSON record accepted (AC:276).
pub const MAX_LINE: usize = 128 * 1024 * 1024;
/// Largest captured catalog response (AC:259).
pub const MAX_CAPTURE: usize = 16 * 1024 * 1024;

pub fn cancelled() -> AdapterError {
    classified(ErrorClass::Cancelled, "cancelled")
}

fn exe_name(exe: &Path) -> String {
    exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

pub struct Proc {
    child: Child,
    pid: Option<u32>,
    exe: String,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    tail: Arc<Mutex<StderrTail>>,
    exited: Option<Option<i32>>,
}

/// `start(exe, args, {cwd, env})` (AC:223-248).
pub fn start(exe: &Path, args: &[String], cwd: &Path, env: &Sys) -> Result<Proc, AdapterError> {
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env.vars().iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(|e| {
        let mut err = classified(ErrorClass::RouteUnavailable, "spawn:failed");
        err.message = format!("无法启动 {}：{}", exe_name(exe), e.kind());
        err
    })?;
    let tail = Arc::new(Mutex::new(StderrTail::default()));
    if let Some(mut stderr) = child.stderr.take() {
        let tail = tail.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                tail.lock().unwrap_or_else(|e| e.into_inner()).push(&buf[..n]);
            }
        });
    }
    Ok(Proc {
        pid: child.id(),
        stdin: child.stdin.take(),
        stdout: child.stdout.take(),
        child,
        exe: exe_name(exe),
        tail,
        exited: None,
    })
}

impl Proc {
    pub fn exe(&self) -> &str {
        &self.exe
    }

    /// Write `input` and close stdin (`child.stdin.end(input)`).
    pub async fn finish_input(&mut self, input: &str) {
        if let Some(mut stdin) = self.stdin.take() {
            // A broken pipe surfaces through the exit contract.
            let _ = stdin.write_all(input.as_bytes()).await;
            let _ = stdin.shutdown().await;
        }
    }

    /// Exit code; `None` when killed by a signal or unknown.
    pub async fn wait(&mut self) -> Option<i32> {
        if let Some(code) = self.exited {
            return code;
        }
        let code = self.child.wait().await.ok().and_then(|s| s.code());
        self.exited = Some(code);
        code
    }

    /// Wait at most `limit`, then kill the tree.
    pub async fn wait_or_kill(&mut self, limit: Duration) -> Option<i32> {
        if self.exited.is_none() && tokio::time::timeout(limit, self.wait()).await.is_err() {
            self.kill_tree().await;
        }
        self.wait().await
    }

    pub fn running(&mut self) -> bool {
        self.exited.is_none() && matches!(self.child.try_wait(), Ok(None))
    }

    /// `killTree(child)` (AC:204-222).
    pub async fn kill_tree(&mut self) {
        if !self.running() {
            return;
        }
        if let Some(pid) = self.pid {
            let _ = tree_killer(pid).status().await;
        }
        let _ = self.child.start_kill();
    }

    /// Error for an exit without a valid terminal record, refined by stderr.
    pub fn exit_error(&self, code: Option<i32>) -> AdapterError {
        let tail = self.tail.lock().unwrap_or_else(|e| e.into_inner());
        let mut e = tail.exit_error(code);
        if e.class == ErrorClass::ProcessCrashed && code.is_some_and(|c| c != 0) {
            e.message = format!("{} 执行失败", self.exe);
        }
        e
    }
}

fn tree_killer(pid: u32) -> Command {
    #[cfg(windows)]
    {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        let mut c = Command::new(Path::new(&root).join("System32/taskkill.exe"));
        c.args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = Command::new("kill");
        c.args(["-KILL", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        c
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        // A cancelled or aborted turn must not leave descendants behind.
        if self.running()
            && let Some(pid) = self.pid
        {
            let mut killer = tree_killer(pid);
            let _ = killer.as_std_mut().spawn();
        }
    }
}

/// Strict UTF-8 NDJSON reader (`jsonLines`, AC:270-306).
pub struct Lines<R> {
    reader: R,
    buf: Vec<u8>,
    eof: bool,
}

pub fn protocol_error(message: &str) -> AdapterError {
    let mut e = classified(ErrorClass::Protocol, "ndjson:invalid");
    e.message = message.into();
    e
}

impl<R: AsyncRead + Unpin> Lines<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buf: Vec::new(),
            eof: false,
        }
    }

    fn decode(line: &[u8]) -> Result<Option<Value>, AdapterError> {
        let text = std::str::from_utf8(line).map_err(|_| protocol_error("无效的原生 UTF-8 NDJSON"))?;
        if text.trim().is_empty() {
            return Ok(None);
        }
        serde_json::from_str(text)
            .map(Some)
            .map_err(|_| protocol_error("无效的原生 UTF-8 NDJSON"))
    }

    /// Next record; `None` at EOF.
    pub async fn next(&mut self) -> Result<Option<Value>, AdapterError> {
        loop {
            if let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=i).collect();
                match Self::decode(&line[..i])? {
                    Some(v) => return Ok(Some(v)),
                    None => continue,
                }
            }
            if self.eof {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                let line = std::mem::take(&mut self.buf);
                match Self::decode(&line)? {
                    Some(v) => return Ok(Some(v)),
                    None => return Ok(None),
                }
            }
            if self.buf.len() > MAX_LINE {
                return Err(protocol_error("单条原生事件超过 128 MiB"));
            }
            let mut chunk = vec![0u8; 64 * 1024];
            let n = self
                .reader
                .read(&mut chunk)
                .await
                .map_err(|_| protocol_error("原生输出读取失败"))?;
            if n == 0 {
                self.eof = true;
            } else {
                self.buf.extend_from_slice(&chunk[..n]);
            }
        }
    }
}

/// `capture(exe, args, {input, timeout, signal})` (AC:249-269): stdout of a
/// short-lived probe.
pub async fn capture(
    exe: &Path,
    args: &[String],
    cwd: &Path,
    env: &Sys,
    input: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<String, AdapterError> {
    let mut p = start(exe, args, cwd, env)?;
    p.finish_input(input).await;
    let mut stdout = p.stdout.take().ok_or_else(|| protocol_error("原生输出不可读"))?;
    let read = async {
        let mut out = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let n = stdout.read(&mut chunk).await.map_err(|_| protocol_error("原生输出读取失败"))?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..n]);
            if out.len() > MAX_CAPTURE {
                return Err(protocol_error("原生目录响应超限"));
            }
        }
        Ok(out)
    };
    let result = tokio::select! {
        r = tokio::time::timeout(timeout, read) => match r {
            Ok(r) => r,
            Err(_) => Err(classified(ErrorClass::Stalled, "probe:timeout")),
        },
        () = cancel.cancelled() => Err(cancelled()),
    };
    let out = match result {
        Ok(out) => out,
        Err(e) => {
            p.kill_tree().await;
            p.wait().await;
            return Err(e);
        }
    };
    let code = p.wait_or_kill(Duration::from_secs(2)).await;
    if code != Some(0) {
        return Err(p.exit_error(code));
    }
    String::from_utf8(out).map_err(|_| protocol_error("原生目录不是 UTF-8"))
}

/// `stream(exe, args, input, options, onEvent)` (AC:307-318): write the
/// prompt, feed every NDJSON record to `on_event`, then require exit 0.
pub async fn stream(
    p: &mut Proc,
    input: &str,
    cancel: &CancellationToken,
    mut on_event: impl FnMut(Value) -> Result<(), AdapterError>,
) -> Result<(), AdapterError> {
    p.finish_input(input).await;
    let stdout = p.stdout.take().ok_or_else(|| protocol_error("原生输出不可读"))?;
    let mut lines = Lines::new(stdout);
    let outcome = loop {
        let next = tokio::select! {
            r = lines.next() => r,
            () = cancel.cancelled() => break Err(cancelled()),
        };
        match next {
            Ok(Some(event)) => {
                if let Err(e) = on_event(event) {
                    break Err(e);
                }
            }
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    if let Err(e) = outcome {
        p.kill_tree().await;
        p.wait().await;
        return Err(e);
    }
    let code = tokio::select! {
        c = p.wait() => c,
        () = cancel.cancelled() => {
            p.kill_tree().await;
            p.wait().await;
            return Err(cancelled());
        }
    };
    if code != Some(0) {
        return Err(p.exit_error(code));
    }
    Ok(())
}
