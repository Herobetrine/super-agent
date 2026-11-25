//! Recovery panel for a ticket database that did not start: an interrupted
//! migration (design C2) and a corrupt journal (design C3).
//!
//! `Host::start` keeps the typed [`JournalError`] in its `anyhow::Error`; the
//! desktop copies what the panel needs into a [`RecoveryPanel`] and shows it
//! as a modal over the read-only window. Every filesystem or process action
//! runs on a background thread.

use crate::{app::App, host::Host, ops::Pending, theme, theme::Palette, widgets, worker::Cmd};
use anyhow::{Context, Result};
use egui::{RichText, TextFormat, Ui, ViewportCommand, text::LayoutJob};
use sa_store::journal::{JournalError, RestoreIdentity, RestoreOutcome};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::SystemTime,
};

/// Spawns the current executable again with the same arguments.
type Restart = Arc<dyn Fn() -> Result<()> + Send + Sync>;
/// Opens a folder in the system file manager.
type OpenFolder = Arc<dyn Fn(&Path) -> Result<()> + Send + Sync>;
type Restore = Arc<dyn Fn(&Path, &Path, &RestoreIdentity) -> Result<RestoreOutcome> + Send + Sync>;
type Cleanup = Arc<dyn Fn(&Path) -> Result<RestoreOutcome> + Send + Sync>;

#[derive(Clone)]
pub(crate) struct Hooks {
    pub restart: Restart,
    pub open_folder: OpenFolder,
    pub restore: Restore,
    pub cleanup: Cleanup,
}

impl Default for Hooks {
    fn default() -> Self {
        Self {
            restart: Arc::new(restart_self),
            open_folder: Arc::new(open_folder),
            restore: Arc::new(Host::restore_snapshot),
            cleanup: Arc::new(Host::retry_restore_cleanup),
        }
    }
}

fn restart_self() -> Result<()> {
    let exe = std::env::current_exe().context("无法确定程序路径")?;
    let mut command = std::process::Command::new(exe);
    command.args(std::env::args_os().skip(1));
    sa_engine::owner::spawn_detached(command).context("无法启动新的 Super Agent")?;
    Ok(())
}

fn open_folder(dir: &Path) -> Result<()> {
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    // The file manager's own exit status is unreliable (Explorer reports 1),
    // so only a failure to start it is an error.
    std::process::Command::new(program)
        .arg(dir)
        .spawn()
        .with_context(|| format!("无法启动 {program}"))?;
    Ok(())
}

pub(crate) enum Kind {
    /// The marker of an unfinished migration was found (C2).
    Interrupted,
    /// SQLite refused to read the file (C3).
    Corrupt {
        path: PathBuf,
        code: i32,
    },
    RestoreInterrupted {
        record: PathBuf,
        installed: bool,
        reason: String,
    },
    InvalidMigrationMarker {
        path: PathBuf,
        reason: String,
    },
}

pub(crate) struct Snapshot {
    pub path: PathBuf,
    /// Modification time of the snapshot file.
    pub saved: Option<SystemTime>,
}

pub(crate) enum Phase {
    Ready,
    Restoring(Pending<Result<RestoreOutcome>>),
    Cleaning(Pending<Result<RestoreOutcome>>),
    CleanupRequired(String),
    Restored,
    Restarting(Pending<()>),
    /// The new process is running; this window is closing.
    Closing,
}

pub(crate) struct RecoveryPanel {
    pub kind: Kind,
    /// Present and readable; a missing snapshot of a C2 panel is `missing`.
    pub snapshot: Option<Snapshot>,
    /// C2 only: the snapshot the marker names cannot be read.
    pub missing: Option<PathBuf>,
    pub data_dir: PathBuf,
    /// Restore is the job of the main window.
    pub observer: bool,
    pub phase: Phase,
    /// "暂不恢复" or the close button: hidden for this session.
    pub dismissed: bool,
    /// Red text under the buttons.
    pub error: Option<String>,
    opening: Option<Pending<()>>,
    pub hooks: Hooks,
    identity: Option<RestoreIdentity>,
}

fn readable(path: &Path) -> Option<SystemTime> {
    let file = std::fs::File::open(path).ok()?;
    let meta = file.metadata().ok().filter(|m| m.is_file())?;
    meta.modified().ok().or(Some(SystemTime::UNIX_EPOCH))
}

fn stamp(time: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(time)
        .format("%Y/%-m/%-d %H:%M:%S")
        .to_string()
}

impl RecoveryPanel {
    pub(crate) fn from_journal(
        journal: &JournalError,
        data_dir: &Path,
        observer: bool,
    ) -> Option<Self> {
        let (kind, snapshot, missing) = match journal {
            JournalError::MigrationInterrupted { snapshot, .. } => match readable(snapshot) {
                Some(saved) => (
                    Kind::Interrupted,
                    Some(Snapshot {
                        path: snapshot.clone(),
                        saved: Some(saved),
                    }),
                    None,
                ),
                None => (Kind::Interrupted, None, Some(snapshot.clone())),
            },
            JournalError::Corrupt {
                path,
                code,
                latest_snapshot,
                ..
            } => (
                Kind::Corrupt {
                    path: path.clone(),
                    code: *code,
                },
                // A snapshot that cannot be read is no recovery source.
                latest_snapshot
                    .as_ref()
                    .and_then(|(path, _)| readable(path).map(|saved| (path, saved)))
                    .map(|(path, saved)| Snapshot {
                        path: path.clone(),
                        saved: Some(saved),
                    }),
                None,
            ),
            JournalError::RestoreInterrupted {
                record,
                installed,
                reason,
                ..
            } => (
                Kind::RestoreInterrupted {
                    record: record.clone(),
                    installed: *installed,
                    reason: reason.clone(),
                },
                None,
                None,
            ),
            JournalError::InvalidMigrationMarker { path, reason } => (
                Kind::InvalidMigrationMarker {
                    path: path.clone(),
                    reason: reason.clone(),
                },
                None,
                None,
            ),
            JournalError::InvalidSnapshot { .. } | JournalError::RestoreCancelled => return None,
        };
        Some(Self {
            kind,
            snapshot,
            missing,
            data_dir: data_dir.to_path_buf(),
            observer,
            phase: Phase::Ready,
            dismissed: false,
            error: None,
            opening: None,
            hooks: Hooks::default(),
            identity: journal.restore_identity().cloned(),
        })
    }

    pub fn title(&self) -> &'static str {
        if matches!(self.phase, Phase::CleanupRequired(_) | Phase::Cleaning(_)) {
            return "上次恢复没有完成清理";
        }
        match self.kind {
            Kind::Interrupted => "上次工单数据库升级没有完成",
            Kind::Corrupt { .. } => "工单数据库已损坏",
            Kind::RestoreInterrupted {
                installed: true, ..
            } => "上次恢复没有完成清理",
            Kind::RestoreInterrupted {
                installed: false, ..
            } => "上次恢复没有完成",
            Kind::InvalidMigrationMarker { .. } => "升级标记无法识别",
        }
    }

    fn busy(&self) -> bool {
        matches!(
            self.phase,
            Phase::Restoring(_) | Phase::Cleaning(_) | Phase::Restarting(_) | Phase::Closing
        )
    }

    fn can_cleanup(&self) -> bool {
        matches!(self.phase, Phase::CleanupRequired(_) | Phase::Cleaning(_))
            || matches!(
                self.kind,
                Kind::RestoreInterrupted {
                    installed: true,
                    ..
                }
            )
    }

    fn poll(&mut self, ctx: &egui::Context, reader: &mpsc::Sender<Cmd>) {
        if let Some(result) = self.opening.as_ref().and_then(Pending::poll) {
            self.opening = None;
            if let Err(error) = result {
                self.error = Some(format!("无法打开数据文件夹：{error}"));
            }
        }
        match &self.phase {
            Phase::Restoring(pending) | Phase::Cleaning(pending) => {
                if let Some(result) = pending.poll() {
                    let cleaning = matches!(self.phase, Phase::Cleaning(_));
                    match result {
                        Ok(outcome) => self.finish_recovery(outcome, cleaning, reader),
                        Err(error) if cleaning => self.phase = Phase::CleanupRequired(error),
                        Err(error) => {
                            self.phase = Phase::Ready;
                            self.identity = None;
                            self.error = Some(format!(
                                "恢复失败：{error}。请重新启动 Super Agent 后再看。"
                            ));
                        }
                    }
                }
            }
            Phase::Restarting(pending) => {
                if let Some(result) = pending.poll() {
                    match result {
                        Ok(()) => {
                            self.phase = Phase::Closing;
                            ctx.send_viewport_cmd(ViewportCommand::Close);
                        }
                        Err(error) => {
                            self.phase = Phase::Restored;
                            self.error = Some(format!(
                                "无法重新启动：{error}。请手动重新启动 Super Agent。"
                            ));
                        }
                    }
                }
            }
            Phase::Ready | Phase::CleanupRequired(_) | Phase::Restored | Phase::Closing => {}
        }
    }

    fn finish_recovery(
        &mut self,
        result: Result<RestoreOutcome>,
        cleaning: bool,
        reader: &mpsc::Sender<Cmd>,
    ) {
        self.error = None;
        match result {
            Ok(RestoreOutcome::Installed) => self.phase = Phase::Restored,
            Ok(RestoreOutcome::InstalledMarkerNotRemoved(error)) => {
                self.phase = Phase::CleanupRequired(format!("{error:#}"));
            }
            Err(error) => {
                let journal = error.downcast_ref::<JournalError>();
                if let Some(JournalError::RestoreInterrupted {
                    record,
                    installed,
                    reason,
                    ..
                }) = journal
                {
                    self.kind = Kind::RestoreInterrupted {
                        record: record.clone(),
                        installed: *installed,
                        reason: reason.clone(),
                    };
                    self.snapshot = None;
                    self.identity = None;
                    self.phase = Phase::Ready;
                    return;
                }
                if cleaning {
                    self.phase = Phase::CleanupRequired(format!("{error:#}"));
                    return;
                }
                self.phase = Phase::Ready;
                let _ = reader.send(Cmd::Resume);
                self.error = Some(match journal {
                    Some(JournalError::InvalidSnapshot { reason, .. }) => {
                        format!("快照不完整，没有恢复：{reason}。当前文件没有改动。")
                    }
                    Some(JournalError::RestoreCancelled) => {
                        self.identity = None;
                        "工单数据库的状态已经变化，已取消这次恢复。请重新启动 Super Agent 后再看。".into()
                    }
                    _ if in_use(&error) => "恢复失败：工单数据库正被其他程序占用。请关闭其他 Super Agent 窗口后重试。当前文件没有改动。".into(),
                    _ => format!("恢复失败：{error:#}。当前文件没有改动。"),
                });
            }
        }
    }

    fn restore(&mut self, ctx: &egui::Context, reader: mpsc::Sender<Cmd>) {
        let (Some(snapshot), Some(identity)) = (&self.snapshot, &self.identity) else {
            return;
        };
        if self.observer || self.busy() {
            return;
        }
        let dir = self.data_dir.clone();
        let snapshot = snapshot.path.clone();
        let identity = identity.clone();
        let restore = self.hooks.restore.clone();
        self.error = None;
        self.phase = Phase::Restoring(after_reader_closed(ctx, reader, move || {
            restore(&dir, &snapshot, &identity)
        }));
    }

    fn cleanup(&mut self, ctx: &egui::Context, reader: mpsc::Sender<Cmd>) {
        if self.observer || self.busy() || !self.can_cleanup() {
            return;
        }
        let dir = self.data_dir.clone();
        let cleanup = self.hooks.cleanup.clone();
        self.error = None;
        self.phase = Phase::Cleaning(after_reader_closed(ctx, reader, move || cleanup(&dir)));
    }

    fn open_data_folder(&mut self, ctx: &egui::Context) {
        if self.opening.is_some() {
            return;
        }
        let dir = self.data_dir.clone();
        let open = self.hooks.open_folder.clone();
        self.error = None;
        self.opening = Some(Pending::spawn(ctx, move || open(&dir)));
    }

    fn restart(&mut self, ctx: &egui::Context) {
        if !matches!(self.phase, Phase::Restored) {
            return;
        }
        let restart = self.hooks.restart.clone();
        self.error = None;
        self.phase = Phase::Restarting(Pending::spawn(ctx, move || restart()));
    }
}

fn after_reader_closed(
    ctx: &egui::Context,
    reader: mpsc::Sender<Cmd>,
    action: impl FnOnce() -> Result<RestoreOutcome> + Send + 'static,
) -> Pending<Result<RestoreOutcome>> {
    // Keep typed backend errors inside Pending's text-error boundary.
    Pending::spawn(ctx, move || {
        Ok(close_reader(&reader).and_then(|()| action()))
    })
}

fn close_reader(reader: &mpsc::Sender<Cmd>) -> Result<()> {
    let (ack, closed) = mpsc::channel();
    reader
        .send(Cmd::Pause(ack))
        .map_err(|_| anyhow::anyhow!("无法通知后台读取线程关闭工单数据库"))?;
    closed.recv().context("后台读取线程没有确认关闭工单数据库")
}

fn in_use(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.to_string() == sa_engine::owner::BUSY
            || cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
                io.kind() == std::io::ErrorKind::PermissionDenied
                    || (cfg!(windows) && matches!(io.raw_os_error(), Some(32 | 33)))
            })
    })
}

enum Seg<'a> {
    Text(&'a str),
    /// A path, shown in the inline-code style.
    Path(&'a Path),
}

/// One wrapped paragraph; paths use the monospace inline-code style.
fn paragraph(ui: &mut Ui, pal: &Palette, segs: &[Seg<'_>]) {
    let mut job = LayoutJob::default();
    job.wrap.break_anywhere = true;
    for seg in segs {
        match seg {
            Seg::Text(text) => job.append(
                text,
                0.0,
                TextFormat {
                    font_id: theme::body(theme::UI),
                    color: pal.ink,
                    line_height: Some(20.0),
                    ..Default::default()
                },
            ),
            Seg::Path(path) => job.append(
                &path.display().to_string(),
                0.0,
                TextFormat {
                    font_id: theme::mono(12.0),
                    color: pal.inline_code_fg,
                    background: pal.inline_code_bg,
                    line_height: Some(20.0),
                    ..Default::default()
                },
            ),
        }
    }
    ui.add(egui::Label::new(job).wrap());
}

fn reason(code: i32) -> Option<&'static str> {
    match code {
        11 => Some("文件内容损坏"),
        26 => Some("不是有效的数据库文件"),
        _ => None,
    }
}

const DAMAGED: &str = "hub-damaged-<时间戳>.sqlite3";

/// What a button press asked for; applied after the frame is drawn.
#[derive(Default)]
struct Pressed {
    restore: bool,
    cleanup: bool,
    open: bool,
    dismiss: bool,
    restart: bool,
}

fn body(ui: &mut Ui, pal: &Palette, panel: &RecoveryPanel) {
    ui.spacing_mut().item_spacing.y = 8.0;
    match &panel.kind {
        Kind::Interrupted => {
            if let Some(missing) = &panel.missing {
                paragraph(
                    ui,
                    pal,
                    &[
                        Seg::Text("找不到升级前的快照（"),
                        Seg::Path(missing),
                        Seg::Text("）。可以打开数据文件夹手动处理。"),
                    ],
                );
            } else if let Some(snapshot) = &panel.snapshot {
                let saved = snapshot
                    .saved
                    .map(|time| format!("（保存于 {}）", stamp(time)))
                    .unwrap_or_default();
                paragraph(
                    ui,
                    pal,
                    &[
                        Seg::Text("升级前的快照在："),
                        Seg::Path(&snapshot.path),
                        Seg::Text(&format!("{saved}。")),
                    ],
                );
                paragraph(
                    ui,
                    pal,
                    &[Seg::Text(&format!(
                        "从快照恢复后，工单数据库会回到那个时间点，之后的工单和对话不会保留。恢复前，当前文件会先另存为 {DAMAGED}，不会删除。"
                    ))],
                );
                paragraph(
                    ui,
                    pal,
                    &[Seg::Text(
                        "暂不恢复时，Super Agent 以只读方式打开，不能运行工单；下次启动会再次提示。",
                    )],
                );
            }
        }
        Kind::Corrupt { path, code } => {
            let why = reason(*code)
                .map(|why| format!("：{why}"))
                .unwrap_or_default();
            paragraph(
                ui,
                pal,
                &[
                    Seg::Text("无法读取 "),
                    Seg::Path(path),
                    Seg::Text(&format!(
                        "（SQLite 错误 {code}{why}）。Super Agent 已停止写入，没有改动这个文件。"
                    )),
                ],
            );
            if let Some(snapshot) = &panel.snapshot {
                let saved = snapshot
                    .saved
                    .map(|time| format!("（保存于 {}）", stamp(time)))
                    .unwrap_or_default();
                paragraph(
                    ui,
                    pal,
                    &[
                        Seg::Text("可以从最近的快照恢复："),
                        Seg::Path(&snapshot.path),
                        Seg::Text(&format!(
                            "{saved}。恢复后回到那个时间点，之后的工单和对话不会保留；当前文件会先另存为 {DAMAGED}。"
                        )),
                    ],
                );
            } else {
                paragraph(
                    ui,
                    pal,
                    &[Seg::Text(
                        "可以退出所有 Super Agent 窗口，把 hub.sqlite3 和同一目录下存在的 hub.sqlite3-wal、hub.sqlite3-shm、hub.sqlite3-journal 一起复制到别处保存，再对副本用 sqlite3 的 .recover 命令尝试抢救。",
                    )],
                );
                paragraph(
                    ui,
                    pal,
                    &[Seg::Text(
                        "如果不需要旧数据，可以退出后把这些文件一起移走或改名，再次打开 Super Agent 会新建空的工单数据库。",
                    )],
                );
            }
        }
        Kind::RestoreInterrupted {
            record,
            installed,
            reason,
        } => {
            if *installed {
                paragraph(
                    ui,
                    pal,
                    &[Seg::Text(&format!(
                        "快照已经装好，但升级标记还没清理：{reason}。"
                    ))],
                );
            } else {
                paragraph(
                    ui,
                    pal,
                    &[
                        Seg::Text(&format!(
                            "上次从快照恢复时中断，Super Agent 无法自动完成：{reason}。恢复记录在 "
                        )),
                        Seg::Path(record),
                        Seg::Text("。为避免数据丢失，没有创建新的工单数据库。"),
                    ],
                );
            }
        }
        Kind::InvalidMigrationMarker { path, reason } => {
            paragraph(
                ui,
                pal,
                &[
                    Seg::Path(path),
                    Seg::Text(&format!("：{reason}。为安全起见，没有继续升级。")),
                ],
            );
        }
    }
}

impl App {
    pub(crate) fn inspect_recovery(&mut self, ctx: &egui::Context, dir: PathBuf) {
        self.recovery_probe = Some(Pending::spawn(ctx, move || {
            Ok(sa_store::journal::inspect(&dir)
                .and_then(|error| RecoveryPanel::from_journal(&error, &dir, true)))
        }));
    }

    pub fn recovery_dialog(&mut self, ctx: &egui::Context) {
        if let Some(result) = self.recovery_probe.as_ref().and_then(Pending::poll) {
            self.recovery_probe = None;
            if let Ok(panel) = result {
                self.recovery = panel;
            }
        }
        let pal = self.pal.clone();
        let reader = &self.worker.tx;
        let Some(panel) = self.recovery.as_mut().filter(|panel| !panel.dismissed) else {
            return;
        };
        panel.poll(ctx, reader);
        let mut pressed = Pressed::default();
        let modal = egui::Modal::new(egui::Id::new("sa-recovery"))
            .frame(widgets::modal_frame(&pal))
            .show(ctx, |ui| {
                ui.set_width(540.0);
                if widgets::modal_header(ui, "", panel.title(), "", &pal) {
                    pressed.dismiss = true;
                }
                let restored = matches!(
                    panel.phase,
                    Phase::Restored | Phase::Restarting(_) | Phase::Closing
                );
                let restoring = matches!(panel.phase, Phase::Restoring(_));
                let cleaning = matches!(panel.phase, Phase::Cleaning(_));
                if restored {
                    ui.add(
                        egui::Label::new(
                            RichText::new("已从快照恢复。请重新启动 Super Agent。")
                                .font(theme::body(theme::UI))
                                .color(pal.ink),
                        )
                        .wrap(),
                    );
                } else if let Phase::CleanupRequired(reason) = &panel.phase {
                    paragraph(
                        ui,
                        &pal,
                        &[Seg::Text(&format!(
                            "已从快照恢复，但没能清理升级标记：{reason}。"
                        ))],
                    );
                } else if !cleaning {
                    body(ui, &pal, panel);
                }
                if restoring || cleaning {
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        widgets::spinner(ui);
                        ui.label(if cleaning {
                            "正在清理升级标记…"
                        } else {
                            "正在从快照恢复…"
                        });
                    });
                }
                if let Some(error) = &panel.error {
                    ui.add_space(10.0);
                    ui.add(egui::Label::new(RichText::new(error).color(pal.fail.fg)).wrap());
                }
                ui.add_space(16.0);
                let busy = panel.busy();
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if restored {
                        let launching =
                            matches!(panel.phase, Phase::Restarting(_) | Phase::Closing);
                        pressed.restart =
                            widgets::primary_button(ui, "", "立即重启", &pal, !launching).clicked();
                        return;
                    }
                    let can_cleanup = panel.can_cleanup();
                    if can_cleanup {
                        pressed.cleanup = widgets::primary_button(
                            ui,
                            "",
                            "重试清理",
                            &pal,
                            !busy && !panel.observer,
                        )
                        .clicked();
                    } else if panel.snapshot.is_some() {
                        pressed.restore = widgets::primary_button(
                            ui,
                            "",
                            "从快照恢复",
                            &pal,
                            !busy && !panel.observer && panel.identity.is_some(),
                        )
                        .clicked();
                    }
                    ui.add_enabled_ui(!busy, |ui| {
                        pressed.open =
                            widgets::outline_button(ui, "", "打开数据文件夹", &pal).clicked();
                        let can_defer = matches!(panel.kind, Kind::Interrupted)
                            || (matches!(panel.kind, Kind::Corrupt { .. })
                                && panel.snapshot.is_some());
                        if can_defer && !can_cleanup {
                            pressed.dismiss |=
                                widgets::quiet_button(ui, "", "暂不恢复", &pal, false).clicked();
                        }
                    });
                });
                if panel.observer && !restored {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("请在主窗口处理。")
                            .font(theme::body(theme::SMALL))
                            .color(pal.mute),
                    );
                }
            });
        if modal.should_close() && !panel.busy() && matches!(panel.phase, Phase::Ready) {
            pressed.dismiss = true;
        }
        if pressed.restore {
            panel.restore(ctx, reader.clone());
        }
        if pressed.cleanup {
            panel.cleanup(ctx, reader.clone());
        }
        if pressed.open {
            panel.open_data_folder(ctx);
        }
        if pressed.restart {
            panel.restart(ctx);
        }
        if pressed.dismiss && matches!(panel.phase, Phase::Ready) {
            panel.dismissed = true;
        }
    }
}
