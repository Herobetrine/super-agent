//! "从 T3 Code 导入": the welcome-page card and link, and the four-step dialog
//! (data location, selection, progress, result).
//!
//! The journal is only ever written through the host;
//! every other file or SQLite access here runs on a background thread. The
//! conversation export writes only below `<data dir>\archives\t3\`.

use crate::{app::App, host::Action, ops::Pending, theme, widgets};
use egui::text::{LayoutJob, TextFormat};
use egui::{Align, Label, Layout, RichText, Sense, Ui};
use rusqlite::{Connection, OpenFlags};
use sa_store::projects::{self, Project, Recent};
use sa_store::t3_import::{self, Report, Scan, ScanError, T3Project};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// eframe storage key of "不再提示".
pub const DISMISS_KEY: &str = "t3_import_dismissed";
const DATABASE: &str = "statev2.sqlite";
const EXIT_WAIT: Duration = Duration::from_secs(30);
/// Entries of the welcome page's "最近的项目".
pub const RECENT_LIMIT: usize = 8;

/// Read-only view of the journal; falls back to an empty in-memory store with
/// the projects table when the journal has not been created yet.
fn journal(data: &Path) -> anyhow::Result<Connection> {
    let empty = || -> anyhow::Result<Connection> {
        let conn = Connection::open_in_memory()?;
        projects::init(&conn)?;
        Ok(conn)
    };
    let file = data.join(sa_store::DB_FILE);
    if !file.is_file() {
        return empty();
    }
    let conn = Connection::open_with_flags(&file, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let has_table: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'studio_projects')",
        [],
        |row| row.get(0),
    )?;
    if has_table { Ok(conn) } else { empty() }
}

struct Quick {
    present: bool,
    imported: bool,
}

type ScanResult = Result<Scan, ScanError>;

fn scan_in_background(ctx: &egui::Context, data: PathBuf, folder: PathBuf) -> Pending<ScanResult> {
    Pending::spawn(ctx, move || {
        let store = journal(&data)?;
        Ok(t3_import::scan(&folder, &store))
    })
}

/// State of the welcome card.
#[derive(Default)]
enum Card {
    #[default]
    Idle,
    Probing(Pending<Quick>),
    Counting(Pending<ScanResult>),
    /// `Some((projects, conversations))`, or `None` when counting failed.
    Shown(Option<(usize, usize)>),
    Hidden,
}

pub struct T3State {
    dismissed: bool,
    card: Card,
    imported: Vec<Project>,
    imported_load: Option<Pending<Vec<Project>>>,
    imported_loaded: bool,
    dialog: Option<Dialog>,
    /// Holds the export thread at its first progress report (tests only).
    #[cfg(test)]
    pub hold: Option<Arc<AtomicBool>>,
    #[cfg(test)]
    pub next_picked_folder: Option<PathBuf>,
}

impl T3State {
    pub fn new(dismissed: bool) -> Self {
        Self {
            dismissed,
            card: Card::Idle,
            imported: Vec::new(),
            imported_load: None,
            imported_loaded: false,
            dialog: None,
            #[cfg(test)]
            hold: None,
            #[cfg(test)]
            next_picked_folder: None,
        }
    }

    pub fn dismissed(&self) -> bool {
        self.dismissed
    }

    /// Drop the cached imported projects and the card verdict after an import.
    fn invalidate(&mut self) {
        self.imported_loaded = false;
        self.card = Card::Idle;
    }

    /// Whether the imported projects of the welcome list were read, for tests.
    #[cfg(test)]
    pub fn imported_ready(&self) -> bool {
        self.imported_loaded && !self.imported.is_empty()
    }

    /// Whether the welcome page stopped changing its layout, for tests.
    #[cfg(test)]
    pub fn settled(&self) -> bool {
        matches!(self.card, Card::Shown(_) | Card::Hidden) && self.imported_loaded
    }

    /// Card state, for tests.
    #[cfg(test)]
    pub fn card_state(&self) -> &'static str {
        match self.card {
            Card::Idle => "idle",
            Card::Probing(_) => "probing",
            Card::Counting(_) => "counting",
            Card::Shown(_) => "shown",
            Card::Hidden => "hidden",
        }
    }

    /// Step of the open dialog, for tests.
    #[cfg(test)]
    pub fn step_name(&self) -> &'static str {
        match self.dialog.as_ref().map(|d| &d.step) {
            None => "closed",
            Some(Step::Locate(Locate::Scanning(_))) => "scanning",
            Some(Step::Locate(_)) => "locate",
            Some(Step::Select(_)) => "select",
            Some(Step::Run(_)) => "run",
            Some(Step::Done(_)) => "done",
        }
    }

    /// Selection flags of the open step 2, for tests.
    #[cfg(test)]
    pub fn picked(&self) -> Vec<bool> {
        match self.dialog.as_ref().map(|d| &d.step) {
            Some(Step::Select(sel)) => sel.picked.clone(),
            _ => Vec::new(),
        }
    }

    /// Conversations exported so far by the running export, for tests.
    #[cfg(test)]
    pub fn export_progress(&self) -> Option<usize> {
        match self.dialog.as_ref().map(|d| &d.step) {
            Some(Step::Run(run)) => Some(run.shared.done.load(Ordering::Relaxed)),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn export_stop_flag(&self) -> Arc<AtomicBool> {
        match &self.dialog.as_ref().expect("import dialog").step {
            Step::Run(run) => run.stop.clone(),
            _ => panic!("expected an active export"),
        }
    }

    /// Archive folder of the finished import, for tests.
    #[cfg(test)]
    pub fn archive_folder(&self) -> Option<PathBuf> {
        match self.dialog.as_ref().map(|d| &d.step) {
            Some(Step::Done(outcome)) => outcome.archive().map(Path::to_path_buf),
            _ => None,
        }
    }
}

struct Dialog {
    default_folder: Option<PathBuf>,
    folder: Option<PathBuf>,
    custom: bool,
    step: Step,
    picking: Option<Pending<Option<PathBuf>>>,
    /// Start the import as soon as the selection opens (screenshot review aid).
    auto: bool,
}

enum Step {
    Locate(Locate),
    Select(Select),
    Run(Run),
    Done(Outcome),
}

enum Locate {
    Scanning(Pending<ScanResult>),
    NotFoundDefault,
    NotFoundChosen,
    Unreadable(String),
    Unsupported(String),
    Empty,
}

struct Select {
    scan: Scan,
    picked: Vec<bool>,
    export: bool,
}

impl Select {
    fn new(scan: Scan) -> Self {
        let picked = scan.projects.iter().map(|p| p.exists && !p.already_listed).collect();
        let export = scan.threads > 0;
        Self { scan, picked, export }
    }

    fn new_projects(&self) -> usize {
        self.scan
            .projects
            .iter()
            .zip(&self.picked)
            .filter(|(p, on)| **on && p.exists && !p.already_listed)
            .count()
    }

    fn will_export(&self) -> bool {
        self.export && self.scan.threads > 0
    }

    fn can_start(&self) -> bool {
        self.new_projects() > 0 || self.will_export()
    }
}

#[derive(Default)]
struct Shared {
    done: AtomicUsize,
    total: AtomicUsize,
}

struct Run {
    export: bool,
    folder: PathBuf,
    threads: usize,
    stage: Stage,
    stop: Arc<AtomicBool>,
    stopping: bool,
    shared: Arc<Shared>,
    added: usize,
}

impl Run {
    fn new(folder: PathBuf, threads: usize, export: bool, added: usize) -> Self {
        Self {
            export,
            folder,
            threads,
            stage: Stage::Projects,
            stop: Arc::default(),
            stopping: false,
            shared: Arc::default(),
            added,
        }
    }
}

enum Stage {
    Projects,
    Export { pending: Pending<Report>, worker: Option<JoinHandle<()>> },
}

impl Drop for Run {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

enum Outcome {
    Success { added: usize, exported: Option<(usize, PathBuf)> },
    Partial { added: usize, exported: usize, total: usize, failed: usize, folder: PathBuf },
    ExportFailed { added: usize, reason: String, folder: PathBuf, threads: usize },
    Failed(String),
    Stopped { added: usize, exported: usize, total: usize, folder: PathBuf },
}

impl Outcome {
    fn title(&self) -> &'static str {
        match self {
            Self::Success { .. } => "导入完成",
            Self::Partial { .. } | Self::ExportFailed { .. } => "导入部分完成",
            Self::Failed(_) => "导入失败",
            Self::Stopped { .. } => "已停止",
        }
    }

    fn text(&self) -> String {
        match self {
            Self::Success { added, exported } => match (*added > 0, exported) {
                (true, Some((m, _))) => format!("已加入 {added} 个项目，已导出 {m} 个对话。"),
                (true, None) => format!("已加入 {added} 个项目。"),
                (false, Some((m, _))) => format!("已导出 {m} 个对话。"),
                (false, None) => "没有新的内容被导入。".into(),
            },
            Self::Partial { added, exported, total, failed, .. } => format!(
                "已加入 {added} 个项目，已导出 {exported}/{total} 个对话。{failed} 个对话导出失败，原因见归档文件夹中的 errors.txt。"
            ),
            Self::ExportFailed { added, reason, .. } =>
                format!("已加入 {added} 个项目。对话导出失败：{reason}。"),
            Self::Failed(reason) => format!("无法完成导入：{reason}。"),
            Self::Stopped { added, exported, total, .. } => format!(
                "已停止导入。已加入 {added} 个项目，已导出 {exported}/{total} 个对话。再次导入会保存到新的文件夹，不会覆盖已有文件。"
            ),
        }
    }

    /// The archive folder, when there is something in it to open.
    fn archive(&self) -> Option<&Path> {
        match self {
            Self::Success { exported: Some((m, folder)), .. } if *m > 0 => Some(folder),
            Self::Partial { folder, .. } => Some(folder),
            Self::Stopped { exported, folder, .. } if *exported > 0 => Some(folder),
            _ => None,
        }
    }
}

fn open_folder(path: PathBuf) {
    std::thread::spawn(move || {
        #[cfg(windows)]
        let program = "explorer";
        #[cfg(target_os = "macos")]
        let program = "open";
        #[cfg(all(not(windows), not(target_os = "macos")))]
        let program = "xdg-open";
        let _ = std::process::Command::new(program).arg(path).spawn();
    });
}

enum Intent {
    Close,
    Pick,
    Rescan,
    Start,
    RetryExport,
    Stop,
    OpenArchive(PathBuf),
}

impl App {
    fn t3_available(&self) -> bool {
        self.writable() && !self.opts.demo
    }

    /// Where T3 Code keeps its data; tests never look at the real one.
    fn t3_default_folder(&self) -> Option<PathBuf> {
        if let Some(folder) = &self.opts.t3_folder {
            return Some(folder.clone());
        }
        if cfg!(test) { None } else { t3_import::default_folder() }
    }

    fn t3_data_dir(&self) -> PathBuf {
        self.opts.data.clone().unwrap_or_else(sa_store::default_data_dir)
    }

    /// Entries of "最近的项目": ticket projects first, then imported ones.
    pub(crate) fn t3_recent(&mut self) -> Vec<Recent> {
        if self.t3_available() {
            if let Some(result) = self.t3.imported_load.as_ref().and_then(Pending::poll) {
                self.t3.imported_load = None;
                self.t3.imported_loaded = true;
                if let Ok(list) = result {
                    self.t3.imported = list;
                }
            }
            if !self.t3.imported_loaded && self.t3.imported_load.is_none() {
                let data = self.t3_data_dir();
                self.t3.imported_load = Some(Pending::spawn(&self.ctx, move || projects::list(&journal(&data)?)));
            }
        }
        let cwds: Vec<String> = self.history.iter().map(|t| t.cwd.clone()).collect();
        projects::recent(&cwds, &self.t3.imported, RECENT_LIMIT)
    }

    fn t3_poll_card(&mut self) {
        if !self.t3_available() || self.t3.dismissed {
            return;
        }
        let ctx = self.ctx.clone();
        let data = self.t3_data_dir();
        let folder = self.t3_default_folder();
        self.t3.card = match std::mem::take(&mut self.t3.card) {
            Card::Idle => match folder {
                Some(folder) => Card::Probing(Pending::spawn(&ctx, move || {
                    let present = folder.join(DATABASE).is_file();
                    let imported = present && t3_import::has_imported(&journal(&data)?)?;
                    Ok(Quick { present, imported })
                })),
                None => Card::Hidden,
            },
            Card::Probing(pending) => match pending.poll() {
                None => Card::Probing(pending),
                Some(Ok(Quick { present: true, imported: false })) => match folder {
                    Some(folder) => Card::Counting(scan_in_background(&ctx, data, folder)),
                    None => Card::Hidden,
                },
                Some(_) => Card::Hidden,
            },
            Card::Counting(pending) => match pending.poll() {
                None => Card::Counting(pending),
                Some(Ok(Ok(scan))) if scan.projects.is_empty() && scan.threads == 0 => Card::Hidden,
                Some(Ok(Ok(scan))) => Card::Shown(Some((scan.projects.len(), scan.threads))),
                Some(_) => Card::Shown(None),
            },
            other => other,
        };
        if matches!(self.t3.card, Card::Probing(_) | Card::Counting(_)) {
            self.ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// Whether the welcome card takes room on the page.
    pub(crate) fn t3_card_showing(&self) -> bool {
        self.t3_available() && !self.t3.dismissed && matches!(self.t3.card, Card::Shown(_) | Card::Counting(_))
    }

    /// The welcome page's card (below "最近的项目") and the persistent link.
    pub(crate) fn t3_welcome(&mut self, ui: &mut Ui, indent: f32) {
        if !self.t3_available() {
            return;
        }
        self.t3_poll_card();
        let pal = self.pal.clone();
        let body = match &self.t3.card {
            Card::Counting(_) => Some("找到 T3 Code 的数据，正在统计…".to_owned()),
            Card::Shown(None) => Some("找到 T3 Code 的数据。".to_owned()),
            Card::Shown(Some((n, m))) => Some(format!(
                "找到 T3 Code 的数据：{n} 个项目、{m} 个对话。可以把项目加到这里，并把对话导出为本地归档（仅供查阅）。"
            )),
            _ => None,
        };
        let (mut start, mut dismiss) = (false, false);
        if let (false, Some(body)) = (self.t3.dismissed, body) {
            ui.add_space(22.0);
            ui.horizontal_top(|ui| {
                ui.add_space(indent);
                ui.vertical(|ui| {
                    widgets::card(&pal).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 6.0;
                        ui.label(RichText::new("从 T3 Code 导入").font(theme::strong(theme::UI)).color(pal.ink));
                        ui.add(Label::new(RichText::new(body).font(theme::body(theme::SMALL)).color(pal.mute)).wrap());
                        ui.add_space(2.0);
                        ui.horizontal(|ui| {
                            start = widgets::primary_button(ui, "", "开始导入…", &pal, true).clicked();
                            dismiss = widgets::quiet_button(ui, "", "不再提示", &pal, false).clicked();
                        });
                    });
                });
            });
        }
        ui.add_space(14.0);
        let link = ui
            .horizontal(|ui| {
                ui.add_space(indent);
                let resp = ui.add(
                    Label::new(RichText::new("从 T3 Code 导入…").font(theme::body(theme::SMALL)).color(pal.faint))
                        .sense(Sense::click()),
                );
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "从 T3 Code 导入…"));
                resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
            })
            .inner;
        if dismiss {
            self.t3.dismissed = true;
        }
        if start || link {
            self.open_t3_import();
        }
    }

    pub fn open_t3_import(&mut self) {
        if self.t3.dialog.is_some() || !self.t3_available() {
            return;
        }
        let default = self.t3_default_folder();
        let mut dialog = Dialog {
            folder: default.clone(),
            default_folder: default,
            custom: false,
            step: Step::Locate(Locate::NotFoundDefault),
            picking: None,
            auto: false,
        };
        self.t3_rescan(&mut dialog);
        self.t3.dialog = Some(dialog);
    }

    /// Open the dialog and start importing at once (screenshot review aid).
    pub fn open_t3_import_auto(&mut self) {
        self.open_t3_import();
        if let Some(dialog) = self.t3.dialog.as_mut() {
            dialog.auto = true;
        }
    }

    fn t3_rescan(&self, dialog: &mut Dialog) {
        dialog.step = Step::Locate(match &dialog.folder {
            Some(folder) => Locate::Scanning(scan_in_background(&self.ctx, self.t3_data_dir(), folder.clone())),
            None => Locate::NotFoundDefault,
        });
    }

    /// Use `folder` as the T3 Code data location (the folder picker's result).
    #[cfg(test)]
    pub(crate) fn t3_choose_folder(&mut self, folder: PathBuf) {
        let Some(mut dialog) = self.t3.dialog.take() else { return };
        dialog.custom = dialog.default_folder.as_ref() != Some(&folder);
        dialog.folder = Some(folder);
        self.t3_rescan(&mut dialog);
        self.t3.dialog = Some(dialog);
    }

    fn t3_pick_folder(&mut self, start: Option<PathBuf>) -> Pending<Option<PathBuf>> {
        #[cfg(test)]
        if let Some(folder) = self.t3.next_picked_folder.take() {
            return Pending::spawn(&self.ctx, move || Ok(Some(folder)));
        }
        Pending::spawn(&self.ctx, move || {
            let mut picker = rfd::FileDialog::new().set_title("选择 T3 Code 数据文件夹");
            if let Some(start) = start.filter(|p| p.is_dir()) {
                picker = picker.set_directory(start);
            }
            Ok(picker.pick_folder())
        })
    }

    fn t3_spawn_export(&self, run: &Run) -> Stage {
        let folder = run.folder.clone();
        let archives = self.t3_data_dir().join("archives").join("t3");
        let stop = run.stop.clone();
        let shared = run.shared.clone();
        let repaint = self.ctx.clone();
        #[cfg(test)]
        let hold = self.t3.hold.clone();
        #[cfg(not(test))]
        let hold: Option<Arc<AtomicBool>> = None;
        let (pending, worker) = Pending::spawn_joinable(&self.ctx, move || {
            t3_import::export_threads(&folder, &archives, &stop, &|progress| {
                shared.total.store(progress.total, Ordering::Relaxed);
                shared.done.store(progress.done, Ordering::Relaxed);
                repaint.request_repaint();
                if let Some(hold) = &hold {
                    while hold.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            })
        });
        Stage::Export { pending, worker: Some(worker) }
    }

    pub(crate) fn t3_finish_on_exit(&mut self) {
        let Some(mut dialog) = self.t3.dialog.take() else { return };
        if let Step::Run(run) = &mut dialog.step
            && let Stage::Export { worker, .. } = &mut run.stage
            && let Some(thread) = worker.take()
        {
            run.stop.store(true, Ordering::Relaxed);
            let deadline = Instant::now() + EXIT_WAIT;
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if thread.is_finished() {
                let _ = thread.join();
                // Process the final report while the host can still record it.
                self.t3_poll_dialog(&mut dialog);
            } else {
                eprintln!("等待 T3 对话归档完成超时");
            }
        }
        self.t3.dialog = Some(dialog);
    }

    fn t3_start(&mut self, dialog: &mut Dialog) {
        let Step::Select(sel) = &dialog.step else { return };
        if dialog.picking.is_some() || !sel.can_start() {
            return;
        }
        let chosen: Vec<T3Project> = sel
            .scan
            .projects
            .iter()
            .zip(&sel.picked)
            .filter(|(p, on)| **on && p.exists && !p.already_listed)
            .map(|(p, _)| p.clone())
            .collect();
        let mut run = Run::new(sel.scan.folder.clone(), sel.scan.threads, sel.will_export(), 0);
        if chosen.is_empty() {
            run.stage = self.t3_spawn_export(&run);
            dialog.step = Step::Run(run);
            return;
        }
        let pending = self.composer.pending;
        self.submit(Action::ImportT3Projects(chosen));
        dialog.step = if self.composer.pending == pending {
            Step::Done(Outcome::Failed("调度线程已退出，请重新打开窗口".into()))
        } else {
            Step::Run(run)
        };
    }

    fn t3_retry_export(&self, dialog: &mut Dialog) {
        let Step::Done(Outcome::ExportFailed { added, folder, threads, .. }) = &dialog.step else { return };
        let mut run = Run::new(folder.clone(), *threads, true, *added);
        run.stage = self.t3_spawn_export(&run);
        dialog.step = Step::Run(run);
    }

    /// The host answered `Action::ImportT3Projects`.
    pub(crate) fn t3_projects_imported(&mut self, result: Result<Option<i64>, String>) {
        let Some(mut dialog) = self.t3.dialog.take() else { return };
        if let Step::Run(mut run) = std::mem::replace(&mut dialog.step, Step::Locate(Locate::Empty)) {
            dialog.step = match result {
                Ok(added) => {
                    run.added = added.and_then(|n| usize::try_from(n).ok()).unwrap_or(0);
                    self.t3.invalidate();
                    if run.export {
                        run.stage = self.t3_spawn_export(&run);
                        Step::Run(run)
                    } else {
                        Step::Done(Outcome::Success { added: run.added, exported: None })
                    }
                }
                Err(error) => Step::Done(Outcome::Failed(error)),
            };
        }
        self.t3.dialog = Some(dialog);
    }

    fn t3_poll_dialog(&mut self, dialog: &mut Dialog) {
        if let Some(result) = dialog.picking.as_ref().and_then(Pending::poll) {
            dialog.picking = None;
            if let Ok(Some(folder)) = result {
                dialog.custom = dialog.default_folder.as_ref() != Some(&folder);
                dialog.folder = Some(folder);
                self.t3_rescan(dialog);
            }
        }
        let custom = dialog.custom;
        match &mut dialog.step {
            Step::Locate(Locate::Scanning(pending)) => {
                let Some(result) = pending.poll() else { return };
                dialog.step = match result {
                    Ok(Ok(scan)) if scan.projects.is_empty() && scan.threads == 0 => Step::Locate(Locate::Empty),
                    Ok(Ok(scan)) => Step::Select(Select::new(scan)),
                    Ok(Err(ScanError::NotFound)) => Step::Locate(if custom {
                        Locate::NotFoundChosen
                    } else {
                        Locate::NotFoundDefault
                    }),
                    Ok(Err(ScanError::Unreadable(reason))) | Err(reason) => Step::Locate(Locate::Unreadable(reason)),
                    Ok(Err(ScanError::Unsupported { missing })) => Step::Locate(Locate::Unsupported(missing)),
                };
            }
            Step::Run(run) => {
                let Stage::Export { pending, .. } = &run.stage else { return };
                let Some(result) = pending.poll() else { return };
                let added = run.added;
                self.t3.invalidate();
                if result.as_ref().is_ok_and(|report| report.exported > 0) {
                    self.t3.card = Card::Hidden;
                    self.submit(Action::RecordT3Export);
                }
                dialog.step = Step::Done(match result {
                    Ok(report) if report.stopped => Outcome::Stopped {
                        added,
                        exported: report.exported,
                        total: report.total,
                        folder: report.folder,
                    },
                    Ok(report) if added > 0 && report.exported == 0 && !report.failed.is_empty() => {
                        Outcome::ExportFailed {
                            added,
                            reason: format!("{}（详见 {}）", report.failed[0].1, report.folder.join("errors.txt").display()),
                            folder: run.folder.clone(),
                            threads: run.threads,
                        }
                    }
                    Ok(report) if !report.failed.is_empty() => Outcome::Partial {
                        added,
                        exported: report.exported,
                        total: report.total,
                        failed: report.failed.len(),
                        folder: report.folder,
                    },
                    Ok(report) => Outcome::Success { added, exported: Some((report.exported, report.folder)) },
                    Err(reason) if added > 0 => Outcome::ExportFailed {
                        added,
                        reason,
                        folder: run.folder.clone(),
                        threads: run.threads,
                    },
                    Err(error) => Outcome::Failed(error),
                });
            }
            _ => {}
        }
    }

    pub fn t3_import_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.t3.dialog.take() else { return };
        self.t3_poll_dialog(&mut dialog);
        if dialog.auto && matches!(dialog.step, Step::Select(_)) {
            dialog.auto = false;
            self.t3_start(&mut dialog);
        }
        let pal = self.pal.clone();
        let data = self.t3_data_dir();
        let running = matches!(dialog.step, Step::Run(_));
        let mut intent = None;
        let modal = egui::Modal::new(egui::Id::new("sa-t3-import"))
            .frame(widgets::modal_frame(&pal))
            .show(ctx, |ui| {
                ui.set_width(560.0);
                let title = match &dialog.step {
                    Step::Done(outcome) => outcome.title(),
                    _ => "从 T3 Code 导入",
                };
                let close = widgets::modal_header(ui, "", title, "只读取 T3 Code 的数据，不会修改或删除它。", &pal);
                // X and Esc are inert while exporting; "停止" is the way out.
                if close && !running {
                    intent = Some(Intent::Close);
                }
                let max_height = (ctx.content_rect().height() - 300.0).max(180.0);
                match &mut dialog.step {
                    Step::Locate(locate) => {
                        locate_ui(ui, locate, &dialog.folder, &dialog.default_folder, dialog.picking.is_some(), &pal, &mut intent);
                    }
                    Step::Select(sel) => {
                        let can_pick = dialog.picking.is_none();
                        folder_row(ui, dialog.folder.as_deref(), can_pick, &pal, &mut intent);
                        select_ui(ui, sel, &data, max_height, can_pick, &pal, &mut intent);
                    }
                    Step::Run(run) => run_ui(ui, run, &pal, &mut intent),
                    Step::Done(outcome) => done_ui(ui, outcome, &pal, &mut intent),
                }
            });
        if !running && modal.should_close() {
            intent = Some(Intent::Close);
        }
        match intent {
            Some(Intent::Close) => return,
            Some(Intent::Pick) if dialog.picking.is_none() => {
                dialog.picking = Some(self.t3_pick_folder(dialog.folder.clone()));
            }
            Some(Intent::Rescan) => self.t3_rescan(&mut dialog),
            Some(Intent::Start) => self.t3_start(&mut dialog),
            Some(Intent::RetryExport) => self.t3_retry_export(&mut dialog),
            Some(Intent::Stop) => {
                if let Step::Run(run) = &mut dialog.step {
                    run.stop.store(true, Ordering::Relaxed);
                    run.stopping = true;
                }
            }
            Some(Intent::OpenArchive(path)) => open_folder(path),
            _ => {}
        }
        self.t3.dialog = Some(dialog);
    }
}

fn mute(ui: &mut Ui, text: &str, pal: &theme::Palette) {
    ui.add(Label::new(RichText::new(text).font(theme::body(theme::SMALL)).color(pal.mute)).wrap());
}

fn colored(ui: &mut Ui, text: &str, color: egui::Color32) {
    ui.add(Label::new(RichText::new(text).font(theme::body(theme::UI)).color(color)).wrap());
}

fn folder_row(ui: &mut Ui, folder: Option<&Path>, can_pick: bool, pal: &theme::Palette, intent: &mut Option<Intent>) {
    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            ui.set_max_width((ui.available_width() - 130.0).max(120.0));
            let mut job = LayoutJob::default();
            let format = |color| TextFormat { font_id: theme::body(theme::UI), color, ..Default::default() };
            job.append("T3 Code 数据文件夹：", 0.0, format(pal.mute));
            let shown = folder.map_or_else(|| "未找到用户目录".to_owned(), |p| p.display().to_string());
            job.append(&shown, 0.0, format(pal.ink));
            ui.add(Label::new(job).wrap());
        });
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            ui.add_enabled_ui(can_pick, |ui| {
                if widgets::outline_button(ui, "", "选择文件夹…", pal)
                    .on_hover_text("请选择包含 statev2.sqlite 的文件夹")
                    .clicked()
                {
                    *intent = Some(Intent::Pick);
                }
            });
        });
    });
    ui.add_space(12.0);
}

fn footer(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.add_space(16.0);
    ui.with_layout(Layout::right_to_left(Align::Center), add);
}

fn locate_ui(
    ui: &mut Ui,
    locate: &Locate,
    folder: &Option<PathBuf>,
    default_folder: &Option<PathBuf>,
    picking: bool,
    pal: &theme::Palette,
    intent: &mut Option<Intent>,
) {
    folder_row(ui, folder.as_deref(), !picking, pal, intent);
    match locate {
        Locate::Scanning(_) => {
            ui.horizontal(|ui| {
                widgets::spinner(ui);
                ui.label("正在读取 T3 Code 数据…");
            });
        }
        Locate::NotFoundDefault => {
            let default = default_folder.as_ref().map_or_else(String::new, |p| p.display().to_string());
            mute(
                ui,
                &format!("没有在默认位置找到 T3 Code 数据（{default}）。如果你改过数据目录，请点击“选择文件夹…”。"),
                pal,
            );
        }
        Locate::NotFoundChosen => {
            colored(ui, "所选文件夹中没有找到 T3 Code 数据（缺少 statev2.sqlite），请重新选择。", pal.fail.fg);
        }
        Locate::Unreadable(reason) => {
            colored(ui, &format!("无法读取 T3 Code 数据：{reason}。如果 T3 Code 正在运行，请先退出后重试。"), pal.fail.fg);
            ui.add_space(8.0);
            if widgets::outline_button(ui, "", "重试", pal).clicked() {
                *intent = Some(Intent::Rescan);
            }
        }
        Locate::Unsupported(missing) => {
            colored(ui, &format!("无法识别这个版本的 T3 Code 数据（缺少 {missing}），没有做任何改动。"), pal.fail.fg);
        }
        Locate::Empty => mute(ui, "没有找到可导入的项目或对话。", pal),
    }
    footer(ui, |ui| {
        if widgets::quiet_button(ui, "", "取消", pal, false).clicked() {
            *intent = Some(Intent::Close);
        }
    });
}

fn project_name(p: &T3Project) -> String {
    let title = p.title.trim();
    if title.is_empty() {
        crate::labels::basename(&p.path)
    } else {
        title.to_owned()
    }
}

fn select_ui(ui: &mut Ui, sel: &mut Select, data: &Path, max_height: f32, can_start_now: bool, pal: &theme::Palette, intent: &mut Option<Intent>) {
    egui::ScrollArea::vertical().max_height(max_height).auto_shrink([false, true]).show(ui, |ui| {
        ui.horizontal(|ui| {
            widgets::section(ui, &format!("项目（{}）", sel.scan.projects.len()), pal);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::quiet_button(ui, "", "全不选", pal, false).clicked() {
                    sel.picked.iter_mut().for_each(|on| *on = false);
                }
                if widgets::quiet_button(ui, "", "全选", pal, false).clicked() {
                    for (on, p) in sel.picked.iter_mut().zip(&sel.scan.projects) {
                        *on = p.exists && !p.already_listed;
                    }
                }
            });
        });
        ui.add_space(4.0);
        for (project, picked) in sel.scan.projects.iter().zip(sel.picked.iter_mut()) {
            // Rows that cannot be imported show their fixed state instead.
            let (mut on, enabled, tag) = if project.already_listed {
                (true, false, Some("已在列表中"))
            } else if !project.exists {
                (false, false, Some("文件夹已不存在"))
            } else {
                (*picked, true, None)
            };
            ui.horizontal(|ui| {
                ui.add_enabled_ui(enabled, |ui| {
                    if ui.add(widgets::Check::new(&mut on, project_name(project))).changed() {
                        *picked = on;
                    }
                });
                if let Some(tag) = tag {
                    ui.label(RichText::new(tag).font(theme::body(theme::TINY)).color(pal.warn.fg));
                }
                ui.add(
                    Label::new(RichText::new(&project.path).font(theme::body(theme::SMALL)).color(pal.faint)).truncate(),
                )
                .on_hover_text(&project.path);
            });
        }
        ui.add_space(6.0);
        mute(ui, "导入后会出现在欢迎页的“最近的项目”里（最多显示 8 个，有工单的项目排在前面）。", pal);
        ui.add_space(14.0);
        widgets::rule(ui, pal.line_soft);
        ui.add_space(14.0);
        widgets::section(ui, &format!("对话（{}）", sel.scan.threads), pal);
        ui.add_space(6.0);
        ui.add_enabled_ui(sel.scan.threads > 0, |ui| {
            ui.add(widgets::Check::new(&mut sel.export, "把对话导出为本地归档（仅供查阅）"));
        });
        ui.add_space(4.0);
        for line in [
            "每个对话导出一个 Markdown 文件（方便阅读）和一个 JSONL 文件（方便检索和程序处理）。",
            "内容包括标题、所属项目、模型、时间、全部消息和工具调用摘要。",
            "命令输出和文件改动只保留摘要，附件只记录文件名。",
            "另有目录文件 index.md，按项目分组，按更新时间排列。",
        ] {
            mute(ui, line, pal);
        }
        ui.add_space(4.0);
        mute(ui, &format!("保存位置：{}\\archives\\t3\\{{日期时间}}\\", data.display()), pal);
        mute(ui, "导出的对话仅供查阅，不能在 Super Agent 里继续。", pal);
    });
    let has_selection = sel.can_start();
    let can_start = can_start_now && has_selection;
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        if !has_selection {
            mute(ui, "没有可导入的新内容", pal);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if widgets::primary_button(ui, "", "开始导入", pal, can_start).clicked() && can_start {
                *intent = Some(Intent::Start);
            }
            if widgets::quiet_button(ui, "", "取消", pal, false).clicked() {
                *intent = Some(Intent::Close);
            }
        });
    });
}

fn run_ui(ui: &mut Ui, run: &mut Run, pal: &theme::Palette, intent: &mut Option<Intent>) {
    let done = run.shared.done.load(Ordering::Relaxed);
    let total = match run.shared.total.load(Ordering::Relaxed) {
        0 => run.threads,
        n => n,
    };
    match &run.stage {
        Stage::Projects => {
            ui.horizontal(|ui| {
                widgets::spinner(ui);
                ui.label("正在导入项目…");
            });
        }
        Stage::Export { .. } => {
            let text = if run.stopping {
                format!("正在完成当前对话后停止（{done}/{total}）…")
            } else {
                format!("正在导出对话 {done}/{total}…")
            };
            ui.horizontal(|ui| {
                widgets::spinner(ui);
                ui.label(text);
            });
            ui.add_space(8.0);
            let fraction = if total == 0 { 0.0 } else { (done as f32 / total as f32).clamp(0.0, 1.0) };
            ui.add(egui::ProgressBar::new(fraction).desired_width(ui.available_width()).desired_height(8.0));
        }
    }
    let can_stop = matches!(run.stage, Stage::Export { .. }) && !run.stopping;
    footer(ui, |ui| {
        ui.add_enabled_ui(can_stop, |ui| {
            let label = if run.stopping { "正在停止…" } else { "停止" };
            if widgets::outline_button(ui, "", label, pal).clicked() {
                *intent = Some(Intent::Stop);
            }
        });
    });
}

fn done_ui(ui: &mut Ui, outcome: &Outcome, pal: &theme::Palette, intent: &mut Option<Intent>) {
    match outcome {
        Outcome::Failed(_) => colored(ui, &outcome.text(), pal.fail.fg),
        _ => colored(ui, &outcome.text(), pal.ink),
    }
    footer(ui, |ui| {
        if matches!(outcome, Outcome::Failed(_)) {
            if widgets::primary_button(ui, "", "关闭", pal, true).clicked() {
                *intent = Some(Intent::Close);
            }
            if widgets::outline_button(ui, "", "重试", pal).clicked() {
                *intent = Some(Intent::Rescan);
            }
            return;
        }
        if widgets::primary_button(ui, "", "完成", pal, true).clicked() {
            *intent = Some(Intent::Close);
        }
        if matches!(outcome, Outcome::ExportFailed { .. })
            && widgets::outline_button(ui, "", "重试导出", pal).clicked()
        {
            *intent = Some(Intent::RetryExport);
        }
        if let Some(folder) = outcome.archive()
            && widgets::outline_button(ui, "", "打开归档文件夹", pal).clicked()
        {
            *intent = Some(Intent::OpenArchive(folder.to_path_buf()));
        }
    });
}
