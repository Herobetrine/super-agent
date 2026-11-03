//! Inspector "知识" tab: the project's shared knowledge base (memory,
//! lessons, progress) that every agent of the project sees in its turns.
//! Reads go through a read-only connection (observers too); edits are host
//! actions.

use crate::app::App;
use crate::host::Action;
use crate::labels;
use crate::ops::Pending;
use crate::theme;
use crate::widgets;
use egui::{Align, CornerRadius, Frame, Label, Layout, Margin, RichText, Stroke, Ui};
use egui_phosphor::regular as icon;
use sa_store::knowledge::KnowledgeEntry;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const REFRESH: Duration = Duration::from_secs(4);

pub struct KnowledgeState {
    pub cwd: Option<String>,
    pub entries: Option<Result<Vec<KnowledgeEntry>, String>>,
    pub pending: Option<Pending<Vec<KnowledgeEntry>>>,
    pub loaded_at: Option<Instant>,
    pub draft: String,
    pub kind: String,
    pub show_archived: bool,
    /// Body of the last submitted add (cleared on success).
    pub last: Option<String>,
}

impl Default for KnowledgeState {
    fn default() -> Self {
        Self {
            cwd: None,
            entries: None,
            pending: None,
            loaded_at: None,
            draft: String::new(),
            kind: "memory".into(),
            show_archived: false,
            last: None,
        }
    }
}

pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        "memory" => "记忆",
        "lesson" => "经验",
        "progress" => "进展",
        _ => "条目",
    }
}

impl App {
    fn load_knowledge(&mut self, ctx: &egui::Context, cwd: String) {
        if self.knowledge.pending.is_some() {
            return;
        }
        let data = self.opts.data.clone().unwrap_or_else(sa_store::default_data_dir);
        let key = sa_engine::services::realpath(&PathBuf::from(&cwd))
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| cwd.clone());
        if self.knowledge.cwd.as_deref() != Some(cwd.as_str()) {
            self.knowledge.entries = None;
        }
        self.knowledge.cwd = Some(cwd);
        self.knowledge.pending = Some(Pending::spawn(ctx, move || sa_store::knowledge::read(&data, &key)));
    }

    pub fn poll_knowledge(&mut self) {
        if let Some(r) = self.knowledge.pending.as_ref().and_then(Pending::poll) {
            self.knowledge.pending = None;
            self.knowledge.entries = Some(r);
            self.knowledge.loaded_at = Some(Instant::now());
        }
    }

    /// `ActionKind::Knowledge` outcome.
    pub fn knowledge_saved(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => {
                if self.knowledge.last.as_deref() == Some(self.knowledge.draft.as_str()) {
                    self.knowledge.draft.clear();
                }
                self.knowledge.loaded_at = None;
            }
            Err(e) => self.composer.message = Some((e, true)),
        }
    }

    pub fn knowledge_tab(&mut self, ui: &mut Ui, cwd: Option<String>, ticket: Option<i64>) {
        let pal = self.pal.clone();
        let Some(cwd) = cwd else {
            crate::inspector::empty(ui, icon::BRAIN, "选择工单或项目目录后查看项目知识库。", &pal);
            return;
        };
        let stale = self.knowledge.cwd.as_deref() != Some(cwd.as_str())
            || self.knowledge.loaded_at.is_none_or(|t| t.elapsed() > REFRESH);
        if stale {
            self.load_knowledge(ui.ctx(), cwd.clone());
        }
        ui.ctx().request_repaint_after(REFRESH);
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} {}", icon::FOLDER_SIMPLE, labels::basename(&cwd))).color(pal.mute))
                .on_hover_text(&cwd);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if self.knowledge.pending.is_some() {
                    ui.spinner();
                }
                ui.checkbox(&mut self.knowledge.show_archived, "显示已归档");
            });
        });
        ui.add(
            Label::new(
                RichText::new("同一项目的所有执行者在每轮上下文中共享这些条目；执行者用 SA_MEMO 写入，主控完成工单时自动记录进展。")
                    .font(theme::body(theme::TINY + 0.5))
                    .color(pal.mute),
            )
            .wrap(),
        );
        ui.add_space(8.0);

        let mut action: Option<Action> = None;
        if self.writable() {
            Frame::new()
                .fill(pal.surface)
                .stroke(Stroke::new(1.0_f32, pal.line_soft))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::same(10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let resp = ui.add(
                        egui::TextEdit::multiline(&mut self.knowledge.draft)
                            .id_salt("knowledge-draft")
                            .hint_text("记录项目事实、经验或进展（勿含密钥）…")
                            .desired_rows(2)
                            .desired_width(f32::INFINITY),
                    );
                    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "知识条目输入"));
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("knowledge-kind")
                            .width(84.0)
                            .selected_text(kind_label(&self.knowledge.kind))
                            .show_ui(ui, |ui| {
                                for k in sa_store::knowledge::KINDS {
                                    ui.selectable_value(&mut self.knowledge.kind, k.to_string(), kind_label(k));
                                }
                            });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let ok = !self.knowledge.draft.trim().is_empty() && self.composer.pending == 0;
                            if ui
                                .add_enabled_ui(ok, |ui| widgets::quiet_button(ui, icon::PLUS, "添加条目", &pal, true))
                                .inner
                                .clicked()
                            {
                                self.knowledge.last = Some(self.knowledge.draft.clone());
                                action = Some(Action::KnowledgeAdd {
                                    cwd: cwd.clone(),
                                    kind: self.knowledge.kind.clone(),
                                    body: self.knowledge.draft.clone(),
                                    ticket,
                                });
                            }
                        });
                    });
                });
            ui.add_space(8.0);
        }

        match &self.knowledge.entries {
            None => crate::inspector::note(ui, "正在读取项目知识库…", &pal),
            Some(Err(e)) => {
                ui.add(Label::new(RichText::new(e).color(pal.fail.fg)).wrap());
            }
            Some(Ok(rows)) => {
                let shown: Vec<&KnowledgeEntry> =
                    rows.iter().filter(|e| self.knowledge.show_archived || !e.archived).collect();
                if shown.is_empty() {
                    crate::inspector::empty(ui, icon::BRAIN, "这个项目还没有共享知识。", &pal);
                }
                for e in shown {
                    Frame::new()
                        .fill(if e.archived { pal.canvas } else { pal.surface })
                        .stroke(Stroke::new(1.0_f32, pal.line_soft))
                        .corner_radius(CornerRadius::same(10))
                        .inner_margin(Margin::same(10))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let tone = match e.kind.as_str() {
                                    "lesson" => pal.warn,
                                    "progress" => pal.ok,
                                    _ => pal.neutral,
                                };
                                widgets::chip(ui, kind_label(&e.kind), tone, theme::TINY);
                                let who = match e.agent.as_deref() {
                                    Some("operator") => "运营方".to_string(),
                                    Some(a) => labels::agent_name(a),
                                    None => "执行者".into(),
                                };
                                let mut meta = who;
                                if let Some(t) = e.ticket {
                                    meta.push_str(&format!(" · 工单 #{t}"));
                                }
                                if e.archived {
                                    meta.push_str(" · 已归档");
                                }
                                ui.label(RichText::new(meta).font(theme::body(theme::TINY + 0.5)).color(pal.mute));
                                if self.writable() {
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        let (ic, text) =
                                            if e.archived { (icon::ARROW_COUNTER_CLOCKWISE, "恢复") } else { (icon::ARCHIVE, "归档") };
                                        let label = format!("{text}条目 #{}", e.id);
                                        let r = widgets::icon_button(ui, ic, 24.0, pal.mute, pal.hover, false).on_hover_text(text);
                                        r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
                                        if r.clicked() {
                                            action = Some(Action::KnowledgeArchive {
                                                id: e.id,
                                                archived: !e.archived,
                                            });
                                        }
                                    });
                                }
                            });
                            let color = if e.archived { pal.mute } else { pal.ink };
                            ui.add(Label::new(RichText::new(&e.body).font(theme::body(13.0)).color(color)).wrap());
                        });
                    ui.add_space(6.0);
                }
            }
        }
        if let Some(a) = action {
            self.submit(a);
        }
    }
}
