//! Exclusive scheduler ownership of a journal directory (port of
//! `native/src/owner.rs`).
//!
//! - `desktop.lock` is held with an exclusive OS file lock for the lifetime
//!   of [`Owner`]. The Electron broker takes the same lock, so the Rust and
//!   Electron schedulers can never run against one journal at the same time.
//! - On Windows the current process joins a job object with
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. The job handle lives in a static
//!   for the whole process: closing it would terminate this process too, so
//!   only process exit (abnormal included) closes it, and Windows then
//!   terminates the ordinary harness children. Desktop relaunch explicitly
//!   requests breakaway so the replacement survives.

use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::path::Path;
use std::process::{Child, Command};
#[cfg(windows)]
use std::sync::OnceLock;

pub const LOCK_FILE: &str = "desktop.lock";
pub const BUSY: &str = "此工单库已有工作台在运行，请先关闭旧窗口";

/// Holds the journal lock; dropping it releases the lock only.
pub struct Owner {
    _lock: File,
}

#[cfg(windows)]
static JOB: OnceLock<Option<win::Handle>> = OnceLock::new();

/// Claim `dir` for this process. Fails with [`BUSY`] when another scheduler
/// (Rust or Electron) holds it.
pub fn claim(dir: &Path) -> Result<Owner> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(LOCK_FILE))?;
    lock.try_lock().map_err(|_| anyhow::anyhow!(BUSY))?;
    // Containment is best effort: a parent job that forbids nesting must
    // not prevent the scheduler from running.
    #[cfg(windows)]
    JOB.get_or_init(|| win::contain_self().ok());
    Ok(Owner { _lock: lock })
}

/// Whether another process currently owns `dir`.
pub fn is_claimed(dir: &Path) -> bool {
    let Ok(f) = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join(LOCK_FILE))
    else {
        return false;
    };
    match f.try_lock() {
        Ok(()) => {
            let _ = f.unlock();
            false
        }
        Err(_) => true,
    }
}

/// Spawn a desktop replacement outside this process's Windows Job.
/// An enclosing Job may still apply its own restrictions. Ordinary harness
/// commands use their usual spawn paths and remain subject to containment.
pub fn spawn_detached(mut command: Command) -> std::io::Result<Child> {
    #[cfg(windows)]
    if JOB.get().is_some_and(Option::is_some) {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_BREAKAWAY_FROM_JOB;
        command.creation_flags(CREATE_BREAKAWAY_FROM_JOB);
    }
    command.spawn()
}

#[cfg(windows)]
mod win {
    use std::{io, mem, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::{JobObjects::*, Threading::GetCurrentProcess},
    };

    pub struct Handle(HANDLE);
    // The handle is never used after creation; it only keeps the job open.
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub fn contain_self() -> io::Result<Handle> {
        unsafe {
            let raw = CreateJobObjectW(ptr::null(), ptr::null());
            if raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Handle(raw);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
            limits.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            if SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                mem::size_of_val(&limits) as u32,
            ) == 0
                || AssignProcessToJobObject(job.0, GetCurrentProcess()) == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }
    }

    #[cfg(test)]
    pub(super) fn contains(job: &Handle, child: &std::process::Child) -> io::Result<bool> {
        use std::os::windows::io::AsRawHandle;
        let mut contained = 0;
        if unsafe { IsProcessInJob(child.as_raw_handle(), job.0, &mut contained) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(contained != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_claim_is_refused_until_the_first_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_claimed(dir.path()));
        let first = claim(dir.path()).unwrap();
        let err = claim(dir.path()).err().unwrap();
        assert_eq!(err.to_string(), BUSY);
        drop(first);
        claim(dir.path()).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn only_explicit_relaunch_breaks_away_from_the_desktop_job() {
        use std::process::Stdio;
        let dir = tempfile::tempdir().unwrap();
        let _owner = claim(dir.path()).unwrap();
        let Some(job) = JOB.get().and_then(Option::as_ref) else {
            // An enclosing Job can forbid containment.
            return;
        };
        let wait_for_input = || {
            let mut command = Command::new("cmd.exe");
            command
                .args(["/d", "/c", "set /p SA_TEST_INPUT="])
                .stdin(Stdio::piped());
            command
        };
        let mut ordinary = wait_for_input().spawn().unwrap();
        let contained = win::contains(job, &ordinary);
        ordinary.wait().unwrap();
        assert!(contained.unwrap());
        let mut detached = spawn_detached(wait_for_input()).unwrap();
        let contained = win::contains(job, &detached);
        detached.wait().unwrap();
        assert!(!contained.unwrap());
    }
}
