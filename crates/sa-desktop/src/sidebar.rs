//! Left sidebar: brand, search and tickets grouped by project folder.

use crate::app::App;
use crate::labels;
use crate::theme;
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

impl App {
    pub fn sidebar(&mut self, ui: &mut Ui) {
        let pal = self.pal.clone();
        egui::TopBottomPanel::bottom("sidebar-foot")
            .frame(
                Frame::new()
                    .inner_margin(Margin::symmetric(18, 12))
                    .stroke(Stroke::new(1.0_f32, pal.line_soft)),
            )
            .show_separator_line(false)
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    let (color, text) = match (&self.source.error, &self.source.label) {
                        (Some(_), _) => (pal.fail.fg, "未连接到工单数据库".to_string()),
                        (None, Some(l)) if self.writable() => (
                            pal.run.fg,
                            l.replacen("只读连接", "调度中", 1),
                        ),
                        (None, Some(l)) => (pal.run.fg, l.clone()),
                        _ => (pal.faint, "正在连接数据源…".to_string()),
                    };
                    let (r, _) = ui.allocate_exact_size(vec2(8.0, 16.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 3.5, color);
                    let g = widgets::line_galley(
                        ui,
                        &text,
                        theme::body(theme::TINY),
                        pal.mute,
                        ui.available_width(),
                    );
                    ui.add(egui::Label::new(g).selectable(false))
                        .on_hover_text(&text);
                });
            });

        Frame::new()
            .inner_margin(Margin {
                left: 14,
                right: 14,
                top: 18,
                bottom: 0,
            })
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(
                            RichText::new("Super Agent")
                                .font(theme::strong(16.5))
                                .color(pal.ink),
                        );
                        ui.label(
                            RichText::new("项目工作台")
                                .font(theme::body(theme::SMALL))
                                .color(pal.mute),
                        );
                    });
                });
                ui.add_space(16.0);
                if self.writable() {
                    if widgets::wide_button(ui, icon::NOTE_PENCIL, "新建对话", "Ctrl N", &pal)
                        .on_hover_text("开始一个新会话")
                        .clicked()
                    {
                        self.new_conversation();
                    }
                    ui.add_space(10.0);
                }
                self.search_box(ui);
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("最近对话")
                            .font(theme::body(theme::TINY + 0.5))
                            .color(pal.mute),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(4.0);
                        if !self.history.is_empty() {
                            ui.label(
                                RichText::new(self.history.len().to_string())
                                    .font(theme::body(theme::TINY))
                                    .color(pal.faint),
                            );
                        }
                    });
                });
                ui.add_space(6.0);
            });

        egui::ScrollArea::vertical()
            .id_salt("history")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                Frame::new()
                    .inner_margin(Margin {
                        left: 10,
                        right: 10,
                        top: 0,
                        bottom: 12,
                    })
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        if !self.history_loaded {
                            ui.add_space(20.0);
                            ui.horizontal(|ui| {
                                ui.add_space(8.0);
                                ui.spinner();
                                ui.label(RichText::new("正在读取会话…").color(pal.mute));
                            });
                            return;
                        }
                        if self.history.is_empty() {
                            ui.add_space(24.0);
                            ui.vertical_centered(|ui| {
                                let text = if self.source.error.is_some() {
                                    "暂时无法读取会话"
                                } else if self.search.is_empty() {
                                    "还没有会话。"
                                } else {
                                    "没找到匹配的会话。"
                                };
                                ui.label(RichText::new(text).color(pal.mute));
                            });
                            return;
                        }
                        let all = groups(&self.history);
                        let searching = !self.search.trim().is_empty();
                        let mut clicked = None;
                        for g in &all {
                            let open = searching || !self.collapsed_projects.contains(&g.key);
                            if self.group_header(ui, g, &display_name(g, &all), open)
                                && !self.collapsed_projects.remove(&g.key)
                            {
                                self.collapsed_projects.insert(g.key.clone());
                            }
                            if open {
                                for r in &g.rows {
                                    if self.ticket_row(ui, r) {
                                        clicked = Some(r.id);
                                    }
                                }
                                ui.add_space(6.0);
                            }
                        }
                        if let Some(id) = clicked {
                            self.select(Some(id));
                        }
                    });
            });
    }

    fn search_box(&mut self, ui: &mut Ui) {
        let pal = self.pal.clone();
        let mut q = self.search.clone();
        Frame::new()
            .fill(pal.surface)
            .stroke(Stroke::new(1.0_f32, pal.line))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin {
                left: 10,
                right: 6,
                top: 5,
                bottom: 5,
            })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(
                        RichText::new(icon::MAGNIFYING_GLASS)
                            .font(theme::body(15.0))
                            .color(pal.mute),
                    );
                    let clear_w = if q.is_empty() { 0.0 } else { 24.0 };
                    let edit = egui::TextEdit::singleline(&mut q)
                        .hint_text(RichText::new("搜索对话或项目").color(pal.faint))
                        .frame(false)
                        .font(theme::body(13.5))
                        .desired_width(ui.available_width() - clear_w)
                        .margin(Margin::symmetric(0, 3))
                        .id_salt("sidebar-search");
                    let r = ui.add(edit);
                    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "搜索对话或项目"));
                    if std::mem::take(&mut self.focus_search) {
                        r.request_focus();
                    }
                    if !q.is_empty()
                        && widgets::icon_button(ui, icon::X, 22.0, pal.mute, pal.hover, false)
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
        let (rect, resp) = ui.allocate_exact_size(vec2(w, 30.0), Sense::click());
        if ui.is_rect_visible(rect) {
            let p = ui.painter();
            if resp.hovered() {
                p.rect_filled(rect, CornerRadius::same(8), pal.hover);
            }
            let caret = if open {
                icon::CARET_DOWN
            } else {
                icon::CARET_RIGHT
            };
            p.text(
                pos2(rect.left() + 12.0, rect.center().y),
                Align2::CENTER_CENTER,
                caret,
                theme::body(11.0),
                pal.faint,
            );
            p.text(
                pos2(rect.left() + 30.0, rect.center().y),
                Align2::CENTER_CENTER,
                icon::FOLDER_SIMPLE,
                theme::body(15.0),
                pal.mute,
            );
            let count = g.rows.len().to_string();
            let cg = widgets::line_galley(ui, &count, theme::body(theme::TINY), pal.faint, 60.0);
            let cx = rect.right() - 10.0 - cg.size().x;
            p.galley(pos2(cx, rect.center().y - cg.size().y / 2.0), cg, pal.faint);
            let ng = widgets::line_galley(
                ui,
                name,
                theme::strong(13.5),
                pal.ink,
                cx - rect.left() - 52.0,
            );
            p.galley(
                pos2(rect.left() + 42.0, rect.center().y - ng.size().y / 2.0),
                ng,
                pal.ink,
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

    fn ticket_row(&self, ui: &mut Ui, r: &TicketSummary) -> bool {
        let pal = &self.pal;
        let selected = self.view.as_ref().is_some_and(|v| v.id == r.id);
        let w = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(vec2(w, 54.0), Sense::click());
        if ui.is_rect_visible(rect) {
            let p = ui.painter();
            if selected {
                p.rect_filled(rect, CornerRadius::same(9), pal.selected);
                p.rect_filled(
                    egui::Rect::from_min_size(
                        pos2(rect.left() + 3.0, rect.top() + 14.0),
                        vec2(3.0, rect.height() - 28.0),
                    ),
                    CornerRadius::same(2),
                    pal.accent,
                );
            } else if resp.hovered() {
                p.rect_filled(rect, CornerRadius::same(9), pal.hover);
            }
            let left = rect.left() + 18.0;
            let inner_w = rect.right() - 10.0 - left;
            let title = labels::compact(if r.goal.is_empty() { "会话" } else { &r.goal }, 120);
            let font = if selected {
                theme::strong(13.5)
            } else {
                theme::body(13.5)
            };
            let tg = widgets::line_galley(ui, &title, font, pal.ink, inner_w);
            p.galley(pos2(left, rect.top() + 8.0), tg, pal.ink);

            let tone = pal.state_tone(&r.state);
            let st = labels::job_state(&r.state);
            let sg = widgets::line_galley(ui, &st, theme::body(theme::TINY), tone.fg, 120.0);
            let chip = egui::Rect::from_min_size(
                pos2(left, rect.top() + 30.0),
                sg.size() + vec2(12.0, 3.0),
            );
            p.rect_filled(chip, CornerRadius::same(5), tone.bg);
            p.galley(chip.min + vec2(6.0, 1.5), sg, tone.fg);
            let lead = labels::agent_name(&r.lead);
            let mg = widgets::line_galley(ui, &lead, theme::body(theme::TINY), pal.faint, 120.0);
            let time = labels::relative(r.created);
            let rg = widgets::line_galley(ui, &time, theme::body(theme::TINY), pal.faint, 100.0);
            let rx = rect.right() - 10.0 - rg.size().x;
            let y = chip.center().y;
            if chip.right() + 8.0 + mg.size().x < rx - 8.0 {
                p.galley(
                    pos2(chip.right() + 8.0, y - mg.size().y / 2.0),
                    mg,
                    pal.faint,
                );
            }
            p.galley(pos2(rx, y - rg.size().y / 2.0), rg, pal.faint);
        }
        let resp = resp
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_ui(|ui| {
                ui.label(RichText::new(labels::compact(&r.goal, 400)));
                ui.label(
                    RichText::new(format!("#{} · {}", r.id, r.cwd))
                        .font(theme::body(theme::TINY))
                        .color(self.pal.mute),
                );
            });
        resp.clicked()
    }
}
