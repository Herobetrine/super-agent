//! Journal watches and native Windows notifications.

#[path = "notification_badge.rs"]
mod badge;
#[path = "notification_cards.rs"]
mod cards;
#[path = "notification_gate.rs"]
mod gate;

pub(crate) use badge::Badge;
pub(crate) use cards::Cards;
pub(crate) use gate::{Delivery, Gate, Mode, Preferences, Visual, Watch, Window};

use std::collections::HashSet;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

#[derive(Clone, Copy)]
struct PendingRequest {
    id: i64,
    ticket: i64,
    kind: NoticeKind,
}

/// Each consumer keeps its own baseline over the shared journal rows.
#[derive(Default)]
struct PendingWatch {
    seen: Option<HashSet<(NoticeKind, i64)>>,
}

impl PendingWatch {
    fn observe(&mut self, rows: &[PendingRequest]) -> Vec<Notice> {
        let current = rows.iter().map(|row| (row.kind, row.id)).collect();
        let Some(seen) = self.seen.replace(current) else {
            return Vec::new();
        };
        let mut emitted = HashSet::new();
        let mut notices: Vec<_> = rows.iter().rev()
            .filter(|row| !seen.contains(&(row.kind, row.id)) && emitted.insert((row.kind, row.ticket)))
            .map(|row| Notice { ticket: row.ticket, kind: row.kind, status: String::new() })
            .collect();
        notices.reverse();
        notices
    }
}

fn open_journal(path: &std::path::Path) -> rusqlite::Result<rusqlite::Connection> {
    let connection = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    connection.busy_timeout(std::time::Duration::from_secs(1))?;
    Ok(connection)
}

fn table_exists(connection: &rusqlite::Connection, name: &str) -> rusqlite::Result<bool> {
    connection.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1")?
        .exists([name])
}

fn read_pending(connection: &rusqlite::Connection) -> rusqlite::Result<Vec<PendingRequest>> {
    let mut sources = Vec::new();
    let has_events = table_exists(connection, "events")?;
    for (tag, table, status, event, field) in [
        (0, "approvals", "decision", "approval.request", "approval"),
        (1, "questions", "state", "ask", "id"),
    ] {
        if !table_exists(connection, table)? {
            continue;
        }
        // Global journal seq orders approvals and questions together. Legacy
        // requests without a journal event fall back to their per-table ids.
        let order = if has_events {
            format!("(SELECT e.seq FROM events e WHERE e.ticket=p.ticket AND e.kind='{event}'
                     AND CASE WHEN json_valid(e.payload) THEN json_extract(e.payload,'$.{field}') END=p.id
                     ORDER BY e.seq DESC LIMIT 1)")
        } else {
            "NULL".into()
        };
        sources.push(format!("SELECT {tag},p.id,p.ticket,{order} FROM {table} p WHERE p.{status}='pending'"));
    }
    if sources.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!("{} ORDER BY 4,2,1", sources.join(" UNION ALL "));
    #[cfg(test)]
    pending_query_probe::record(connection.path());
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map([], |row| Ok(PendingRequest {
        kind: if row.get::<_, i64>(0)? == 0 { NoticeKind::Approval } else { NoticeKind::Question },
        id: row.get(1)?,
        ticket: row.get(2)?,
    }))?;
    rows.collect()
}

#[cfg(test)]
pub(crate) mod pending_query_probe {
    use std::{collections::BTreeMap, ffi::OsString, path::Path, sync::Mutex};

    static COUNTS: Mutex<BTreeMap<OsString, usize>> = Mutex::new(BTreeMap::new());

    pub(crate) struct Probe(OsString);

    impl Probe {
        pub(crate) fn start(data: &Path) -> Self {
            let key = data.file_name().unwrap().to_owned();
            COUNTS.lock().unwrap().insert(key.clone(), 0);
            Self(key)
        }

        pub(crate) fn count(&self) -> usize {
            COUNTS.lock().unwrap()[&self.0]
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            COUNTS.lock().unwrap().remove(&self.0);
        }
    }

    pub(super) fn record(database: Option<&str>) {
        if let Some(dir) = database.map(Path::new).and_then(Path::parent)
            .and_then(Path::file_name)
            && let Some(count) = COUNTS.lock().unwrap().get_mut(dir)
        {
            *count += 1;
        }
    }
}

type CompletionBatch = (i64, Vec<(i64, String)>);

/// Observe root jobs independently of the sidebar's filtering and pagination.
#[derive(Default)]
pub struct CompletionWatch {
    epoch: Option<u64>,
    pending: Option<crate::ops::Pending<CompletionBatch>>,
    last: Option<std::time::Instant>,
    cursor: Option<i64>,
}

impl CompletionWatch {
    pub fn set_epoch(&mut self, epoch: Option<u64>) {
        if self.epoch != epoch {
            *self = Self { epoch, ..Self::default() };
        }
    }

    pub fn poll(&mut self, ctx: &egui::Context, data: &std::path::Path) -> Vec<(i64, String)> {
        let mut finished = Vec::new();
        if let Some(result) = self.pending.as_ref().and_then(crate::ops::Pending::poll) {
            self.pending = None;
            if let Ok((head, endings)) = result {
                self.cursor = Some(head);
                finished = endings;
            }
        }
        if self.pending.is_none()
            && self
                .last
                .is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(1))
        {
            let path = data.join(sa_store::DB_FILE);
            let cursor = self.cursor;
            self.pending = Some(crate::ops::Pending::spawn(ctx, move || {
                read_completions(&path, cursor)
            }));
            self.last = Some(std::time::Instant::now());
        }
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
        finished
    }

    #[cfg(test)]
    pub(crate) fn cursor(&self) -> Option<i64> {
        self.cursor
    }

    #[cfg(test)]
    pub(crate) fn is_polling(&self) -> bool {
        self.last.is_some()
    }
}

fn read_completions(path: &std::path::Path, cursor: Option<i64>) -> anyhow::Result<CompletionBatch> {
    let mut conn = open_journal(path)?;
    let tx = conn.transaction()?;
    let head = completion_head(&tx)?;
    let rows = read_completion_rows(&tx, cursor, head)?;
    Ok((head, rows.into_iter().map(|row| (row.ticket, row.state)).collect()))
}

struct CompletionRow {
    seq: i64,
    ticket: i64,
    job: i64,
    turn: i64,
    state: String,
}

fn completion_head(conn: &rusqlite::Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT coalesce(max(seq), 0) FROM events", [], |r| r.get(0))
}

fn read_completion_rows(
    conn: &rusqlite::Connection,
    cursor: Option<i64>,
    head: i64,
) -> anyhow::Result<Vec<CompletionRow>> {
    let Some(after) = cursor.filter(|&after| after < head) else {
        return Ok(Vec::new());
    };
    // A reply or pause failure carries its resulting state in the payload.
    // Dedicated failure/interruption events encode the state in their kind.
    // CROSS JOIN keeps SQLite on the new event rowid range before root lookup.
    let mut stmt = conn.prepare(
        "SELECT seq, ticket, job, turn, state FROM (
           SELECT e.seq, e.ticket, e.job, coalesce(e.turn, j.turn) AS turn, CASE
             WHEN e.kind IN ('reply', 'turn.pause_failure') THEN
               CASE WHEN json_valid(e.payload) THEN json_extract(e.payload, '$.state') END
             ELSE e.kind END AS state
           FROM events e CROSS JOIN jobs j ON j.id=e.job AND j.ticket=e.ticket
           WHERE e.seq>?1 AND e.seq<=?2 AND j.parent IS NULL
             AND e.kind IN ('reply', 'turn.pause_failure', 'done', 'failed', 'partial', 'interrupted')
         ) WHERE state IN ('done', 'failed', 'partial', 'interrupted') ORDER BY seq",
    )?;
    let rows = stmt.query_map([after, head], |r| Ok(CompletionRow {
        seq: r.get(0)?,
        ticket: r.get(1)?,
        job: r.get(2)?,
        turn: r.get(3)?,
        state: r.get(4)?,
    }))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoticeKind {
    Approval,
    Question,
    Completion,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub ticket: i64,
    pub kind: NoticeKind,
    pub status: String,
}

impl Notice {
    pub fn title(&self) -> String {
        match self.kind {
            NoticeKind::Approval => format!("工单 #{} 待审批", self.ticket),
            NoticeKind::Question => "需要你的回答".into(),
            NoticeKind::Completion => format!("工单 #{} {}", self.ticket, self.status),
        }
    }

    #[cfg(any(windows, test))]
    fn detail(&self) -> String {
        format!("工单 #{} · 点击返回工作台查看", self.ticket)
    }
}

pub(crate) enum Request {
    Toast(ToastRequest),
    Sound,
}

fn deliver_pending(
    requests: impl IntoIterator<Item = Request>,
    cancelled: &AtomicBool,
    mut deliver: impl FnMut(Request),
) {
    for request in requests {
        if cancelled.load(Ordering::Acquire) {
            break;
        }
        deliver(request);
    }
}

pub(crate) struct ToastRequest {
    pub notice: Notice,
    pub sound: bool,
}

#[cfg(all(windows, test))]
impl From<Notice> for ToastRequest {
    fn from(notice: Notice) -> Self {
        Self { notice, sound: false }
    }
}

pub enum Feedback {
    Open(i64),
    Failed(String),
}

#[derive(Default)]
pub struct Notifications {
    tx: Option<mpsc::SyncSender<Request>>,
    rx: Option<mpsc::Receiver<Feedback>>,
    cancel: Option<Arc<AtomicBool>>,
    local_error: Option<String>,
}

impl Notifications {
    #[cfg(test)]
    pub(crate) fn test_worker(&mut self) -> mpsc::Receiver<Request> {
        let (tx, rx) = mpsc::sync_channel(32);
        self.tx = Some(tx);
        self.cancel = Some(Arc::new(AtomicBool::new(false)));
        rx
    }

    #[cfg(test)]
    pub(crate) fn test_queued_sounds(&mut self) -> impl FnOnce() -> usize + use<> {
        let requests = self.test_worker();
        let cancelled = self.cancel.as_ref().unwrap().clone();
        for _ in 0..2 {
            assert!(self.tx.as_ref().unwrap().try_send(Request::Sound).is_ok());
        }
        move || {
            let mut delivered = 0;
            deliver_pending(requests.try_iter(), &cancelled, |_| delivered += 1);
            delivered
        }
    }

    pub fn post(&mut self, ctx: &egui::Context, data: &std::path::Path, request: Request) {
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
                let mut backend = None;
                deliver_pending(requests, &cancelled, |request| {
                    match request {
                        Request::Sound => native::play_sound(),
                        Request::Toast(toast) => {
                            if backend.is_none() {
                                match native::Backend::new(&data) {
                                    Ok(ready) => backend = Some(ready),
                                    Err(error) => {
                                        send_error(format!("系统通知初始化失败：{error:#}"));
                                        return;
                                    }
                                }
                            }
                            if let Some(backend) = &mut backend
                                && !cancelled.load(Ordering::Acquire)
                                && let Err(error) = backend.show(toast, feedback.clone(), ctx.clone())
                            {
                                send_error(format!("系统通知发送失败：{error:#}"));
                            }
                        }
                    }
                });
            });
            self.tx = Some(tx);
            self.rx = Some(rx);
        }
        if let Some(tx) = &self.tx {
            match tx.try_send(request) {
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
    use super::{Feedback, ToastRequest};
    #[cfg(test)]
    use super::Notice;
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
            Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_NODEFAULT, SND_SYSTEM},
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
        core::{HSTRING, Interface, PCWSTR, w},
    };

    const APP_ID: &str = "Herobetrine.SuperAgent";

    pub fn play_sound() {
        unsafe {
            let _ = PlaySoundW(
                w!("Notification.Default"),
                None,
                SND_ALIAS | SND_ASYNC | SND_NODEFAULT | SND_SYSTEM,
            );
        }
    }

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
            request: impl Into<ToastRequest>,
            feedback: mpsc::Sender<Feedback>,
            ctx: egui::Context,
        ) -> Result<()> {
            let ToastRequest { notice, sound } = request.into();
            // Windows may return ELEMENT_NOT_FOUND from Setting() until the
            // first notification creates its settings record. Submit through
            // Show, as in the native desktop sample, and observe Failed.
            let document = XmlDocument::new()?;
            let audio = if sound {
                "<audio src=\"ms-winsoundevent:Notification.Default\"/>"
            } else {
                "<audio silent=\"true\"/>"
            };
            document.LoadXml(&HSTRING::from(format!(
                "<toast><visual><binding template=\"ToastGeneric\"><text/><text/></binding></visual>{audio}</toast>"
            )))?;
            let text = document.GetElementsByTagName(&HSTRING::from("text"))?;
            text.Item(0)?
                .AppendChild(&document.CreateTextNode(&HSTRING::from(notice.title()))?)?;
            text.Item(1)?
                .AppendChild(&document.CreateTextNode(&HSTRING::from(notice.detail()))?)?;
            let toast = ToastNotification::CreateToastNotification(&document)?;
            toast.SetGroup(&self.group)?;
            let digest = Sha256::digest(format!("{}:{}", notice.ticket, notice.kind as u8));
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

        struct QuestionToastIdentity {
            id: String,
            shortcut: PathBuf,
            _apartment: Apartment,
        }

        impl QuestionToastIdentity {
            fn cleanup(&self) -> Result<()> {
                ToastNotificationManager::History()?.ClearWithId(&HSTRING::from(&self.id))?;
                if self.shortcut.exists() {
                    std::fs::remove_file(&self.shortcut)?;
                }
                unsafe {
                    windows::Win32::System::Registry::RegDeleteTreeW(
                        HKEY_CURRENT_USER,
                        &HSTRING::from(format!(
                            "Software\\Classes\\AppUserModelId\\{}",
                            self.id
                        )),
                    )
                    .ok()?;
                }
                Ok(())
            }
        }

        impl Drop for QuestionToastIdentity {
            fn drop(&mut self) {
                // Preserve the original assertion error while attempting every cleanup.
                if let Ok(history) = ToastNotificationManager::History() {
                    let _ = history.ClearWithId(&HSTRING::from(&self.id));
                }
                let _ = std::fs::remove_file(&self.shortcut);
                unsafe {
                    let _ = windows::Win32::System::Registry::RegDeleteTreeW(
                        HKEY_CURRENT_USER,
                        &HSTRING::from(format!(
                            "Software\\Classes\\AppUserModelId\\{}",
                            self.id
                        )),
                    );
                }
            }
        }

        #[test]
        #[ignore = "displays a real question toast using a disposable journal and app identity"]
        fn windows_question_toast_reaches_notification_history() -> Result<()> {
            let data = tempfile::tempdir()?;
            let suffix = data
                .path()
                .file_name()
                .context("临时目录缺少名称")?
                .to_string_lossy();
            let name = format!(
                "Super Agent question verification {} {suffix}",
                std::process::id()
            );
            let identity = QuestionToastIdentity {
                id: format!(
                    "Herobetrine.SuperAgent.QuestionCheck.{}.{suffix}",
                    std::process::id()
                ),
                shortcut: programs()?
                    .join("Herobetrine Super Agent")
                    .join(format!("{name}.lnk")),
                _apartment: Apartment::new()?,
            };
            register(&identity.id, &name)?;
            let connection = rusqlite::Connection::open(data.path().join(sa_store::DB_FILE))?;
            connection.execute_batch(
                "CREATE TABLE questions(id INTEGER PRIMARY KEY, ticket INTEGER, state TEXT, question TEXT);",
            )?;
            let context = egui::Context::default();
            let mut watch = crate::notifications::PendingWatch::default();
            let mut drain = || -> Result<Vec<Notice>> {
                Ok(watch.observe(&crate::notifications::read_pending(&connection)?))
            };
            assert!(
                drain()?.is_empty(),
                "startup establishes a baseline"
            );
            let private_question = "PRIVATE QUESTION CONTENT MUST STAY IN THE JOURNAL";
            connection.execute(
                "INSERT INTO questions VALUES(1,731,'pending',?)",
                [private_question],
            )?;
            let mut notices = drain()?;
            assert_eq!(
                notices,
                vec![Notice {
                    ticket: 731,
                    kind: crate::notifications::NoticeKind::Question,
                    status: String::new(),
                }]
            );
            assert!(
                drain()?.is_empty(),
                "pending question is not repeated"
            );
            let mut backend = Backend::with_id(&identity.id, data.path(), Apartment::new()?)?;
            let (tx, rx) = mpsc::channel();
            backend.show(notices.remove(0), tx, context)?;
            let history = ToastNotificationManager::History()?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if let Ok(Feedback::Failed(error)) = rx.try_recv() {
                    bail!(error);
                }
                let notifications = history.GetHistoryWithId(&HSTRING::from(&identity.id))?;
                if notifications.Size()? > 0 {
                    assert_eq!(notifications.Size()?, 1);
                    let document = notifications.GetAt(0)?.Content()?;
                    let text = document.GetElementsByTagName(&HSTRING::from("text"))?;
                    assert_eq!(text.Length()?, 2);
                    assert_eq!(text.Item(0)?.InnerText()?.to_string(), "需要你的回答");
                    assert_eq!(
                        text.Item(1)?.InnerText()?.to_string(),
                        "工单 #731 · 点击返回工作台查看"
                    );
                    assert!(!document.GetXml()?.to_string().contains(private_question));
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    bail!("问题通知未进入 Windows 通知历史");
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            drop(backend);
            identity.cleanup()?;
            Ok(())
        }

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
                        kind: crate::notifications::NoticeKind::Completion,
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
    use super::{Feedback, ToastRequest};
    pub fn play_sound() {}
    pub struct Backend;
    impl Backend {
        pub fn new(_: &std::path::Path) -> anyhow::Result<Self> {
            Ok(Self)
        }
        pub fn show(
            &mut self,
            request: ToastRequest,
            _: std::sync::mpsc::Sender<Feedback>,
            _: egui::Context,
        ) -> anyhow::Result<()> {
            let ToastRequest { notice, sound } = request;
            let _ = (notice, sound);
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
                kind: NoticeKind::Approval,
                status: String::new()
            }
            .title(),
            "工单 #8 待审批"
        );
        assert_eq!(
            Notice {
                ticket: 8,
                kind: NoticeKind::Completion,
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
        let mut watch = PendingWatch::default();
        let mut drain = || watch.observe(&read_pending(&connection).unwrap());
        assert!(
            drain().is_empty(),
            "initial pending approvals establish a baseline"
        );
        connection.execute_batch("UPDATE approvals SET decision='allow' WHERE id=1; INSERT INTO approvals VALUES(2,202,'pending');").unwrap();
        let notices = drain();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].ticket, 202);
        assert_eq!(notices[0].kind, NoticeKind::Approval);
        assert!(
            drain().is_empty(),
            "same pending approval is not repeated"
        );
        connection
            .execute_batch("INSERT INTO approvals VALUES(3,202,'pending'),(4,303,'pending');")
            .unwrap();
        assert_eq!(
            drain()
                .iter()
                .map(|notice| notice.ticket)
                .collect::<Vec<_>>(),
            [202, 303]
        );
    }
    #[test]
    fn question_notice_contains_only_ticket_identity() {
        let notice = Notice { ticket: 19, kind: NoticeKind::Question, status: String::new() };
        assert_eq!(notice.title(), "需要你的回答");
        assert_eq!(notice.detail(), "工单 #19 · 点击返回工作台查看");
    }

    #[test]
    fn question_watch_baselines_and_observes_background_replacements() {
        let directory = tempfile::tempdir().unwrap();
        let connection = rusqlite::Connection::open(directory.path().join(sa_store::DB_FILE)).unwrap();
        connection.execute_batch("CREATE TABLE questions(id INTEGER PRIMARY KEY, ticket INTEGER, state TEXT); INSERT INTO questions VALUES(1,101,'pending');").unwrap();
        let mut watch = PendingWatch::default();
        let mut drain = || watch.observe(&read_pending(&connection).unwrap());
        assert!(drain().is_empty());
        connection.execute_batch("UPDATE questions SET state='answered' WHERE id=1; INSERT INTO questions VALUES(2,202,'pending'),(3,202,'pending'),(4,303,'withdrawn');").unwrap();
        let notices = drain();
        assert_eq!(notices, vec![Notice { ticket: 202, kind: NoticeKind::Question, status: String::new() }]);
        assert!(drain().is_empty());
        connection.execute_batch("INSERT INTO questions VALUES(5,303,'pending');").unwrap();
        assert_eq!(drain()[0].ticket, 303);
    }

    #[test]
    fn question_watch_tolerates_a_legacy_journal_without_questions() {
        let directory = tempfile::tempdir().unwrap();
        let connection = rusqlite::Connection::open(directory.path().join(sa_store::DB_FILE)).unwrap();
        connection.execute_batch("CREATE TABLE legacy(id INTEGER);").unwrap();
        let mut watch = PendingWatch::default();
        assert!(watch.observe(&read_pending(&connection).unwrap()).is_empty());
        assert_eq!(watch.seen, Some(Default::default()));
        assert_eq!(connection.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name='questions'", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
    }
}
