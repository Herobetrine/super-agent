//! Short-lived validation process ownership. Windows children enter a private
//! Job Object before any user code runs. Normal completion also kills and waits
//! for descendants; a parent exiting alone is not a complete validation run.
use anyhow::{Context, Result};
use std::process::{Child, Command, ExitStatus};

pub(super) struct OwnedProcess {
    child: Child,
    #[cfg(windows)]
    job: windows::Job,
    closed: bool,
}

impl OwnedProcess {
    pub(super) fn spawn(command: &mut Command) -> Result<Self> {
        #[cfg(windows)]
        let job = {
            use std::os::windows::process::CommandExt;
            use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};
            command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
            windows::Job::new()?
        };
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command.spawn().context("无法启动验证命令或 Git")?;
        let process = Self {
            child,
            #[cfg(windows)]
            job,
            closed: false,
        };
        // If assignment or resumption fails, Drop kills the still-suspended
        // child. No uncontained fallback is permitted.
        #[cfg(windows)]
        process.job.assign_and_resume(&process.child)?;
        Ok(process)
    }

    pub(super) fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub(super) fn shutdown(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        #[cfg(windows)]
        self.job.terminate_and_wait()?;
        #[cfg(unix)]
        {
            // Best effort outside Windows: a child that deliberately creates
            // another session can escape a process group, unlike a Windows job.
            let _ = Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{}", self.child.id())])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        let _ = self.child.kill();
        self.child.wait().context("无法确认验证父进程退出")?;
        self.closed = true;
        Ok(())
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        #[cfg(windows)]
        let _ = self.job.terminate_and_wait();
        #[cfg(unix)]
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{}", self.child.id())])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(windows)]
mod windows {
    use anyhow::{Context, Result, ensure};
    use std::{
        io, mem,
        os::windows::io::AsRawHandle,
        process::Child,
        ptr,
        time::{Duration, Instant},
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            JobObjects::*,
            Threading::{
                GetProcessIdOfThread, OpenThread, ResumeThread, THREAD_QUERY_LIMITED_INFORMATION,
                THREAD_SUSPEND_RESUME,
            },
        },
    };
    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    pub(super) struct Job(Handle);
    impl Job {
        pub(super) fn new() -> Result<Self> {
            unsafe {
                let raw = CreateJobObjectW(ptr::null(), ptr::null());
                ensure!(
                    !raw.is_null(),
                    "无法创建验证进程组：{}",
                    io::Error::last_os_error()
                );
                let job = Self(Handle(raw));
                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                ensure!(
                    SetInformationJobObject(
                        raw,
                        JobObjectExtendedLimitInformation,
                        (&raw const limits).cast(),
                        mem::size_of_val(&limits) as u32
                    ) != 0,
                    "无法设置验证进程组：{}",
                    io::Error::last_os_error()
                );
                Ok(job)
            }
        }
        pub(super) fn assign_and_resume(&self, child: &Child) -> Result<()> {
            unsafe {
                ensure!(
                    AssignProcessToJobObject(self.0.0, child.as_raw_handle().cast()) != 0,
                    "无法约束验证进程，已停止启动：{}",
                    io::Error::last_os_error()
                );
                let raw = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
                ensure!(
                    raw != INVALID_HANDLE_VALUE,
                    "无法读取验证主线程：{}",
                    io::Error::last_os_error()
                );
                let snapshot = Handle(raw);
                let mut entry: THREADENTRY32 = mem::zeroed();
                entry.dwSize = mem::size_of::<THREADENTRY32>() as u32;
                let mut more = Thread32First(snapshot.0, &mut entry);
                let mut ids = Vec::new();
                while more != 0 {
                    if entry.th32OwnerProcessID == child.id() {
                        ids.push(entry.th32ThreadID);
                    }
                    more = Thread32Next(snapshot.0, &mut entry);
                }
                ensure!(ids.len() == 1, "挂起的验证进程主线程不唯一，已停止启动");
                let raw = OpenThread(
                    THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
                    0,
                    ids[0],
                );
                ensure!(
                    !raw.is_null(),
                    "无法打开验证主线程：{}",
                    io::Error::last_os_error()
                );
                let thread = Handle(raw);
                ensure!(
                    GetProcessIdOfThread(thread.0) == child.id(),
                    "验证主线程身份已改变"
                );
                ensure!(ResumeThread(thread.0) == 1, "无法安全恢复验证主线程");
                Ok(())
            }
        }
        pub(super) fn terminate_and_wait(&self) -> Result<()> {
            unsafe {
                ensure!(
                    TerminateJobObject(self.0.0, 1223) != 0,
                    "无法终止验证进程组：{}",
                    io::Error::last_os_error()
                );
                let start = Instant::now();
                loop {
                    let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = mem::zeroed();
                    if QueryInformationJobObject(
                        self.0.0,
                        JobObjectBasicAccountingInformation,
                        (&raw mut info).cast(),
                        mem::size_of_val(&info) as u32,
                        ptr::null_mut(),
                    ) == 0
                    {
                        return Err(io::Error::last_os_error()).context("无法确认验证子进程退出");
                    }
                    if info.ActiveProcesses == 0 {
                        return Ok(());
                    }
                    ensure!(
                        start.elapsed() < Duration::from_secs(5),
                        "验证子进程退出超时；不发布通过回执"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };

    fn live_tree(explicit_shutdown: bool) {
        let directory = tempfile::tempdir().unwrap();
        let started = directory.path().join("started");
        let escaped = directory.path().join("escaped");
        let child = "const fs=require('node:fs');fs.writeFileSync(process.argv[1],'ready');setTimeout(()=>fs.writeFileSync(process.argv[2],'escaped'),700);";
        let parent = format!(
            "require('node:child_process').spawn(process.execPath,['-e',{},process.argv[1],process.argv[2]],{{stdio:'ignore',windowsHide:true}});setInterval(()=>{{}},1000);",
            serde_json::to_string(child).unwrap()
        );
        let mut command = Command::new("node.exe");
        command
            .args(["-e", &parent])
            .arg(&started)
            .arg(&escaped)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut process = OwnedProcess::spawn(&mut command).unwrap();
        let begin = Instant::now();
        while !started.exists() {
            assert!(
                begin.elapsed() < Duration::from_secs(10),
                "fixture child did not start"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        if explicit_shutdown {
            process.shutdown().unwrap();
        }
        drop(process);
        std::thread::sleep(Duration::from_millis(900));
        assert!(!escaped.exists(), "descendant survived cleanup");
    }

    #[test]
    fn explicit_shutdown_reaps_running_tree() {
        live_tree(true);
    }

    #[test]
    fn error_unwind_drop_reaps_running_tree() {
        live_tree(false);
    }
}
