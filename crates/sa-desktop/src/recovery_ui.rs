//! Journal diagnostics and snapshot recovery for startup failures.

use crate::{app::App, ops::Pending, theme, theme::Palette, widgets};
use anyhow::{Context, Result};
use egui::{RichText, TextFormat, Ui, text::LayoutJob};
use sa_store::journal::{
    JournalError, RestoreError, RestorePlan, Restored, journal_busy, restore_backup_stamp,
    restore_from_snapshot_at, restore_plan,
};
use std::{
    mem::{Discriminant, discriminant},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};

type OpenFolder = Arc<dyn Fn(&Path) -> Result<()> + Send + Sync>;
type RestoreResult = std::result::Result<Restored, RestoreError>;
type Restore = Arc<dyn Fn(&Path, &RestorePlan, SystemTime) -> RestoreResult + Send + Sync>;
type Relaunch = Arc<dyn Fn() -> Result<()> + Send + Sync>;

pub(crate) enum State {
    Ready,
    Busy,
    Confirm {
        time: SystemTime,
        folder_stamp: String,
    },
    Running(Pending<RestoreResult>),
    Succeeded(Restored),
    Relaunching(Restored, Pending<()>),
    RelaunchFailed(String),
    Untouched(String),
    Moved {
        reason: String,
        broken_dir: PathBuf,
    },
    Interrupted(String),
}

fn open_folder(dir: &Path) -> Result<()> {
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    // Explorer's exit status does not reliably indicate a launch failure.
    std::process::Command::new(program)
        .arg(dir)
        .spawn()
        .with_context(|| format!("无法启动 {program}"))?;
    Ok(())
}

enum Kind {
    Migration {
        snapshot: PathBuf,
        saved: Option<SystemTime>,
    },
    Corrupt {
        path: PathBuf,
        code: i32,
        snapshot: Option<(PathBuf, SystemTime)>,
    },
    InvalidMigrationMarker {
        path: PathBuf,
        reason: String,
    },
}

pub(crate) struct RecoveryPanel {
    kind: Kind,
    data_dir: PathBuf,
    observer: bool,
    dismissed: bool,
    plan: Option<RestorePlan>,
    reader_paused: Arc<AtomicBool>,
    pub(crate) state: State,
    measured_state: Option<Discriminant<State>>,
    pub(crate) restore: Restore,
    pub(crate) relaunch: Relaunch,
    opening: Option<Pending<()>>,
    pub(crate) error: Option<String>,
    pub(crate) open_folder: OpenFolder,
}

fn readable(path: &Path) -> Option<SystemTime> {
    let file = std::fs::File::open(path).ok()?;
    let meta = file.metadata().ok().filter(|m| m.is_file())?;
    Some(meta.modified().unwrap_or(SystemTime::UNIX_EPOCH))
}

fn stamp(time: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(time)
        .format("%Y/%-m/%-d %H:%M:%S")
        .to_string()
}

fn short_stamp(time: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(time)
        .format("%-m/%-d %H:%M")
        .to_string()
}

impl RecoveryPanel {
    pub(crate) fn from_journal(journal: &JournalError, data_dir: &Path, observer: bool) -> Self {
        let plan = if observer {
            None
        } else {
            restore_plan(data_dir, journal)
        };
        let kind = match journal {
            JournalError::MigrationInterrupted { snapshot, .. } => Kind::Migration {
                snapshot: snapshot.clone(),
                saved: readable(snapshot),
            },
            JournalError::Corrupt {
                path,
                code,
                latest_snapshot,
            } => Kind::Corrupt {
                path: path.clone(),
                code: *code,
                snapshot: plan
                    .as_ref()
                    .map(|plan| (plan.snapshot.clone(), plan.snapshot_time))
                    .or_else(|| {
                        latest_snapshot
                            .as_ref()
                            .filter(|(path, _)| readable(path).is_some())
                            .cloned()
                    }),
            },
            JournalError::InvalidMigrationMarker { path, reason } => Kind::InvalidMigrationMarker {
                path: path.clone(),
                reason: reason.clone(),
            },
        };
        let state = if plan.is_some() && journal_busy(data_dir) {
            State::Busy
        } else {
            State::Ready
        };
        Self {
            kind,
            data_dir: data_dir.to_path_buf(),
            observer,
            dismissed: false,
            reader_paused: Arc::new(AtomicBool::new(plan.is_some())),
            plan,
            state,
            measured_state: None,
            restore: Arc::new(restore_from_snapshot_at),
            relaunch: Arc::new(crate::relaunch),
            opening: None,
            error: None,
            open_folder: Arc::new(open_folder),
        }
    }

    pub(crate) fn gate_reader(&self, opener: crate::data::Opener) -> crate::data::Opener {
        let paused = Arc::clone(&self.reader_paused);
        Box::new(move || {
            anyhow::ensure!(!paused.load(Ordering::Acquire), "工单数据库等待恢复");
            opener()
        })
    }

    fn dismiss(&mut self) {
        self.dismissed = true;
        self.reader_paused.store(false, Ordering::Release);
    }

    fn title(&self) -> &'static str {
        match self.kind {
            Kind::Migration { .. } => "上次工单数据库升级没有完成",
            Kind::Corrupt { .. } => "工单数据库已损坏",
            Kind::InvalidMigrationMarker { .. } => "升级标记无法识别",
        }
    }

    fn poll_open(&mut self) {
        if let Some(result) = self.opening.as_ref().and_then(Pending::poll) {
            self.opening = None;
            if let Err(error) = result {
                self.error = Some(format!("无法打开数据文件夹：{error}"));
            }
        }
    }

    fn open_path(&mut self, ctx: &egui::Context, dir: PathBuf) {
        let open = self.open_folder.clone();
        self.error = None;
        self.opening = Some(Pending::spawn(ctx, move || open(&dir)));
    }

    fn confirm(&mut self) {
        let time = SystemTime::now();
        self.state = match restore_backup_stamp(time) {
            Ok(folder_stamp) => State::Confirm { time, folder_stamp },
            Err(error) => State::Untouched(format!("{error:#}")),
        };
    }

    fn start_restore(&mut self, ctx: &egui::Context, time: SystemTime) {
        let Some(plan) = self.plan.clone() else {
            return;
        };
        let dir = self.data_dir.clone();
        let restore = Arc::clone(&self.restore);
        self.state = State::Running(Pending::spawn(ctx, move || Ok(restore(&dir, &plan, time))));
    }

    fn poll_restore(&mut self) -> bool {
        match &self.state {
            State::Running(task) => {
                if let Some(result) = task.poll() {
                    self.state = match result {
                        Ok(Ok(restored)) => State::Succeeded(restored),
                        Ok(Err(RestoreError::Busy)) => State::Busy,
                        Ok(Err(RestoreError::Untouched { reason })) => State::Untouched(reason),
                        Ok(Err(RestoreError::Moved { reason, broken_dir })) => {
                            State::Moved { reason, broken_dir }
                        }
                        Err(reason) => State::Interrupted(reason),
                    };
                }
            }
            State::Relaunching(_, task) => {
                if let Some(result) = task.poll() {
                    match result {
                        Ok(()) => return true,
                        Err(reason) => self.state = State::RelaunchFailed(reason),
                    }
                }
            }
            _ => {}
        }
        false
    }

    fn data_folder_button(&mut self, ui: &mut Ui, pal: &Palette) {
        ui.add_enabled_ui(self.opening.is_none(), |ui| {
            if widgets::outline_button(ui, "", "打开数据文件夹", pal).clicked() {
                self.open_path(ui.ctx(), self.data_dir.clone());
            }
        });
    }

    fn diagnostics(&mut self, ui: &mut Ui, pal: &Palette) -> bool {
        let mut dismiss = widgets::modal_header(ui, "", self.title(), "", pal);
        body(ui, pal, &self.kind, self.plan.is_some());
        if self.observer {
            ui.label(
                RichText::new("请在主窗口处理。")
                    .font(theme::body(theme::SMALL))
                    .color(pal.mute),
            );
        }
        let busy = matches!(self.state, State::Busy);
        if busy {
            paragraph(
                ui,
                pal,
                &[Seg::Text(
                    "检测到其他 Super Agent 窗口正在运行，请先全部关闭。",
                )],
            );
        }
        ui.add_space(16.0);
        ui.horizontal_wrapped(|ui| {
            if let Some(plan) = &self.plan {
                let label = match self.kind {
                    Kind::Migration { .. } => "用升级前的快照恢复".to_owned(),
                    _ => format!("用最近快照恢复（{}）", short_stamp(plan.snapshot_time)),
                };
                if widgets::primary_button(ui, "", &label, pal, !busy).clicked() {
                    self.confirm();
                }
            }
            if busy
                && widgets::outline_button(ui, "", "重新检查", pal).clicked()
                && !journal_busy(&self.data_dir)
            {
                self.state = State::Ready;
            }
            self.data_folder_button(ui, pal);
            if !matches!(self.kind, Kind::InvalidMigrationMarker { .. }) {
                dismiss |= widgets::quiet_button(ui, "", "知道了", pal, false).clicked();
            }
        });
        dismiss
    }

    fn contents(&mut self, ui: &mut Ui, pal: &Palette) -> bool {
        let dismiss = match &self.state {
            State::Ready | State::Busy => self.diagnostics(ui, pal),
            State::Confirm { time, folder_stamp } => {
                let time = *time;
                let close = widgets::modal_header(ui, "", "确认从快照恢复", "", pal);
                let snapshot_time = stamp(
                    self.plan
                        .as_ref()
                        .expect("confirmation requires a plan")
                        .snapshot_time,
                );
                paragraph(
                    ui,
                    pal,
                    &[Seg::Text(&format!(
                        "恢复会做两件事：\n1. 把当前的 hub.sqlite3 及其 -wal、-shm、-journal 文件移到 backups\\broken-{folder_stamp}\\ 保存，不会删除；\n2. 把 {snapshot_time} 的快照复制为新的 hub.sqlite3。\n恢复完成后需要重新打开 Super Agent。快照之后产生的工单记录不会出现在恢复后的数据库里，但原来的文件会一直保留。"
                    ))],
                );
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    if widgets::primary_button(ui, "", "确认恢复", pal, true).clicked() {
                        self.start_restore(ui.ctx(), time);
                    }
                    if close || widgets::quiet_button(ui, "", "取消", pal, false).clicked() {
                        self.state = State::Ready;
                    }
                });
                false
            }
            State::Running(_) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    paragraph(ui, pal, &[Seg::Text("正在恢复…")]);
                });
                false
            }
            State::Succeeded(restored) | State::Relaunching(restored, _) => {
                let launching = matches!(self.state, State::Relaunching(..));
                paragraph(
                    ui,
                    pal,
                    &[
                        Seg::Text(&format!(
                            "已恢复到 {} 的快照。原来的文件保存在：",
                            stamp(restored.snapshot_time)
                        )),
                        Seg::Path(&restored.broken_dir),
                    ],
                );
                let backup = restored.broken_dir.clone();
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    if widgets::primary_button(ui, "", "重新打开", pal, !launching).clicked() {
                        let relaunch = Arc::clone(&self.relaunch);
                        let task = Pending::spawn(ui.ctx(), move || relaunch());
                        let State::Succeeded(restored) =
                            std::mem::replace(&mut self.state, State::Ready)
                        else {
                            unreachable!("only an idle success can relaunch");
                        };
                        self.state = State::Relaunching(restored, task);
                    }
                    ui.add_enabled_ui(self.opening.is_none(), |ui| {
                        if widgets::outline_button(ui, "", "打开备份文件夹", pal).clicked() {
                            self.open_path(ui.ctx(), backup);
                        }
                    });
                });
                false
            }
            State::RelaunchFailed(reason) => {
                paragraph(
                    ui,
                    pal,
                    &[Seg::Text(&format!(
                        "无法自动重新打开：{reason}。请手动关闭并重新打开 Super Agent。"
                    ))],
                );
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    self.data_folder_button(ui, pal);
                    if widgets::quiet_button(ui, "", "关闭窗口", pal, false).clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                false
            }
            State::Untouched(_) | State::Moved { .. } | State::Interrupted(_) => {
                let can_retry = matches!(self.state, State::Untouched(_));
                match &self.state {
                    State::Untouched(reason) => paragraph(
                        ui,
                        pal,
                        &[Seg::Text(&format!(
                            "恢复没有完成：{reason}。没有改动任何文件"
                        ))],
                    ),
                    State::Moved { reason, broken_dir } => paragraph(
                        ui,
                        pal,
                        &[
                            Seg::Text(&format!("恢复没有完成：{reason}。原来的文件保存在：")),
                            Seg::Path(broken_dir),
                        ],
                    ),
                    State::Interrupted(reason) => {
                        paragraph(ui, pal, &[Seg::Text(&format!("恢复没有完成：{reason}"))])
                    }
                    _ => unreachable!(),
                }
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    if can_retry && widgets::primary_button(ui, "", "重试", pal, true).clicked() {
                        self.confirm();
                    }
                    self.data_folder_button(ui, pal);
                    widgets::quiet_button(ui, "", "知道了", pal, false).clicked()
                })
                .inner
            }
        };
        if let Some(error) = &self.error {
            ui.add(egui::Label::new(RichText::new(error).color(pal.fail.fg)).wrap());
        }
        dismiss
    }

    fn prevents_close(&self) -> bool {
        matches!(
            self.state,
            State::Running(_) | State::Succeeded(_) | State::Relaunching(..)
        )
    }
}

enum Seg<'a> {
    Text(&'a str),
    Path(&'a Path),
}

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
    ui.add(egui::Label::new(job).wrap().selectable(true));
}

fn body(ui: &mut Ui, pal: &Palette, kind: &Kind, offer: bool) {
    ui.spacing_mut().item_spacing.y = 8.0;
    match kind {
        Kind::Migration {
            snapshot,
            saved: Some(saved),
        } => {
            paragraph(
                ui,
                pal,
                &[
                    Seg::Text("升级前的快照在："),
                    Seg::Path(snapshot),
                    Seg::Text(&format!(
                        "（保存于 {}）。Super Agent 已停止调度，没有继续升级。",
                        stamp(*saved)
                    )),
                ],
            );
            manual(ui, offer, |ui| {
                paragraph(
                    ui,
                    pal,
                    &[Seg::Text(
                        "可以这样手动恢复：1. 关闭所有 Super Agent 窗口。2. 把数据文件夹里的 hub.sqlite3 以及存在的 hub.sqlite3-wal、hub.sqlite3-shm、hub.sqlite3-journal 一起移到别处保存。3. 把快照复制到数据文件夹并改名为 hub.sqlite3。4. 删除 backups 文件夹里的 migration.json，然后重新打开 Super Agent。",
                    )],
                );
            });
        }
        Kind::Migration {
            snapshot,
            saved: None,
        } => {
            paragraph(
                ui,
                pal,
                &[
                    Seg::Text("找不到升级前的快照（"),
                    Seg::Path(snapshot),
                    Seg::Text(
                        "）。Super Agent 已停止调度，没有继续升级。可以打开数据文件夹手动处理。",
                    ),
                ],
            );
        }
        Kind::Corrupt {
            path,
            code,
            snapshot,
        } => {
            let reason = match code {
                11 => "文件内容损坏",
                26 => "不是有效的数据库文件",
                _ => "无法识别的数据库错误",
            };
            paragraph(
                ui,
                pal,
                &[
                    Seg::Text("无法读取 "),
                    Seg::Path(path),
                    Seg::Text(&format!(
                        "（SQLite 错误 {code}：{reason}）。Super Agent 已停止调度，尚未执行任何恢复。"
                    )),
                ],
            );
            if let Some((path, saved)) = snapshot {
                paragraph(
                    ui,
                    pal,
                    &[
                        Seg::Text("有可用的快照："),
                        Seg::Path(path),
                        Seg::Text(&format!(
                            "（保存于 {}）。{}",
                            stamp(*saved),
                            if offer { "" } else { CORRUPT_MANUAL }
                        )),
                    ],
                );
            }
            manual(ui, offer, |ui| {
                if offer {
                    paragraph(ui, pal, &[Seg::Text(CORRUPT_MANUAL)]);
                }
                paragraph(
                    ui,
                    pal,
                    &[Seg::Text(
                        "也可以退出所有 Super Agent 窗口，把 hub.sqlite3 和同一目录下存在的 hub.sqlite3-wal、hub.sqlite3-shm、hub.sqlite3-journal 一起复制到别处保存，再对副本用 sqlite3 的 .recover 命令尝试抢救。",
                    )],
                );
                paragraph(
                    ui,
                    pal,
                    &[Seg::Text(
                        "如果不需要旧数据，可以退出后把这些文件一起移走或改名，再次打开 Super Agent 会新建空的工单数据库。",
                    )],
                );
            });
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

const CORRUPT_MANUAL: &str = "可以按上面的步骤把它恢复为 hub.sqlite3（先关闭所有窗口并移走当前文件及其 -wal、-shm、-journal 文件）。";

fn manual(ui: &mut Ui, offer: bool, show: impl FnOnce(&mut Ui)) {
    if offer {
        egui::CollapsingHeader::new("手动恢复步骤").show(ui, show);
    } else {
        show(ui);
    }
}

impl App {
    pub(crate) fn inspect_recovery(&mut self, ctx: &egui::Context, dir: PathBuf) {
        self.recovery_probe = Some(Pending::spawn(ctx, move || {
            Ok(sa_store::journal::inspect(&dir)
                .map(|error| RecoveryPanel::from_journal(&error, &dir, true)))
        }));
    }

    pub fn recovery_dialog(&mut self, ctx: &egui::Context) {
        if let Some(result) = self.recovery_probe.as_ref().and_then(Pending::poll) {
            self.recovery_probe = None;
            if let Ok(panel) = result {
                self.recovery = panel;
            }
        }
        let pal = &self.pal;
        let Some(panel) = self.recovery.as_mut().filter(|panel| !panel.dismissed) else {
            return;
        };
        panel.poll_open();
        if panel.poll_restore() {
            panel.dismissed = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        let state = discriminant(&panel.state);
        let sizing_pass = panel.measured_state.replace(state) != Some(state);
        if sizing_pass {
            // Measure the new contents before presenting them at the modal's new center.
            ctx.request_discard("size the recovery dialog");
        }
        let id = egui::Id::new("sa-recovery");
        let modal = egui::Modal::new(id)
            .area(egui::Modal::default_area(id).sizing_pass(sizing_pass))
            .frame(widgets::modal_frame(pal))
            .show(ctx, |ui| {
                ui.set_width(540.0);
                ui.set_max_height(ctx.content_rect().height());
                panel.contents(ui, pal)
            });
        if panel.prevents_close() {
            if ctx.input(|input| input.viewport().close_requested()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
        } else if !sizing_pass && (modal.inner || modal.should_close()) {
            if matches!(panel.state, State::Confirm { .. }) {
                panel.state = State::Ready;
            } else if !matches!(panel.state, State::RelaunchFailed(_)) {
                panel.dismiss();
            }
        }
        if discriminant(&panel.state) != state {
            // An action changed the state while drawing the previous view.
            ctx.request_repaint();
        }
    }
}
