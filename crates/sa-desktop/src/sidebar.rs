//! Left sidebar: brand, new conversation, search, tickets grouped by project
//! folder, and the settings row at the foot.

use crate::app::App;
use crate::host::Action;
use crate::labels;
use crate::notifications::Mode;
use crate::theme::{self, Palette};
use crate::widgets;
use egui::{Align2, CornerRadius, Frame, Margin, RichText, Sense, Stroke, Ui, pos2, vec2};
use egui_phosphor::regular as icon;
use sa_store::TicketSummary;

struct Group {
    key: String,
    cwd: String,
    parts: Vec<String>,
    rows: Vec<TicketSummary>,
}

fn project_key(cwd: &str) -> String {
    let n = cwd.replace('\\', "/");
    let n = n.trim_end_matches('/');
    let windows = n.len() >= 2 && n.as_bytes()[1] == b':' || n.starts_with("//");
    if windows {
        n.to_lowercase()
    } else {
        n.to_string()
    }
}

fn attention_reason(row: &TicketSummary) -> Option<&'static str> {
    if row.archived {
        return None;
    }
    match row.attention.as_deref() {
        Some("approval") => Some("待审批"),
        Some("question") => Some("等你回答"),
        _ => None,
    }
}

fn groups(rows: &[TicketSummary]) -> Vec<Group> {
    let mut out: Vec<Group> = Vec::new();
    for r in rows {
        let key = project_key(&r.cwd);
        match out.iter_mut().find(|g| g.key == key) {
            Some(g) => g.rows.push(r.clone()),
            None => out.push(Group {
                key,
                cwd: r.cwd.clone(),
                parts: r
                    .cwd
                    .split(['\\', '/'])
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect(),
                rows: vec![r.clone()],
            }),
        }
    }
    out
}

/// Shortest distinct path suffix among groups sharing a folder name (workbench.js).
fn display_name(g: &Group, all: &[Group]) -> String {
    let suffix = |g: &Group, n: usize| g.parts[g.parts.len().saturating_sub(n)..].join("/");
    let name = g.parts.last().cloned().unwrap_or_else(|| "无项目".into());
    let mut depth = 1;
    while depth < g.parts.len()
        && all.iter().any(|o| {
            o.key != g.key
                && !o.parts.is_empty()
                && suffix(o, depth).to_lowercase() == suffix(g, depth).to_lowercase()
        })
    {
        depth += 1;
    }
    if g.parts.is_empty() {
        name
    } else {
        suffix(g, depth)
    }
}

/// Row height of a ticket; one line, so a screen holds many.
const ROW: f32 = 32.0;
/// Horizontal centre of the state mark and of the group caret.
const MARK_X: f32 = 15.0;
/// Where titles and group names start.
const TEXT_X: f32 = 28.0;

/// The state mark in front of a ticket title.
fn state_mark(ui: &Ui, c: egui::Pos2, state: &str, pal: &Palette) {
    let p = ui.painter();
    match state {
        "running" | "started" => widgets::dot(ui, c, pal.run.fg, true),
        "queued" | "waiting" | "draining" => {
            p.circle_stroke(c, 3.25, Stroke::new(1.5_f32, pal.wait.fg));
        }
        "partial" => widgets::dot(ui, c, pal.warn.fg, false),
        "failed" | "error" => widgets::dot(ui, c, pal.fail.fg, false),
        "cancelled" | "interrupted" | "stopped" => {
            p.circle_stroke(c, 3.25, Stroke::new(1.5_f32, pal.faint));
        }
        "done" | "completed" | "success" | "reported" => {
            p.text(c, Align2::CENTER_CENTER, icon::CHECK, theme::body(12.0), pal.faint);
        }
        _ => widgets::dot(ui, c, pal.faint, false),
    }
}

impl App {
    pub fn sidebar(&mut self, ui: &mut Ui) {
        let pal = self.pal.clone();
        egui::TopBottomPanel::bottom("sidebar-foot")
            .frame(Frame::new().inner_margin(Margin {
                left: 10,
                right: 8,
                top: 6,
                bottom: 10,
            }))
            .show_separator_line(false)
            .show_inside(ui, |ui| self.sidebar_foot(ui));

        Frame::new()
            .inner_margin(Margin {
                left: 12,
                right: 8,
                top: 16,
                bottom: 0,
            })
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.add_space(2.0);
                    let (r, _) = ui.allocate_exact_size(vec2(22.0, 30.0), Sense::hover());
                    widgets::brand_mark(
                        ui,
                        egui::Rect::from_center_size(r.center(), vec2(22.0, 22.0)),
                        &pal,
                    );
                    ui.label(
                        RichText::new("Super Agent")
                            .font(theme::strong(14.0))
                            .color(pal.ink),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if widgets::panel_button(ui, false, 30.0, &pal, true)
                            .on_hover_text("收起会话侧栏")
                            .clicked()
                        {
                            self.sidebar_open = false;
                        }
                    });
                });
                ui.add_space(12.0);
                Frame::new()
                    .inner_margin(Margin {
                        left: 0,
                        right: 4,
                        top: 0,
                        bottom: 0,
                    })
                    .show(ui, |ui| {
                        if self.writable() {
                            if widgets::wide_button(ui, icon::NOTE_PENCIL, "新建对话", "Ctrl N", &pal)
                                .on_hover_text("开始一个新会话")
                                .clicked()
                            {
                                self.new_conversation();
                            }
                            ui.add_space(8.0);
                        }
                        self.search_box(ui);
                    });
                ui.add_space(10.0);
            });

        let out = egui::ScrollArea::vertical()
            .id_salt("history")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                Frame::new()
                    .inner_margin(Margin {
                        left: 8,
                        right: 8,
                        top: 0,
                        bottom: 12,
                    })
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        if !self.history_loaded {
                            ui.add_space(20.0);
                            ui.horizontal(|ui| {
                                ui.add_space(8.0);
                                widgets::spinner(ui);
                                ui.label(RichText::new("正在读取会话…").font(theme::body(theme::UI)).color(pal.mute));
                            });
                            return;
                        }
                        if self.history.is_empty() {
                            ui.add_space(28.0);
                            ui.vertical_centered(|ui| {
                                let text = if self.source.error.is_some() {
                                    "暂时无法读取会话"
                                } else if self.search.is_empty() {
                                    "还没有会话。"
                                } else {
                                    "没找到匹配的会话。"
                                };
                                ui.label(RichText::new(text).font(theme::body(theme::UI)).color(pal.mute));
                            });
                            return;
                        }
                        let searching = !self.search.trim().is_empty();
                        let archived = self.history.iter().filter(|r| r.archived).count();
                        let shown: Vec<TicketSummary> = self
                            .history
                            .iter()
                            .filter(|r| self.show_archived || !r.archived)
                            .cloned()
                            .collect();
                        let (attention, remaining): (Vec<TicketSummary>, Vec<TicketSummary>) =
                            shown.into_iter().partition(|r| attention_reason(r).is_some());
                        let (pinned, rest): (Vec<TicketSummary>, Vec<TicketSummary>) =
                            remaining.into_iter().partition(|r| r.pinned && !r.archived);
                        let all = groups(&rest);
                        let mut clicked = None;
                        let mut action = None;
                        if !attention.is_empty() {
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.add_space(TEXT_X);
                                ui.label(RichText::new("待你处理").font(theme::strong(theme::SMALL)).color(pal.mute));
                            });
                            for r in &attention {
                                let (c, a) = self.ticket_row(ui, r);
                                if c {
                                    clicked = Some(r.id);
                                }
                                action = action.or(a);
                            }
                            ui.add_space(10.0);
                        }
                        if !pinned.is_empty() {
                            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::hover());
                            let p = ui.painter();
                            p.text(
                                pos2(rect.left() + MARK_X, rect.center().y),
                                Align2::CENTER_CENTER,
                                icon::PUSH_PIN,
                                theme::body(12.0),
                                pal.faint,
                            );
                            p.text(
                                pos2(rect.left() + TEXT_X, rect.center().y),
                                Align2::LEFT_CENTER,
                                "置顶",
                                theme::strong(theme::SMALL),
                                pal.mute,
                            );
                            for r in &pinned {
                                let (c, a) = self.ticket_row(ui, r);
                                if c {
                                    clicked = Some(r.id);
                                }
                                action = action.or(a);
                            }
                            ui.add_space(10.0);
                        }
                        for g in &all {
                            let open = searching || !self.collapsed_projects.contains(&g.key);
                            if self.group_header(ui, g, &display_name(g, &all), open)
                                && !self.collapsed_projects.remove(&g.key)
                            {
                                self.collapsed_projects.insert(g.key.clone());
                            }
                            if open {
                                for r in &g.rows {
                                    let (c, a) = self.ticket_row(ui, r);
                                    if c {
                                        clicked = Some(r.id);
                                    }
                                    action = action.or(a);
                                }
                            }
                            ui.add_space(10.0);
                        }
                        if archived > 0 {
                            let text = if self.show_archived {
                                format!("隐藏已归档（{archived}）")
                            } else {
                                format!("显示已归档（{archived}）")
                            };
                            if widgets::quiet_button(ui, icon::ARCHIVE, &text, &pal, false).clicked() {
                                self.show_archived = !self.show_archived;
                            }
                        }
                        if let Some(id) = clicked {
                            self.select(Some(id));
                        }
                        match action {
                            Some(RowAction::Rename(id, title)) => self.rename = Some((id, title)),
                            Some(RowAction::Meta(a)) => self.submit(*a),
                            None => {}
                        }
                    });
            });
        widgets::scroll_fades(ui, &out, pal.sidebar);
    }

    /// Data source state and the settings that apply to every conversation.
    fn sidebar_foot(&mut self, ui: &mut Ui) {
        let pal = self.pal.clone();
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let (color, text) = match (&self.source.error, &self.source.label) {
                (Some(_), _) => (pal.fail.fg, "未连接到工单数据库".to_string()),
                (None, Some(l)) if self.writable() => {
                    use sa_engine::engine::SchedulerState;
                    let state = self.host.as_ref().map(|host| host.scheduler_health().state);
                    let (color, label) = match state {
                        Some(SchedulerState::Running) => (pal.run.fg, "调度中"),
                        Some(SchedulerState::DatabaseBusy) => (pal.warn.fg, "等待数据库"),
                        Some(SchedulerState::Unresponsive) => (pal.fail.fg, "调度无响应"),
                        Some(SchedulerState::Failed) => (pal.fail.fg, "调度已停止"),
                        _ => (pal.warn.fg, "调度未运行"),
                    };
                    (color, l.replacen("只读连接", label, 1))
                }
                (None, Some(l)) => (pal.run.fg, l.clone()),
                _ => (pal.faint, "正在连接数据源…".to_string()),
            };
            let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
            ui.painter()
                .circle_filled(pos2(r.left() + 5.0 + 3.0, r.center().y), 3.0, color);
            let g = widgets::line_galley(
                ui,
                &text,
                theme::body(theme::TINY),
                pal.faint,
                ui.available_width() - 4.0,
            );
            ui.add(egui::Label::new(g).selectable(false))
                .on_hover_text(&text);
        });
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let size = 30.0;
            let button = |ui: &mut Ui, ic: &str, label: &'static str, tip: &str| {
                let r = widgets::icon_button(ui, ic, size, pal.mute, pal.hover, false).on_hover_text(tip);
                r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
                r.clicked()
            };
            if self.writable() {
                if button(ui, icon::SLIDERS_HORIZONTAL, "模型与执行端", "模型与执行端 · 新会话默认") {
                    self.open_models(crate::models::Scope::Defaults);
                }
                if button(ui, icon::PUZZLE_PIECE, "MCP 与技能", "MCP 与技能 · 新会话默认") {
                    self.open_extensions(crate::extensions::ExtScope::Defaults);
                }
                if button(ui, icon::PLUGS, "供应商与轮换", "自定义供应商与轮换") {
                    self.open_providers();
                }
            }
            if button(ui, icon::GAUGE, "Token 与缓存", "Token 与缓存") {
                self.open_efficiency();
            }
            if button(ui, icon::DEVICE_MOBILE, "通知与隐私", "通知与隐私") {
                self.open_notify();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let (ic, tip) = if self.dark {
                    (icon::SUN, "切换到浅色主题 · Ctrl Shift L")
                } else {
                    (icon::MOON, "切换到暗色主题 · Ctrl Shift L")
                };
                if button(ui, ic, "切换主题", tip) {
                    self.dark = !self.dark;
                }
                if self.writable() {
                    let (ic, tip) = match self.notification_mode() {
                        Mode::Off => (icon::BELL_SLASH, "桌面通知已关闭 · 点击恢复".into()),
                        mode => (icon::BELL, format!("桌面通知：{} · 点击关闭", mode.label())),
                    };
                    if button(ui, ic, "桌面通知", &tip) {
                        self.toggle_notifications();
                    }
                }
            });
        });
    }

    fn search_box(&mut self, ui: &mut Ui) {
        let pal = self.pal.clone();
        let mut q = self.search.clone();
        Frame::new()
            .fill(pal.hover)
            .corner_radius(CornerRadius::same(theme::CONTROL_RADIUS))
            .inner_margin(Margin {
                left: 10,
                right: 4,
                top: 4,
                bottom: 4,
            })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 7.0;
                    ui.label(
                        RichText::new(icon::MAGNIFYING_GLASS)
                            .font(theme::body(14.0))
                            .color(pal.mute),
                    );
                    let clear_w = if q.is_empty() { 0.0 } else { 26.0 };
                    let edit = egui::TextEdit::singleline(&mut q)
                        .hint_text(RichText::new("搜索对话或项目").color(pal.faint))
                        .frame(false)
                        .font(theme::body(theme::UI))
                        .desired_width(ui.available_width() - clear_w)
                        .margin(Margin::symmetric(0, 3))
                        .id_salt("sidebar-search");
                    let r = ui.add(edit);
                    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "搜索对话或项目"));
                    if std::mem::take(&mut self.focus_search) {
                        r.request_focus();
                    }
                    if !q.is_empty()
                        && widgets::icon_button(ui, icon::X, 22.0, pal.mute, pal.selected, false)
                            .on_hover_text("清除搜索")
                            .clicked()
                    {
                        q.clear();
                    }
                });
            });
        if q != self.search {
            self.set_search(q);
        }
    }

    /// Returns true when clicked.
    fn group_header(&self, ui: &mut Ui, g: &Group, name: &str, open: bool) -> bool {
        let pal = &self.pal;
        let w = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(vec2(w, 28.0), Sense::click());
        if ui.is_rect_visible(rect) {
            let p = ui.painter();
            let caret = if open {
                icon::CARET_DOWN
            } else {
                icon::CARET_RIGHT
            };
            let hovered = resp.hovered();
            p.text(
                pos2(rect.left() + MARK_X, rect.center().y),
                Align2::CENTER_CENTER,
                caret,
                theme::body(10.0),
                if hovered { pal.mute } else { pal.faint },
            );
            let mut right = rect.right() - 8.0;
            if hovered || !open {
                let count = g.rows.len().to_string();
                let cg = widgets::line_galley(ui, &count, theme::body(theme::TINY), pal.faint, 60.0);
                right -= cg.size().x;
                p.galley(pos2(right, rect.center().y - cg.size().y / 2.0), cg, pal.faint);
                right -= 8.0;
            }
            let color = if hovered { pal.ink } else { pal.mute };
            let ng = widgets::line_galley(
                ui,
                name,
                theme::strong(theme::SMALL),
                color,
                right - rect.left() - TEXT_X,
            );
            p.galley(
                pos2(rect.left() + TEXT_X, rect.center().y - ng.size().y / 2.0),
                ng,
                color,
            );
        }
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        let resp = if g.cwd.is_empty() {
            resp
        } else {
            resp.on_hover_text(&g.cwd)
        };
        resp.clicked()
    }

    fn ticket_row(&self, ui: &mut Ui, r: &TicketSummary) -> (bool, Option<RowAction>) {
        let pal = &self.pal;
        let selected = self.view.as_ref().is_some_and(|v| v.id == r.id);
        let w = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(vec2(w, ROW), Sense::click());
        let name = r.title.as_deref().filter(|title| !title.is_empty()).unwrap_or(&r.goal);
        let mut row_label = match attention_reason(r) {
            Some(reason) => format!("{name} · {reason}"),
            None => name.to_string(),
        };
        let unread = self.notification_gate.unread(r.id);
        if unread {
            row_label.push_str(" · 未读");
        }
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &row_label));
        if ui.is_rect_visible(rect) {
            let p = ui.painter();
            let radius = CornerRadius::same(theme::CONTROL_RADIUS);
            if selected {
                p.rect_filled(rect, radius, pal.selected);
            } else if resp.hovered() {
                p.rect_filled(rect, radius, pal.hover);
            }
            let y = rect.center().y;
            let mark = pos2(rect.left() + MARK_X, y + 0.5);
            if r.archived {
                p.text(mark, Align2::CENTER_CENTER, icon::ARCHIVE, theme::body(12.0), pal.faint);
            } else {
                state_mark(ui, mark, &r.state, pal);
            }
            let time = attention_reason(r).map(str::to_owned).unwrap_or_else(|| labels::relative(r.created));
            let rg = widgets::line_galley(ui, &time, theme::body(theme::TINY), pal.faint, 100.0);
            let rx = rect.right() - 8.0 - rg.size().x;
            p.galley(pos2(rx, y - rg.size().y / 2.0), rg, pal.faint);

            let name = r.title.as_deref().filter(|t| !t.is_empty()).unwrap_or(&r.goal);
            let title = labels::compact(if name.is_empty() { "会话" } else { name }, 120);
            let color = if selected || labels::is_active(&r.state) {
                pal.ink
            } else if r.archived {
                pal.mute
            } else {
                pal.ink.gamma_multiply(0.86)
            };
            let left = rect.left() + TEXT_X;
            let title_right = if unread {
                p.circle_filled(pos2(rx - 7.0, y), 2.5, pal.accent);
                rx - 15.0
            } else {
                rx - 10.0
            };
            let tg = widgets::line_galley(ui, &title, theme::body(theme::UI), color, title_right - left);
            p.galley(pos2(left, y - tg.size().y / 2.0), tg, color);
        }
        let resp = resp
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_ui(|ui| {
                ui.label(RichText::new(labels::compact(&r.goal, 400)).font(theme::body(theme::UI)));
                ui.label(
                    RichText::new(format!(
                        "{} · 主控 {} · #{}",
                        labels::job_state(&r.state),
                        labels::agent_name(&r.lead),
                        r.id
                    ))
                    .font(theme::body(theme::TINY))
                    .color(self.pal.mute),
                );
                if !r.cwd.is_empty() {
                    ui.label(
                        RichText::new(&r.cwd)
                            .font(theme::body(theme::TINY))
                            .color(self.pal.faint),
                    );
                }
            });
        let mut action = None;
        if self.writable() {
            resp.context_menu(|ui| {
                ui.set_min_width(170.0);
                if ui.button(format!("{}  重命名", icon::PENCIL_SIMPLE)).clicked() {
                    let current = r.title.clone().unwrap_or_else(|| labels::compact(&r.goal, 60));
                    action = Some(RowAction::Rename(r.id, current));
                }
                let pin = if r.pinned { "取消置顶" } else { "置顶" };
                if ui.button(format!("{}  {pin}", icon::PUSH_PIN)).clicked() {
                    action = Some(RowAction::Meta(Box::new(Action::TicketMeta {
                        ticket: r.id,
                        title: None,
                        pinned: Some(!r.pinned),
                        archived: None,
                    })));
                }
                let arch = if r.archived { "取消归档" } else { "归档" };
                if ui.button(format!("{}  {arch}", icon::ARCHIVE)).clicked() {
                    action = Some(RowAction::Meta(Box::new(Action::TicketMeta {
                        ticket: r.id,
                        title: None,
                        pinned: None,
                        archived: Some(!r.archived),
                    })));
                }
            });
        }
        (resp.clicked(), action)
    }

    /// Rename dialog opened from a sidebar row's context menu.
    pub fn rename_dialog(&mut self, ctx: &egui::Context) {
        let Some((ticket, title)) = self.rename.as_mut() else { return };
        let pal = self.pal.clone();
        let mut save = false;
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("sa-rename"))
            .frame(widgets::modal_frame(&pal))
            .show(ctx, |ui| {
                ui.set_width(420.0);
                if widgets::modal_header(ui, icon::PENCIL_SIMPLE, "重命名会话", &format!("工单 #{ticket}；留空恢复为目标原文。"), &pal) {
                    close = true;
                }
                let r = ui.add(
                    egui::TextEdit::singleline(title).margin(egui::Margin::symmetric(8, 5))
                        .id_salt("rename-title")
                        .desired_width(f32::INFINITY)
                        .char_limit(200),
                );
                r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "会话名称"));
                if ui.memory(|m| m.focused().is_none()) {
                    r.request_focus();
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    save = true;
                }
                ui.add_space(16.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::primary_button(ui, "", "保存", &pal, true).clicked() {
                        save = true;
                    }
                    if widgets::quiet_button(ui, "", "取消", &pal, false).clicked() {
                        close = true;
                    }
                });
            });
        if modal.should_close() && !save {
            close = true;
        }
        if save {
            if let Some((ticket, title)) = self.rename.take() {
                self.submit(Action::TicketMeta {
                    ticket,
                    title: Some(title),
                    pinned: None,
                    archived: None,
                });
            }
        } else if close {
            self.rename = None;
        }
    }
}

/// What a sidebar row's context menu asked for.
enum RowAction {
    Rename(i64, String),
    Meta(Box<Action>),
}
