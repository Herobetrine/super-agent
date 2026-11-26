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
//!   terminates every harness CLI the process started.

use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::path::Path;
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

/// Allow an explicit breakaway only when relaunching immediately before exit.
/// Harness CLIs remain contained during normal operation.
pub fn allow_breakaway() -> std::io::Result<()> {
    #[cfg(windows)]
    if let Some(Some(job)) = JOB.get() {
        job.allow_breakaway()?;
    }
    Ok(())
}

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
    let Ok(f) = OpenOptions::new().read(true).write(true).open(dir.join(LOCK_FILE)) else {
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

#[cfg(windows)]
mod win {
    use std::{io, mem, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::{JobObjects::*, Threading::GetCurrentProcess},
    };

    pub struct Handle(HANDLE);
    // The handle stays open until process exit.
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}

    impl Handle {
        fn set_limits(&self, flags: JOB_OBJECT_LIMIT) -> io::Result<()> {
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = flags;
            // SAFETY: the handle owns a job object and the limits match this information class.
            if unsafe {
                SetInformationJobObject(
                    self.0,
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    mem::size_of_val(&limits) as u32,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub(super) fn allow_breakaway(&self) -> io::Result<()> {
            self.set_limits(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK)
        }
    }

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
            job.set_limits(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)?;
            if AssignProcessToJobObject(job.0, GetCurrentProcess()) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }
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
}
