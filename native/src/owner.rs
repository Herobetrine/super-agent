use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    fs::{self, File, OpenOptions},
    path::PathBuf,
};

#[derive(Deserialize)]
pub struct Claim {
    directory: PathBuf,
    pid: u32,
}
pub struct Owner {
    _lock: File,
    #[cfg(windows)]
    _job: win::Handle,
}

pub fn claim(req: Claim) -> Result<Owner> {
    fs::create_dir_all(&req.directory)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(req.directory.join("desktop.lock"))?;
    lock.try_lock()
        .context("此工单库已有工作台在运行，请先关闭旧窗口")?;
    Ok(Owner {
        _lock: lock,
        #[cfg(windows)]
        _job: win::contain(req.pid)?,
    })
}

#[cfg(windows)]
mod win {
    use anyhow::Result;
    use std::{io, mem, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::{
            JobObjects::*,
            Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE},
        },
    };
    pub struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    fn checked(raw: HANDLE) -> io::Result<Handle> {
        if raw.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Handle(raw))
        }
    }
    pub fn contain(pid: u32) -> Result<Handle> {
        // Rust holds the job handle. If the broker or desktop exits abnormally,
        // its stdio closes and Windows terminates the remaining CLI descendants.
        unsafe {
            let process = checked(OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid))?;
            let job = checked(CreateJobObjectW(ptr::null(), ptr::null()))?;
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                mem::size_of_val(&limits) as u32,
            ) == 0
                || AssignProcessToJobObject(job.0, process.0) == 0
            {
                return Err(io::Error::last_os_error().into());
            }
            Ok(job)
        }
    }
}
