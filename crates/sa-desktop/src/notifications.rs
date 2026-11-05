//! Native Windows toasts. The worker owns COM objects, so shell/WinRT work
//! never blocks an egui frame. Content contains only ticket ids and status.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

/// Poll all pending approvals independently of the selected conversation.
#[derive(Default)]
pub struct ApprovalWatch {
    pending: Option<crate::ops::Pending<Vec<(i64, i64)>>>,
    last: Option<std::time::Instant>,
    seen: Option<std::collections::HashSet<i64>>,
}

impl ApprovalWatch {
    pub fn poll(&mut self, ctx: &egui::Context, data: &std::path::Path) -> Vec<Notice> {
        let mut notices = Vec::new();
        if let Some(result) = self.pending.as_ref().and_then(crate::ops::Pending::poll) {
            self.pending = None;
            if let Ok(rows) = result {
                if let Some(seen) = &self.seen {
                    let tickets: std::collections::BTreeSet<i64> = rows
                        .iter()
                        .filter(|(id, _)| !seen.contains(id))
                        .map(|(_, ticket)| *ticket)
                        .collect();
                    notices.extend(tickets.into_iter().map(|ticket| Notice {
                        ticket,
                        approval: true,
                        status: String::new(),
                    }));
                }
                self.seen = Some(rows.iter().map(|(id, _)| *id).collect());
            }
        }
        if self.pending.is_none()
            && self
                .last
                .is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(1))
        {
            let data = data.to_path_buf();
            self.pending = Some(crate::ops::Pending::spawn(ctx, move || {
                let connection = rusqlite::Connection::open_with_flags(
                    data.join(sa_store::DB_FILE),
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )?;
                connection.busy_timeout(std::time::Duration::from_secs(1))?;
                let mut statement = connection
                    .prepare("SELECT id,ticket FROM approvals WHERE decision='pending'")?;
                let rows = statement
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            }));
            self.last = Some(std::time::Instant::now());
        }
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
        notices
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub ticket: i64,
    pub approval: bool,
    pub status: String,
}

impl Notice {
    pub fn title(&self) -> String {
        if self.approval {
            format!("工单 #{} 等待审批", self.ticket)
        } else {
            format!("工单 #{} {}", self.ticket, self.status)
        }
    }
}

pub enum Feedback {
    Open(i64),
    Failed(String),
}

#[derive(Default)]
pub struct Notifications {
    tx: Option<mpsc::SyncSender<Notice>>,
    rx: Option<mpsc::Receiver<Feedback>>,
    cancel: Option<Arc<AtomicBool>>,
    local_error: Option<String>,
}

impl Notifications {
    pub fn post(&mut self, ctx: &egui::Context, data: &std::path::Path, notice: Notice) {
        if self.tx.is_none() {
            let (tx, requests) = mpsc::sync_channel(32);
            let (feedback, rx) = mpsc::channel();
            let ctx = ctx.clone();
            let data = data.to_path_buf();
            let cancelled = Arc::new(AtomicBool::new(false));
            self.cancel = Some(cancelled.clone());
            std::thread::spawn(move || {
                if cancelled.load(Ordering::Acquire) {
                    return;
                }
                let send_error = |message: String| {
                    let _ = feedback.send(Feedback::Failed(message));
                    ctx.request_repaint();
                };
                let backend = native::Backend::new(&data);
                match backend {
                    Ok(mut backend) => {
                        for notice in requests {
                            if cancelled.load(Ordering::Acquire) {
                                break;
                            }
                            if let Err(error) = backend.show(notice, feedback.clone(), ctx.clone())
                            {
                                send_error(format!("系统通知发送失败：{error:#}"));
                            }
                        }
                    }
                    Err(error) => send_error(format!("系统通知初始化失败：{error:#}")),
                }
            });
            self.tx = Some(tx);
            self.rx = Some(rx);
        }
        if let Some(tx) = &self.tx {
            match tx.try_send(notice) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(_)) => {
                    self.local_error = Some("系统通知队列已满，请在工作台查看最新状态".into())
                }
                Err(mpsc::TrySendError::Disconnected(_)) => self.tx = None,
            }
        }
    }

    pub fn poll(&mut self) -> Vec<Feedback> {
        let mut feedback: Vec<Feedback> = self
            .rx
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        if let Some(error) = self.local_error.take() {
            feedback.push(Feedback::Failed(error));
        }
        feedback
    }

    pub fn stop(&mut self) {
        if let Some(cancelled) = self.cancel.take() {
            cancelled.store(true, Ordering::Release);
        }
        self.tx = None;
    }
}

impl Drop for Notifications {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(windows)]
mod native {
    use super::{Feedback, Notice};
    use anyhow::{Context, Result};
    use sha2::{Digest, Sha256};
    use std::{
        collections::VecDeque,
        path::{Path, PathBuf},
        sync::mpsc,
    };
    use windows::{
        Data::Xml::Dom::XmlDocument,
        Foundation::TypedEventHandler,
        UI::Notifications::{ToastNotification, ToastNotificationManager, ToastNotifier},
        Win32::{
            Storage::EnhancedStorage::PKEY_AppUserModel_ID,
            System::{
                Com::{
                    CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, IPersistFile,
                    StructuredStorage::PROPVARIANT,
                },
                Registry::{HKEY_CURRENT_USER, REG_SZ, RegSetKeyValueW},
                WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize},
            },
            UI::Shell::{
                FOLDERID_Programs, IShellLinkW, KF_FLAG_DEFAULT, PropertiesSystem::IPropertyStore,
                SHGetKnownFolderPath, ShellLink,
            },
        },
        core::{HSTRING, Interface, PCWSTR},
    };

    const APP_ID: &str = "Herobetrine.SuperAgent";

    struct Apartment;
    impl Apartment {
        fn new() -> Result<Self> {
            unsafe {
                RoInitialize(RO_INIT_MULTITHREADED)?;
            }
            Ok(Self)
        }
    }
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe {
                RoUninitialize();
            }
        }
    }

    fn wide(path: &Path) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn programs() -> Result<PathBuf> {
        unsafe {
            let raw = SHGetKnownFolderPath(&FOLDERID_Programs, KF_FLAG_DEFAULT, None)?;
            let value = raw.to_string();
            CoTaskMemFree(Some(raw.0.cast()));
            Ok(PathBuf::from(value?))
        }
    }

    /// The shell requires a Start-menu link carrying this application's
    /// AUMID. Refresh the owned link atomically after an executable move.
    fn register(id: &str, name: &str) -> Result<PathBuf> {
        let directory = programs()?.join("Herobetrine Super Agent");
        std::fs::create_dir_all(&directory)?;
        let target = std::env::current_exe()?;
        let path = directory.join(format!("{name}.lnk"));
        let temporary = tempfile::Builder::new()
            .prefix(".superagent-")
            .suffix(".lnk")
            .tempfile_in(&directory)?;
        unsafe {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(PCWSTR(wide(&target).as_ptr()))?;
            link.SetWorkingDirectory(PCWSTR(
                wide(target.parent().context("程序路径没有父目录")?).as_ptr(),
            ))?;
            link.SetDescription(&HSTRING::from("Super Agent"))?;
            let properties: IPropertyStore = link.cast()?;
            let value = PROPVARIANT::from(id);
            properties.SetValue(&PKEY_AppUserModel_ID, &value)?;
            properties.Commit()?;
            let file: IPersistFile = link.cast()?;
            file.Save(PCWSTR(wide(temporary.path()).as_ptr()), true)?;
        }
        temporary.persist(&path).map_err(|error| error.error)?;
        // Register immediately for Windows 10/11. A newly written shortcut
        // may not have entered the shell's application catalog yet.
        let subkey = HSTRING::from(format!("Software\\Classes\\AppUserModelId\\{id}"));
        let display: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &subkey,
                &HSTRING::from("DisplayName"),
                REG_SZ.0,
                Some(display.as_ptr().cast()),
                (display.len() * 2) as u32,
            )
            .ok()?;
        }
        Ok(path)
    }

    pub struct Backend {
        notifier: ToastNotifier,
        group: HSTRING,
        retained: VecDeque<ToastNotification>,
        // Drop WinRT objects before uninitializing the apartment.
        _apartment: Apartment,
    }

    impl Backend {
        pub fn new(data: &Path) -> Result<Self> {
            let apartment = Apartment::new()?;
            register(APP_ID, "Super Agent")?;
            Self::with_id(APP_ID, data, apartment)
        }

        fn with_id(id: &str, data: &Path, apartment: Apartment) -> Result<Self> {
            let canonical = std::fs::canonicalize(data).unwrap_or_else(|_| data.to_path_buf());
            static INSTANCES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let instance = INSTANCES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut identity = canonical.as_os_str().as_encoded_bytes().to_vec();
            identity.extend_from_slice(&std::process::id().to_le_bytes());
            identity.extend_from_slice(&instance.to_le_bytes());
            let digest = Sha256::digest(identity);
            let group = digest[..8]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            Ok(Self {
                notifier: ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(id))?,
                group: HSTRING::from(group),
                retained: VecDeque::new(),
                _apartment: apartment,
            })
        }

        pub fn show(
            &mut self,
            notice: Notice,
            feedback: mpsc::Sender<Feedback>,
            ctx: egui::Context,
        ) -> Result<()> {
            // Windows may return ELEMENT_NOT_FOUND from Setting() until the
            // first notification creates its settings record. Submit through
            // Show, as in the native desktop sample, and observe Failed.
            let document = XmlDocument::new()?;
            document.LoadXml(&HSTRING::from("<toast><visual><binding template=\"ToastGeneric\"><text/><text/></binding></visual><audio silent=\"true\"/></toast>"))?;
            let text = document.GetElementsByTagName(&HSTRING::from("text"))?;
            text.Item(0)?
                .AppendChild(&document.CreateTextNode(&HSTRING::from(notice.title()))?)?;
            text.Item(1)?
                .AppendChild(&document.CreateTextNode(&HSTRING::from("点击返回工作台查看"))?)?;
            let toast = ToastNotification::CreateToastNotification(&document)?;
            toast.SetGroup(&self.group)?;
            let digest = Sha256::digest(format!("{}:{}", notice.ticket, notice.approval));
            let tag = digest[..8]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            toast.SetTag(&HSTRING::from(tag))?;
            let (clicked, repaint) = (feedback.clone(), ctx.clone());
            toast.Activated(&TypedEventHandler::new(move |_, _| {
                let _ = clicked.send(Feedback::Open(notice.ticket));
                repaint.request_repaint();
                Ok(())
            }))?;
            toast.Failed(&TypedEventHandler::new(move |_, _| {
                let _ = feedback.send(Feedback::Failed("Windows 未能显示系统通知".into()));
                ctx.request_repaint();
                Ok(())
            }))?;
            self.notifier.Show(&toast).context("提交 Windows 通知")?;
            self.retained.push_back(toast);
            if self.retained.len() > 32
                && let Some(old) = self.retained.pop_front()
            {
                let _ = self.notifier.Hide(&old);
            }
            Ok(())
        }
    }

    impl Drop for Backend {
        fn drop(&mut self) {
            for toast in &self.retained {
                let _ = self.notifier.Hide(toast);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use anyhow::bail;

        #[test]
        #[ignore = "displays a real Windows toast using a disposable app identity"]
        fn windows_toast_reaches_notification_history() {
            let apartment = Apartment::new().unwrap();
            let name = format!("Super Agent verification {}", std::process::id());
            let id = format!("Herobetrine.SuperAgent.Check.{}", std::process::id());
            let shortcut = register(&id, &name).unwrap();
            let data = tempfile::tempdir().unwrap();
            let mut backend = Backend::with_id(&id, data.path(), apartment).unwrap();
            let (tx, rx) = mpsc::channel();
            let result = (|| -> Result<()> {
                backend.show(
                    Notice {
                        ticket: 1,
                        approval: false,
                        status: "通知验证".into(),
                    },
                    tx,
                    egui::Context::default(),
                )?;
                let history = ToastNotificationManager::History()?;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                loop {
                    if let Ok(Feedback::Failed(error)) = rx.try_recv() {
                        bail!(error);
                    }
                    if history
                        .GetHistoryWithId(&HSTRING::from(&id))
                        .context("读取通知历史")?
                        .Size()?
                        > 0
                    {
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        bail!("系统通知未进入 Windows 通知历史");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                history.ClearWithId(&HSTRING::from(&id))?;
                Ok(())
            })();
            drop(backend);
            std::fs::remove_file(shortcut).unwrap();
            unsafe {
                windows::Win32::System::Registry::RegDeleteTreeW(
                    HKEY_CURRENT_USER,
                    &HSTRING::from(format!("Software\\Classes\\AppUserModelId\\{id}")),
                )
                .ok()
                .unwrap();
            }
            result.unwrap();
        }
    }
}

#[cfg(not(windows))]
mod native {
    use super::{Feedback, Notice};
    pub struct Backend;
    impl Backend {
        pub fn new(_: &std::path::Path) -> anyhow::Result<Self> {
            anyhow::bail!("此平台尚未接入系统通知")
        }
        pub fn show(
            &mut self,
            _: Notice,
            _: std::sync::mpsc::Sender<Feedback>,
            _: egui::Context,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_contains_only_ticket_and_status() {
        assert_eq!(
            Notice {
                ticket: 8,
                approval: true,
                status: String::new()
            }
            .title(),
            "工单 #8 等待审批"
        );
        assert_eq!(
            Notice {
                ticket: 8,
                approval: false,
                status: "已完成".into()
            }
            .title(),
            "工单 #8 已完成"
        );
    }

    #[test]
    fn approvals_include_background_tickets_and_same_count_replacements() {
        let directory = tempfile::tempdir().unwrap();
        let connection =
            rusqlite::Connection::open(directory.path().join(sa_store::DB_FILE)).unwrap();
        connection.execute_batch("CREATE TABLE approvals(id INTEGER PRIMARY KEY, ticket INTEGER, decision TEXT); INSERT INTO approvals VALUES(1,101,'pending');").unwrap();
        let context = egui::Context::default();
        let mut watch = ApprovalWatch::default();
        let drain = |watch: &mut ApprovalWatch| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            let mut notices = Vec::new();
            watch.last = None;
            notices.extend(watch.poll(&context, directory.path()));
            while watch.pending.is_some() {
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(5));
                notices.extend(watch.poll(&context, directory.path()));
            }
            notices
        };
        assert!(
            drain(&mut watch).is_empty(),
            "initial pending approvals establish a baseline"
        );
        connection.execute_batch("UPDATE approvals SET decision='allow' WHERE id=1; INSERT INTO approvals VALUES(2,202,'pending');").unwrap();
        let notices = drain(&mut watch);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].ticket, 202);
        assert!(notices[0].approval);
        assert!(
            drain(&mut watch).is_empty(),
            "same pending approval is not repeated"
        );
        connection
            .execute_batch("INSERT INTO approvals VALUES(3,202,'pending'),(4,303,'pending');")
            .unwrap();
        assert_eq!(
            drain(&mut watch)
                .iter()
                .map(|notice| notice.ticket)
                .collect::<Vec<_>>(),
            [202, 303]
        );
    }
}
