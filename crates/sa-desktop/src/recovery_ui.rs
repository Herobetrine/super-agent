//! Recovery panel for a ticket database that did not start: an interrupted
//! migration (design C2) and a corrupt journal (design C3).
//!
//! `Host::start` keeps the typed [`JournalError`] in its `anyhow::Error`; the
//! desktop copies what the panel needs into a [`RecoveryPanel`] and shows it
//! as a modal over the read-only window. Every filesystem or process action
//! runs on a background thread.

use crate::{app::App, host::Host, ops::Pending, theme, theme::Palette, widgets};
use anyhow::{Context, Result};
use egui::{RichText, TextFormat, Ui, ViewportCommand, text::LayoutJob};
use sa_store::journal::JournalError;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

/// Spawns the current executable again with the same arguments.
type Restart = Arc<dyn Fn() -> Result<()> + Send + Sync>;
/// Opens a folder in the system file manager.
type OpenFolder = Arc<dyn Fn(&Path) -> Result<()> + Send + Sync>;

#[derive(Clone)]
pub(crate) struct Hooks {
    pub restart: Restart,
    pub open_folder: OpenFolder,
}

impl Default for Hooks {
    fn default() -> Self {
        Self {
            restart: Arc::new(restart_self),
            open_folder: Arc::new(open_folder),
        }
    }
}

fn restart_self() -> Result<()> {
    let exe = std::env::current_exe().context("无法确定程序路径")?;
    std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .spawn()
        .context("无法启动新的 Super Agent")?;
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
    Corrupt { path: PathBuf, code: i32 },
}

pub(crate) struct Snapshot {
    pub path: PathBuf,
    /// Modification time of the snapshot file.
    pub saved: Option<SystemTime>,
}

pub(crate) enum Phase {
    Ready,
    Restoring(Pending<()>),
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
    /// `None` unless the startup error carries a [`JournalError`].
    pub fn from_error(error: &anyhow::Error, data_dir: &Path, observer: bool) -> Option<Self> {
        let journal = error.downcast_ref::<JournalError>()?;
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
        })
    }

    pub fn title(&self) -> &'static str {
        match self.kind {
            Kind::Interrupted => "上次工单数据库升级没有完成",
            Kind::Corrupt { .. } => "工单数据库已损坏",
        }
    }

    fn busy(&self) -> bool {
        matches!(
            self.phase,
            Phase::Restoring(_) | Phase::Restarting(_) | Phase::Closing
        )
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(result) = self.opening.as_ref().and_then(Pending::poll) {
            self.opening = None;
            if let Err(error) = result {
                self.error = Some(format!("无法打开数据文件夹：{error}"));
            }
        }
        match &self.phase {
            Phase::Restoring(pending) => {
                if let Some(result) = pending.poll() {
                    match result {
                        Ok(()) => {
                            self.phase = Phase::Restored;
                            self.error = None;
                        }
                        Err(error) => {
                            self.phase = Phase::Ready;
                            self.error = Some(format!("恢复失败：{error}。当前文件没有改动。"));
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
                            self.error =
                                Some(format!("无法重新启动：{error}。请手动重新启动 Super Agent。"));
                        }
                    }
                }
            }
            Phase::Ready | Phase::Restored | Phase::Closing => {}
        }
    }

    fn restore(&mut self, ctx: &egui::Context) {
        let Some(snapshot) = self.snapshot.as_ref().map(|s| s.path.clone()) else {
            return;
        };
        if self.observer || self.busy() {
            return;
        }
        let dir = self.data_dir.clone();
        self.error = None;
        self.phase = Phase::Restoring(Pending::spawn(ctx, move || {
            Host::restore_snapshot(&dir, &snapshot)
        }));
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
                        "可以退出 Super Agent，先把这个文件复制到别处保存，再用 sqlite3 的 .recover 命令尝试抢救。",
                    )],
                );
                paragraph(
                    ui,
                    pal,
                    &[Seg::Text(
                        "如果不需要旧数据，可以退出后把这个文件移走或改名，再次打开 Super Agent 会新建空的工单数据库。",
                    )],
                );
            }
        }
    }
}

impl App {
    /// Keep what the recovery panel needs from a failed `Host::start`.
    pub(crate) fn set_start_error(&mut self, error: &anyhow::Error) {
        let dir = self
            .opts
            .data
            .clone()
            .unwrap_or_else(sa_store::default_data_dir);
        self.recovery = RecoveryPanel::from_error(error, &dir, self.opts.observe || self.opts.demo);
    }

    pub fn recovery_dialog(&mut self, ctx: &egui::Context) {
        let pal = self.pal.clone();
        let Some(panel) = self.recovery.as_mut().filter(|panel| !panel.dismissed) else {
            return;
        };
        panel.poll(ctx);
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
                if restored {
                    ui.add(
                        egui::Label::new(
                            RichText::new("已从快照恢复。请重新启动 Super Agent。")
                                .font(theme::body(theme::UI))
                                .color(pal.ink),
                        )
                        .wrap(),
                    );
                } else {
                    body(ui, &pal, panel);
                }
                if restoring {
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        widgets::spinner(ui);
                        ui.label("正在从快照恢复…");
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
                        let launching = matches!(panel.phase, Phase::Restarting(_) | Phase::Closing);
                        pressed.restart =
                            widgets::primary_button(ui, "", "立即重启", &pal, !launching).clicked();
                        return;
                    }
                    let can_restore = panel.snapshot.is_some();
                    if can_restore {
                        pressed.restore = widgets::primary_button(
                            ui,
                            "",
                            "从快照恢复",
                            &pal,
                            !busy && !panel.observer,
                        )
                        .clicked();
                    }
                    ui.add_enabled_ui(!busy, |ui| {
                        pressed.open =
                            widgets::outline_button(ui, "", "打开数据文件夹", &pal).clicked();
                        // C3 without a snapshot offers the folder only.
                        let folder_only = matches!(panel.kind, Kind::Corrupt { .. }) && !can_restore;
                        if !folder_only {
                            pressed.dismiss |=
                                widgets::quiet_button(ui, "", "暂不恢复", &pal, false).clicked();
                        }
                    });
                });
                if panel.snapshot.is_some() && panel.observer && !restored {
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
            panel.restore(ctx);
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
