//! Journal diagnostics and manual recovery instructions for the read-only window.

use crate::{app::App, ops::Pending, theme, theme::Palette, widgets};
use anyhow::{Context, Result};
use egui::{RichText, TextFormat, Ui, text::LayoutJob};
use sa_store::journal::JournalError;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

type OpenFolder = Arc<dyn Fn(&Path) -> Result<()> + Send + Sync>;

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

impl RecoveryPanel {
    pub(crate) fn from_journal(journal: &JournalError, data_dir: &Path, observer: bool) -> Self {
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
                snapshot: latest_snapshot
                    .as_ref()
                    .filter(|(path, _)| readable(path).is_some())
                    .cloned(),
            },
            JournalError::InvalidMigrationMarker { path, reason } => Kind::InvalidMigrationMarker {
                path: path.clone(),
                reason: reason.clone(),
            },
        };
        Self {
            kind,
            data_dir: data_dir.to_path_buf(),
            observer,
            dismissed: false,
            opening: None,
            error: None,
            open_folder: Arc::new(open_folder),
        }
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

    fn open_data_folder(&mut self, ctx: &egui::Context) {
        let dir = self.data_dir.clone();
        let open = self.open_folder.clone();
        self.error = None;
        self.opening = Some(Pending::spawn(ctx, move || open(&dir)));
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
    ui.add(egui::Label::new(job).wrap());
}

fn body(ui: &mut Ui, pal: &Palette, kind: &Kind) {
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
            paragraph(
                ui,
                pal,
                &[Seg::Text(
                    "可以这样手动恢复：1. 关闭所有 Super Agent 窗口。2. 把数据文件夹里的 hub.sqlite3 以及存在的 hub.sqlite3-wal、hub.sqlite3-shm、hub.sqlite3-journal 一起移到别处保存。3. 把快照复制到数据文件夹并改名为 hub.sqlite3。4. 删除 backups 文件夹里的 migration.json，然后重新打开 Super Agent。",
                )],
            );
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
                            "（保存于 {}）。可以按上面的步骤把它恢复为 hub.sqlite3（先关闭所有窗口并移走当前文件及其 -wal、-shm、-journal 文件）。",
                            stamp(*saved)
                        )),
                    ],
                );
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
        let mut open = false;
        let mut dismiss = false;
        let modal = egui::Modal::new(egui::Id::new("sa-recovery"))
            .frame(widgets::modal_frame(pal))
            .show(ctx, |ui| {
                ui.set_width(540.0);
                dismiss = widgets::modal_header(ui, "", panel.title(), "", pal);
                body(ui, pal, &panel.kind);
                if panel.observer {
                    ui.label(
                        RichText::new("请在主窗口处理。")
                            .font(theme::body(theme::SMALL))
                            .color(pal.mute),
                    );
                }
                if let Some(error) = &panel.error {
                    ui.add(egui::Label::new(RichText::new(error).color(pal.fail.fg)).wrap());
                }
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(panel.opening.is_none(), |ui| {
                        open = widgets::outline_button(ui, "", "打开数据文件夹", pal).clicked();
                    });
                    if !matches!(panel.kind, Kind::InvalidMigrationMarker { .. }) {
                        dismiss |= widgets::quiet_button(ui, "", "知道了", pal, false).clicked();
                    }
                });
            });
        if open {
            panel.open_data_folder(ctx);
        }
        if dismiss || modal.should_close() {
            panel.dismissed = true;
        }
    }
}
