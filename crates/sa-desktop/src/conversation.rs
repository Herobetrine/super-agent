//! Central column: banners, autonomy strip, live activity, the feed and the
//! read-only footer.

use crate::app::App;
use crate::feed::{self, Control, ControlKind, Item, LineKind, ProgressRow, Tone};
use crate::labels::{self, activity_state, activity_time, agent_name, stamp};
use crate::markdown::{self, Look};
use crate::theme::{self, Palette};
use crate::widgets;
use egui::{
    Align, Align2, CornerRadius, Frame, Label, Layout, Margin, Rect, RichText, Sense, Stroke, Ui,
    UiBuilder, pos2, vec2,
};
use egui_phosphor::regular as icon;
use sa_store::Snapshot;

const COLUMN: f32 = 860.0;
/// Width of the avatar gutter; reply bodies start after it.
const GUTTER: f32 = 34.0;

/// Run `add` inside a horizontally centered column of at most `max` width.
fn centered<R>(ui: &mut Ui, max: f32, side: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    let full = ui.available_rect_before_wrap();
    let w = (full.width() - side * 2.0).min(max).max(120.0);
    let x = full.left() + ((full.width() - w) / 2.0).max(0.0);
    let rect = Rect::from_min_size(pos2(x, full.top()), vec2(w, full.height()));
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::top_down(Align::Min)),
        add,
    )
    .inner
}

fn control_icon(kind: ControlKind) -> &'static str {
    match kind {
        ControlKind::Progress => icon::FLAG,
        ControlKind::Send => icon::PAPER_PLANE_TILT,
        ControlKind::Note => icon::CHAT_TEXT,
        ControlKind::Role => icon::NOTE_PENCIL,
        ControlKind::Tool => icon::WRENCH,
        ControlKind::Goal => icon::TARGET,
        ControlKind::Next => icon::ARROW_RIGHT,
        ControlKind::Finish => icon::FLAG_CHECKERED,
        ControlKind::Blocked => icon::WARNING,
        ControlKind::Learn => icon::LIGHTBULB,
        ControlKind::Exhausted => icon::LIST_CHECKS,
    }
}

/// Stage text of the autonomy strip (studio.js `renderHeader`).
fn auto_stage(snap: &Snapshot) -> String {
    let status = snap.ticket.state.as_str();
    let run = snap.run.as_ref();
    if status == "cancelled" {
        "已停止 · 需要明确恢复".into()
    } else if run.is_some_and(|r| r.phase == "exhausted") {
        "已提交穷尽说明".into()
    } else if run.is_some_and(|r| r.phase == "discover") {
        "正在搜寻下一步".into()
    } else if labels::is_active(status) {
        match run {
            Some(r) => format!("执行阶段 {}", r.milestone + 1),
            None => "执行目标".into(),
        }
    } else {
        labels::job_state(status)
    }
}

impl App {
    pub fn conversation(&mut self, ui: &mut Ui) {
        let pal = self.pal.clone();
        egui::TopBottomPanel::bottom("footer")
            .frame(Frame::new().fill(pal.surface).inner_margin(Margin {
                left: 24,
                right: 24,
                top: 0,
                bottom: 14,
            }))
            .show_separator_line(false)
            .show_inside(ui, |ui| {
                centered(ui, COLUMN, 0.0, |ui| {
                    if self.writable() {
                        self.composer(ui);
                    } else {
                        self.footer(ui);
                    }
                });
            });

        ui.add_space(14.0);
        if let Some(err) = self.source.error.clone() {
            centered(ui, COLUMN, 24.0, |ui| self.error_banner(ui, &err));
            ui.add_space(10.0);
        }
        let Some(view) = self.view.as_ref() else {
            if self.writable() && self.source.error.is_none() {
                self.welcome(ui);
            } else {
                self.empty_state(ui);
            }
            return;
        };
        if let Some(err) = view.error.clone() {
            centered(ui, COLUMN, 24.0, |ui| {
                widgets::callout(pal.fail).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new(format!("{}  无法读取这个工单", icon::WARNING_CIRCLE))
                            .font(theme::strong(13.5))
                            .color(pal.fail.fg),
                    );
                    ui.label(
                        RichText::new(err)
                            .font(theme::body(theme::SMALL))
                            .color(pal.ink),
                    );
                });
            });
            ui.add_space(10.0);
        }
        let Some(snap) = view.snap.clone() else {
            if view.error.is_none() {
                ui.add_space(80.0);
                ui.vertical_centered(|ui| {
                    ui.spinner();
                    ui.add_space(8.0);
                    ui.label(RichText::new("正在读取工单…").color(pal.mute));
                });
            }
            return;
        };

        let auto = snap.ticket.permission_profile == "auto" || snap.run.is_some();
        if auto {
            centered(ui, COLUMN, 24.0, |ui| self.autonomy_strip(ui, &snap));
            ui.add_space(8.0);
        }
        if labels::is_active(&snap.ticket.state) {
            let root = snap.jobs.iter().find(|j| j.parent.is_none()).map(|j| j.id);
            let badge = root.and_then(|r| self.live_badges().remove(&r));
            centered(ui, COLUMN, 24.0, |ui| self.live_line(ui, &snap, badge));
            ui.add_space(4.0);
        }
        self.feed_area(ui, &snap);
    }

    fn error_banner(&self, ui: &mut Ui, err: &str) {
        let pal = &self.pal;
        widgets::callout(pal.fail).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(icon::WARNING_CIRCLE)
                        .font(theme::body(17.0))
                        .color(pal.fail.fg),
                );
                ui.label(
                    RichText::new("无法读取工单数据库")
                        .font(theme::strong(14.0))
                        .color(pal.fail.fg),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new("每 3 秒自动重试")
                            .font(theme::body(theme::TINY))
                            .color(pal.fail.fg),
                    );
                });
            });
            ui.add_space(2.0);
            ui.label(
                RichText::new(err)
                    .font(theme::body(theme::SMALL))
                    .color(pal.ink),
            );
            ui.label(
                RichText::new("可用 --data <目录> 指定数据目录，或用 --demo 查看演示数据。")
                    .font(theme::body(theme::TINY))
                    .color(pal.mute),
            );
        });
    }

    /// New-conversation home: greeting, starter prompts and recent projects.
    fn welcome(&mut self, ui: &mut Ui) {
        let pal = self.pal.clone();
        let h = ui.available_height();
        ui.add_space((h * 0.14).clamp(16.0, 120.0));
        let mut pick_draft: Option<String> = None;
        let mut pick_project: Option<std::path::PathBuf> = None;
        centered(ui, 760.0, 24.0, |ui| {
            ui.vertical_centered(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(52.0, 52.0), Sense::hover());
                widgets::brand_mark(ui, r, &pal);
                ui.add_space(16.0);
                ui.label(RichText::new(greeting()).font(theme::strong(26.0)).color(pal.ink));
                ui.add_space(6.0);
                let project = self
                    .composer
                    .project
                    .as_ref()
                    .map(|p| format!("项目 · {}", labels::basename(&p.to_string_lossy())))
                    .unwrap_or_else(|| "未选择项目时使用新的临时目录".into());
                ui.label(
                    RichText::new(format!("{project}　·　主控 {}", labels::agent_name(&self.composer.lead)))
                        .font(theme::body(13.5))
                        .color(pal.mute),
                );
            });
            ui.add_space(26.0);
            let gap = 12.0;
            let w = ((ui.available_width() - gap) / 2.0).floor();
            for row in STARTERS.chunks(2) {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for (ic, title, text, prompt) in row {
                        if starter_card(ui, w, ic, title, text, &pal).clicked() {
                            pick_draft = Some(prompt.to_string());
                        }
                    }
                });
                ui.add_space(gap);
            }
            let mut recent: Vec<&str> = Vec::new();
            for t in &self.history {
                if !t.cwd.is_empty() && !t.cwd.contains("super-agent-project-") && !recent.contains(&t.cwd.as_str()) {
                    recent.push(&t.cwd);
                }
                if recent.len() == 5 {
                    break;
                }
            }
            if !recent.is_empty() {
                ui.add_space(10.0);
                ui.label(RichText::new("最近的项目").font(theme::body(theme::SMALL)).color(pal.mute));
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                    for cwd in recent {
                        let active = self.composer.project.as_ref().is_some_and(|p| p.to_string_lossy() == cwd);
                        if widgets::quiet_button(ui, icon::FOLDER_SIMPLE, &labels::basename(cwd), &pal, active)
                            .on_hover_text(cwd)
                            .clicked()
                        {
                            pick_project = Some(std::path::PathBuf::from(cwd));
                        }
                    }
                });
            }
        });
        if let Some(d) = pick_draft {
            self.composer.draft = d;
            self.composer.focus = true;
        }
        if let Some(p) = pick_project
            && p.is_dir()
        {
            if self.composer.project.as_ref() != Some(&p) {
                self.composer.attachments.clear();
            }
            self.composer.project = Some(p);
        }
    }

    fn empty_state(&self, ui: &mut Ui) {
        let pal = &self.pal;
        let h = ui.available_height();
        ui.add_space((h * 0.28).max(30.0));
        ui.vertical_centered(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(56.0, 56.0), Sense::hover());
            widgets::brand_mark(ui, r, pal);
            ui.add_space(18.0);
            let (title, hint) = if self.source.error.is_some() {
                ("等待工单数据库", "连接恢复后会自动载入工单，无需重启。")
            } else if self.writable() && self.view.is_none() {
                ("开始新的对话", "选择项目和主控，在下方描述目标后发送。")
            } else if self.history_loaded && self.history.is_empty() {
                ("还没有会话", "在现有工作台中创建工单后，会自动出现在这里。")
            } else {
                (
                    "选择一个工单开始观察",
                    "左侧列出了现有工作台中的全部工单，执行进展会实时同步到这里。",
                )
            };
            ui.label(
                RichText::new(title)
                    .font(theme::strong(20.0))
                    .color(pal.ink),
            );
            ui.add_space(6.0);
            ui.label(RichText::new(hint).font(theme::body(13.5)).color(pal.mute));
        });
    }

    fn autonomy_strip(&self, ui: &mut Ui, snap: &Snapshot) {
        let pal = &self.pal;
        let resp = Frame::new()
            .fill(pal.surface_alt)
            .stroke(Stroke::new(1.0_f32, pal.line_soft))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin {
                left: 16,
                right: 14,
                top: 10,
                bottom: 10,
            })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(
                        RichText::new(icon::LIGHTNING)
                            .font(theme::body(15.0))
                            .color(pal.accent),
                    );
                    ui.label(
                        RichText::new("自动运行")
                            .font(theme::strong(13.5))
                            .color(pal.ink),
                    );
                    ui.label(RichText::new("/").font(theme::body(13.0)).color(pal.faint));
                    ui.label(
                        RichText::new(auto_stage(snap))
                            .font(theme::body(13.0))
                            .color(pal.ink),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if let Some(run) = &snap.run {
                            if run.milestone > 0 {
                                widgets::chip(
                                    ui,
                                    &format!("已完成里程碑 {}", run.milestone),
                                    pal.ok,
                                    theme::TINY,
                                );
                            }
                            let phase = match run.phase.as_str() {
                                "work" => "执行中",
                                "discover" => "探查中",
                                "exhausted" => "已穷尽",
                                other => other,
                            };
                            widgets::chip(ui, &format!("阶段 · {phase}"), pal.neutral, theme::TINY);
                        }
                    });
                });
                let goal = snap
                    .ticket
                    .objective
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or(snap.ticket.goal.clone());
                let w = ui.available_width();
                let g = widgets::line_galley(
                    ui,
                    &format!("当前目标 · {}", labels::compact(&goal, 400)),
                    theme::body(13.0),
                    pal.ink,
                    w,
                );
                ui.add(Label::new(g).selectable(false)).on_hover_text(&goal);
                let learning = snap
                    .run
                    .as_ref()
                    .map(|r| r.learning.clone())
                    .filter(|s| !s.trim().is_empty());
                let text = format!(
                    "最近经验 · {}",
                    learning
                        .clone()
                        .map(|l| labels::compact(&l, 400))
                        .unwrap_or("暂无记录".into())
                );
                let g = widgets::line_galley(ui, &text, theme::body(12.5), pal.mute, w);
                let r = ui.add(Label::new(g).selectable(false));
                if let Some(l) = learning {
                    r.on_hover_text(l);
                }
            });
        widgets::left_bar(ui, resp.response.rect, pal.accent);
    }

    fn live_line(&self, ui: &mut Ui, snap: &Snapshot, badge: Option<(String, theme::Tone)>) {
        let pal = &self.pal;
        let view = self.view.as_ref();
        let root = snap.jobs.iter().find(|j| j.parent.is_none());
        let mut running: Vec<(String, usize)> = Vec::new();
        for j in &snap.jobs {
            if j.state != "running" || Some(j.id) == root.map(|r| r.id) {
                continue;
            }
            let name = agent_name(&j.agent);
            match running.iter_mut().find(|(n, _)| *n == name) {
                Some((_, c)) => *c += 1,
                None => running.push((name, 1)),
            }
        }
        let status = snap.ticket.state.as_str();
        let mut head = format!(
            "主控 {} · {}",
            agent_name(&snap.ticket.lead),
            labels::job_state(status)
        );
        if !running.is_empty() {
            let peers: Vec<String> = running
                .iter()
                .map(|(n, c)| {
                    if *c > 1 {
                        format!("{n} ×{c}")
                    } else {
                        n.clone()
                    }
                })
                .collect();
            let partial = snap.total_jobs > snap.jobs.len() as i64;
            head.push_str(if partial {
                "　/　概览中执行的协作者 · "
            } else {
                "　/　执行中的协作者 · "
            });
            head.push_str(&peers.join("、"));
        }
        // Prefer the native activity when it is at least as recent as the journal one.
        let feed_latest = view.and_then(|v| v.feed.latest.clone());
        let native = snap.native_activity.latest.as_ref();
        let (label, ts, job, turn, agent) = match (native, &feed_latest) {
            (Some(n), Some(f)) if n.ts < f.ts => {
                (f.label.clone(), f.ts, f.job, f.turn, f.agent.clone())
            }
            (Some(n), _) => (n.label.clone(), n.ts, Some(n.job), Some(n.turn), None),
            (None, Some(f)) => (f.label.clone(), f.ts, f.job, f.turn, f.agent.clone()),
            (None, None) => (String::new(), 0.0, None, None, None),
        };
        let detail = if label.is_empty() {
            if status == "queued" {
                "等待调度".to_string()
            } else {
                "等待首个执行事件".to_string()
            }
        } else {
            let who = agent
                .or_else(|| {
                    job.and_then(|j| snap.jobs.iter().find(|x| x.id == j))
                        .map(|x| x.agent.clone())
                })
                .map(|a| agent_name(&a))
                .unwrap_or_default();
            let source = [who, job.map(|j| format!("#{j}")).unwrap_or_default()]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            [
                "最近活动".to_string(),
                source,
                turn.map(|t| format!("第 {t} 轮")).unwrap_or_default(),
                labels::compact(&label, 160),
            ]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · ")
        };
        let resp = Frame::new()
            .fill(pal.pale)
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin {
                left: 16,
                right: 14,
                top: 8,
                bottom: 8,
            })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 3.0;
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(12.0, 16.0), Sense::hover());
                    ui.painter()
                        .circle_filled(r.center(), 5.0, pal.run.fg.gamma_multiply(0.25));
                    ui.painter().circle_filled(r.center(), 3.0, pal.run.fg);
                    let right = format!(
                        "已运行 {}",
                        labels::duration(labels::now_secs() - snap.ticket.created)
                    );
                    let rg =
                        widgets::line_galley(ui, &right, theme::body(theme::TINY), pal.mute, 200.0);
                    let reserve = if badge.is_some() { 130.0 } else { 0.0 };
                    let w = ui.available_width() - rg.size().x - 12.0 - reserve;
                    let g = widgets::line_galley(ui, &head, theme::strong(12.5), pal.ink, w);
                    ui.add(Label::new(g).selectable(false)).on_hover_text(&head);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add(Label::new(rg).selectable(false))
                            .on_hover_text(format!(
                                "创建于 {}",
                                activity_time(snap.ticket.created)
                            ));
                        if let Some((text, tone)) = &badge {
                            widgets::chip(ui, text, *tone, theme::TINY).on_hover_text("主控的实时状态");
                        }
                    });
                });
                ui.horizontal(|ui| {
                    ui.add_space(20.0);
                    let age = if ts > 0.0 {
                        labels::age(ts)
                    } else {
                        String::new()
                    };
                    let ag =
                        widgets::line_galley(ui, &age, theme::body(theme::TINY), pal.mute, 200.0);
                    let w = ui.available_width() - ag.size().x - 12.0;
                    let g = widgets::line_galley(ui, &detail, theme::body(12.5), pal.mute, w);
                    ui.add(Label::new(g).selectable(false))
                        .on_hover_text(&detail);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add(Label::new(ag).selectable(false))
                            .on_hover_text(activity_time(ts));
                    });
                });
            });
        widgets::left_bar(ui, resp.response.rect, pal.run.fg);
    }

    fn feed_area(&mut self, ui: &mut Ui, snap: &Snapshot) {
        let pal = self.pal.clone();
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let ticket = view.id;
        let at_top = self.opts.at_top;
        let jump = std::mem::take(&mut self.scroll_to_bottom) && !at_top;
        let items = view.feed.items.clone();
        let partial = view.partial_history;
        let mut open_groups = std::mem::take(&mut view.open_groups);
        let pending: Vec<i64> = snap.approvals.iter().map(|a| a.id).collect();
        let writable = self.writable();
        let area = ui.available_rect_before_wrap();
        let out = egui::ScrollArea::vertical()
            .id_salt(("feed", ticket))
            .auto_shrink([false, false])
            .stick_to_bottom(!at_top)
            .show(ui, |ui| {
                ui.add_space(14.0);
                centered(ui, COLUMN, 28.0, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    if partial {
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new("仅显示最近的记录 · 更早的内容保留在工单日志中")
                                    .font(theme::body(theme::TINY))
                                    .color(pal.faint),
                            );
                        });
                        ui.add_space(14.0);
                    }
                    let mut prev: Option<&Item> = None;
                    for item in &items {
                        ui.add_space(gap(prev, item));
                        show_item(ui, item, snap, &pal, &mut open_groups, &pending, writable);
                        prev = Some(item);
                    }
                    ui.add_space(28.0);
                });
                if jump {
                    ui.scroll_to_cursor(Some(Align::BOTTOM));
                }
            });
        if let Some(v) = self.view.as_mut() {
            v.open_groups = open_groups;
        }
        let max = (out.content_size.y - out.inner_rect.height()).max(0.0);
        let at_bottom = out.state.offset.y >= max - 48.0;
        if !at_bottom && !jump {
            let size = vec2(112.0, 34.0);
            let rect =
                Rect::from_center_size(pos2(area.center().x, out.inner_rect.bottom() - 30.0), size);
            let resp = ui.interact(rect, ui.id().with("jump-latest"), Sense::click());
            let p = ui.painter();
            p.rect_filled(
                rect.translate(vec2(0.0, 2.0)).expand(1.5),
                CornerRadius::same(18),
                egui::Color32::from_black_alpha(18),
            );
            p.rect_filled(
                rect,
                CornerRadius::same(17),
                if resp.hovered() {
                    pal.hover
                } else {
                    pal.surface
                },
            );
            p.rect_stroke(
                rect,
                CornerRadius::same(17),
                Stroke::new(1.0_f32, pal.line),
                egui::StrokeKind::Inside,
            );
            p.text(
                rect.center(),
                Align2::CENTER_CENTER,
                format!("{}  跳到最新", icon::ARROW_DOWN),
                theme::body(12.5),
                pal.ink,
            );
            if resp
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                self.scroll_to_bottom = true;
            }
        }
    }

    fn footer(&self, ui: &mut Ui) {
        let pal = &self.pal;
        let snap = self.view.as_ref().and_then(|v| v.snap.as_ref());
        centered(ui, COLUMN, 0.0, |ui| {
            Frame::new()
                .fill(pal.surface_alt)
                .stroke(Stroke::new(1.0_f32, pal.line))
                .corner_radius(CornerRadius::same(14))
                .inner_margin(Margin { left: 16, right: 14, top: 10, bottom: 10 })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        ui.label(RichText::new(icon::LOCK_SIMPLE).font(theme::body(15.0)).color(pal.mute));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            // Project details only when there is room for them.
                            if let Some(s) = snap.filter(|_| ui.available_width() > 560.0) {
                                widgets::chip(ui, &labels::profile(&s.ticket.permission_profile), pal.neutral, theme::TINY)
                                    .on_hover_text("会话权限");
                                ui.label(
                                    RichText::new(format!("主控 · {}", agent_name(&s.ticket.lead)))
                                        .font(theme::body(theme::TINY + 0.5))
                                        .color(pal.mute),
                                );
                                let folder = format!("{} {}", icon::FOLDER_SIMPLE, labels::basename(&s.ticket.cwd));
                                ui.label(RichText::new(folder).font(theme::body(theme::TINY + 0.5)).color(pal.mute))
                                    .on_hover_text(&s.ticket.cwd);
                                ui.add_space(8.0);
                            }
                            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                let text = match &self.host_error {
                                    Some(e) if e.starts_with("只读") => e.clone(),
                                    Some(e) => format!("只读观察模式：{e}"),
                                    None => "只读观察模式：调度仍由现有工作台负责".to_string(),
                                };
                                let text = text.as_str();
                                let g = widgets::line_galley(ui, text, theme::body(13.0), pal.mute, ui.available_width());
                                ui.add(Label::new(g).selectable(false)).on_hover_text(
                                    "此窗口只读取工单日志：不创建工单、不发送消息、不处理审批，也不调用模型。",
                                );
                            });
                        });
                    });
                });
        });
    }
}

/// Vertical space between two feed items.
fn gap(prev: Option<&Item>, item: &Item) -> f32 {
    let Some(prev) = prev else { return 0.0 };
    let quiet = |i: &Item| matches!(i, Item::Line { .. } | Item::Progress { .. });
    match (quiet(prev), quiet(item)) {
        (true, true) => 4.0,
        _ if matches!(item, Item::User { .. }) => 26.0,
        _ => 18.0,
    }
}

fn show_item(
    ui: &mut Ui,
    item: &Item,
    snap: &Snapshot,
    pal: &Palette,
    open_groups: &mut std::collections::HashSet<i64>,
    pending: &[i64],
    writable: bool,
) {
    match item {
        Item::User {
            text, ts, follow, ..
        } => user_bubble(ui, text, *ts, *follow, pal),
        Item::Reply {
            key,
            agent,
            job,
            peer,
            role,
            blocks,
            text,
            controls,
            ts,
            streaming,
            latest_result,
        } => {
            let header = |ui: &mut Ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    widgets::avatar(ui, agent, 24.0, pal);
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new(agent_name(agent))
                            .font(theme::strong(14.0))
                            .color(pal.ink),
                    );
                    if *peer {
                        let r = if role.is_empty() {
                            "协作回报".to_string()
                        } else {
                            format!("协作回报 · {}", labels::compact(role, 30))
                        };
                        widgets::chip(ui, &r, pal.neutral, theme::TINY);
                    } else if *latest_result {
                        widgets::chip(ui, "最近回报", pal.ok, theme::TINY);
                    } else {
                        widgets::chip(ui, "主控", pal.ok, theme::TINY);
                    }
                    if let Some(j) = job {
                        ui.label(
                            RichText::new(format!("#{j}"))
                                .font(theme::body(theme::TINY))
                                .color(pal.faint),
                        );
                    }
                    if *ts > 0.0 {
                        ui.label(
                            RichText::new(stamp(*ts))
                                .font(theme::body(theme::TINY))
                                .color(pal.faint),
                        )
                        .on_hover_text(activity_time(*ts));
                    }
                    if *streaming {
                        let t = ui.input(|i| i.time);
                        let dots = ".".repeat(1 + (t * 2.0) as usize % 3);
                        ui.label(
                            RichText::new(format!("正在输出{dots}"))
                                .font(theme::body(theme::TINY))
                                .color(pal.run.fg),
                        );
                        ui.ctx()
                            .request_repaint_after(std::time::Duration::from_millis(500));
                    }
                    if !text.is_empty() && !*streaming {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let id = ui.id().with(("reply-copied", *key));
                            let now = ui.input(|i| i.time);
                            let recent = ui
                                .data(|d| d.get_temp::<f64>(id))
                                .is_some_and(|t| now - t < 1.5);
                            let (ic, tip) = if recent { (icon::CHECK, "已复制") } else { (icon::COPY, "复制回复") };
                            let r = widgets::icon_button(ui, ic, 24.0, pal.faint, pal.hover, false).on_hover_text(tip);
                            r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "复制回复"));
                            if r.clicked() {
                                ui.ctx().copy_text(text.clone());
                                ui.data_mut(|d| d.insert_temp(id, now));
                            }
                        });
                    }
                });
            };
            let body = |ui: &mut Ui| {
                Frame::new()
                    .inner_margin(Margin {
                        left: GUTTER as i8,
                        right: 0,
                        top: 6,
                        bottom: 0,
                    })
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let look = Look {
                            pal,
                            size: theme::BODY,
                            color: pal.ink,
                        };
                        markdown::show(ui, blocks, &look, *key as u64);
                        if !controls.is_empty() {
                            if !blocks.is_empty() {
                                ui.add_space(10.0);
                            }
                            controls_box(ui, controls, pal);
                        }
                    });
            };
            if *peer {
                Frame::new()
                    .fill(pal.surface_alt)
                    .stroke(Stroke::new(1.0_f32, pal.line_soft))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin {
                        left: 14,
                        right: 16,
                        top: 12,
                        bottom: 14,
                    })
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        header(ui);
                        body(ui);
                    });
            } else {
                header(ui);
                body(ui);
            }
        }
        Item::Line {
            kind,
            text,
            agent,
            to,
            ts,
            tone,
            ..
        } => {
            line(
                ui,
                kind,
                text,
                agent.as_deref(),
                to.as_deref(),
                *ts,
                *tone,
                pal,
            );
        }
        Item::Alert {
            kind,
            agent,
            job,
            text,
            ts,
            ..
        } => {
            let tone = if kind == "failed" { pal.fail } else { pal.stop };
            let ic = match kind.as_str() {
                "failed" => icon::X_CIRCLE,
                "cancelled" => icon::PAUSE_CIRCLE,
                "draining" => icon::HOURGLASS,
                _ => icon::WARNING_CIRCLE,
            };
            widgets::callout(tone).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(RichText::new(ic).font(theme::body(16.0)).color(tone.fg));
                    ui.label(
                        RichText::new(feed::alert_title(kind, agent))
                            .font(theme::strong(13.5))
                            .color(tone.fg),
                    );
                    if let Some(j) = job {
                        ui.label(
                            RichText::new(format!("任务 #{j}"))
                                .font(theme::body(theme::TINY))
                                .color(tone.fg),
                        );
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(stamp(*ts))
                                .font(theme::body(theme::TINY))
                                .color(tone.fg),
                        )
                        .on_hover_text(activity_time(*ts));
                    });
                });
                if !text.trim().is_empty() {
                    ui.add_space(4.0);
                    let look = Look {
                        pal,
                        size: 13.0,
                        color: pal.ink,
                    };
                    ui.add(Label::new(markdown::plain_job(text.trim(), &look)).wrap());
                }
            });
        }
        Item::Approval {
            job,
            approval,
            request,
            ts,
            ..
        } => {
            let open = approval.is_some_and(|a| pending.contains(&a));
            let tone = if open { pal.warn } else { pal.neutral };
            widgets::callout(tone).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(
                        RichText::new(icon::SHIELD_WARNING)
                            .font(theme::body(16.0))
                            .color(tone.fg),
                    );
                    let title = if open { "待审批" } else { "审批请求" };
                    ui.label(
                        RichText::new(title)
                            .font(theme::strong(13.5))
                            .color(tone.fg),
                    );
                    let mut meta = Vec::new();
                    if let Some(j) = job {
                        meta.push(format!("任务 #{j}"));
                    }
                    if let Some(a) = approval {
                        meta.push(format!("请求 #{a}"));
                    }
                    ui.label(
                        RichText::new(meta.join(" · "))
                            .font(theme::body(theme::TINY))
                            .color(tone.fg),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(stamp(*ts))
                                .font(theme::body(theme::TINY))
                                .color(tone.fg),
                        );
                    });
                });
                ui.add_space(3.0);
                ui.label(
                    RichText::new(format!("请求使用 {request}"))
                        .font(theme::body(13.0))
                        .color(pal.ink),
                );
                ui.label(
                    RichText::new(if open {
                        if writable {
                            "请在项目面板的审批页允许或拒绝。"
                        } else {
                            "只读观察模式不会处理审批，请在持有调度的工作台中允许或拒绝。"
                        }
                    } else {
                        "该请求已处理或不再等待。"
                    })
                    .font(theme::body(theme::TINY + 0.5))
                    .color(pal.mute),
                );
            });
        }
        Item::Report { owner, text, .. } => {
            let resp = Frame::new()
                .fill(pal.report.bg)
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin {
                    left: 16,
                    right: 14,
                    top: 9,
                    bottom: 10,
                })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        ui.label(
                            RichText::new(format!("{}  阶段汇报", icon::FLAG))
                                .font(theme::strong(13.0))
                                .color(pal.report.fg),
                        );
                        ui.label(
                            RichText::new(owner)
                                .font(theme::body(theme::TINY))
                                .color(pal.mute),
                        );
                    });
                    ui.add_space(2.0);
                    let look = Look {
                        pal,
                        size: 13.5,
                        color: pal.ink,
                    };
                    ui.add(Label::new(markdown::plain_job(text, &look)).wrap());
                });
            widgets::left_bar(ui, resp.response.rect, pal.report.fg.gamma_multiply(0.7));
        }
        Item::Progress { key, rows } => progress_group(ui, *key, rows, snap, pal, open_groups),
    }
}

fn user_bubble(ui: &mut Ui, text: &str, ts: f64, follow: bool, pal: &Palette) {
    let col = ui.available_width();
    let max_w = (col * 0.8).min(640.0);
    let pad = vec2(16.0, 10.0);
    let look = Look {
        pal,
        size: theme::BODY,
        color: pal.ink,
    };
    let mut job = markdown::plain_job(text.trim(), &look);
    job.wrap.max_width = max_w - pad.x * 2.0;
    let galley = ui.painter().layout_job(job);
    let size = galley.size() + pad * 2.0;
    // `put` moves the cursor to the bottom of the label, so place the bubble
    // relative to the cursor and add the bottom padding afterwards.
    let top = ui.available_rect_before_wrap();
    let rect = Rect::from_min_size(pos2(top.left() + col - size.x, top.top()), size);
    ui.painter().rect_filled(
        rect,
        CornerRadius {
            nw: 16,
            ne: 16,
            sw: 16,
            se: 5,
        },
        pal.user_bubble,
    );
    ui.put(rect.shrink2(pad), Label::new(galley).selectable(true));
    ui.add_space(pad.y + 6.0);
    let meta = format!("{} · {}", if follow { "追加指令" } else { "你" }, stamp(ts));
    let g = widgets::line_galley(ui, &meta, theme::body(theme::TINY), pal.faint, col);
    let (r, resp) = ui.allocate_exact_size(vec2(col, g.size().y), Sense::hover());
    ui.painter()
        .galley(pos2(r.right() - g.size().x - 4.0, r.top()), g, pal.faint);
    resp.on_hover_text(activity_time(ts));
}

fn controls_box(ui: &mut Ui, controls: &[Control], pal: &Palette) {
    Frame::new()
        .fill(pal.surface_alt)
        .stroke(Stroke::new(1.0_f32, pal.line_soft))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin {
            left: 12,
            right: 12,
            top: 6,
            bottom: 6,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 2.0;
            for c in controls {
                let (rect, resp) =
                    ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
                let p = ui.painter();
                let y = rect.center().y;
                let color = match c.kind {
                    ControlKind::Blocked => pal.fail.fg,
                    _ => pal.accent,
                };
                p.text(
                    pos2(rect.left() + 8.0, y),
                    Align2::CENTER_CENTER,
                    control_icon(c.kind),
                    theme::body(14.0),
                    color,
                );
                let mut x = rect.left() + 24.0;
                if let Some(a) = &c.agent {
                    widgets::avatar_at(ui, pos2(x + 8.0, y), 8.0, a, pal);
                    x += 22.0;
                }
                let tg = widgets::line_galley(ui, &c.title, theme::strong(12.5), pal.ink, 200.0);
                let tw = tg.size().x;
                p.galley(pos2(x, y - tg.size().y / 2.0), tg, pal.ink);
                x += tw + 8.0;
                if !c.text.is_empty() {
                    let dot = widgets::line_galley(ui, "·", theme::body(12.5), pal.faint, 20.0);
                    p.galley(pos2(x, y - dot.size().y / 2.0), dot, pal.faint);
                    x += 12.0;
                    let g = widgets::line_galley(
                        ui,
                        &c.text,
                        theme::body(12.5),
                        pal.mute,
                        rect.right() - x,
                    );
                    p.galley(pos2(x, y - g.size().y / 2.0), g, pal.mute);
                }
                if !c.text.is_empty() {
                    resp.on_hover_ui(|ui| {
                        ui.label(RichText::new(&c.title).font(theme::strong(12.5)));
                        ui.label(&c.text);
                    });
                }
            }
        });
}

#[allow(clippy::too_many_arguments)]
fn line(
    ui: &mut Ui,
    kind: &LineKind,
    text: &str,
    agent: Option<&str>,
    to: Option<&str>,
    ts: f64,
    tone: Tone,
    pal: &Palette,
) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
    let p = ui.painter();
    let y = rect.center().y;
    let color = if tone == Tone::Error {
        pal.fail.fg
    } else {
        pal.mute
    };
    let ic = match kind {
        LineKind::Send => icon::PAPER_PLANE_TILT,
        LineKind::Note => icon::CHAT_TEXT,
        LineKind::Tool => icon::WRENCH,
        LineKind::Decision => icon::SHIELD_CHECK,
        LineKind::Auto => icon::SPARKLE,
        LineKind::Retry => icon::ARROWS_CLOCKWISE,
    };
    p.text(
        pos2(rect.left() + 12.0, y),
        Align2::CENTER_CENTER,
        ic,
        theme::body(14.0),
        color,
    );
    let time = stamp(ts);
    let tg = widgets::line_galley(ui, &time, theme::body(theme::TINY), pal.faint, 80.0);
    let right = rect.right() - tg.size().x;
    p.galley(pos2(right, y - tg.size().y / 2.0), tg, pal.faint);
    let mut x = rect.left() + GUTTER;
    if let (LineKind::Send, Some(from), Some(to)) = (kind, agent, to) {
        widgets::avatar_at(ui, pos2(x + 8.0, y), 8.0, from, pal);
        x += 20.0;
        let g = widgets::line_galley(ui, &agent_name(from), theme::strong(12.5), pal.ink, 160.0);
        let w = g.size().x;
        p.galley(pos2(x, y - g.size().y / 2.0), g, pal.ink);
        x += w + 6.0;
        p.text(
            pos2(x + 6.0, y),
            Align2::CENTER_CENTER,
            icon::ARROW_RIGHT,
            theme::body(12.0),
            pal.faint,
        );
        x += 18.0;
        widgets::avatar_at(ui, pos2(x + 8.0, y), 8.0, to, pal);
        x += 20.0;
        let g = widgets::line_galley(ui, &agent_name(to), theme::strong(12.5), pal.ink, 160.0);
        let w = g.size().x;
        p.galley(pos2(x, y - g.size().y / 2.0), g, pal.ink);
        x += w + 10.0;
    }
    let g = widgets::line_galley(ui, text, theme::body(12.5), color, right - x - 12.0);
    p.galley(pos2(x, y - g.size().y / 2.0), g, color);
    resp.on_hover_text(format!("{text}\n{}", activity_time(ts)));
}

fn progress_group(
    ui: &mut Ui,
    key: i64,
    rows: &[ProgressRow],
    snap: &Snapshot,
    pal: &Palette,
    open_groups: &mut std::collections::HashSet<i64>,
) {
    let open = open_groups.contains(&key);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(
            rect.expand2(vec2(4.0, 0.0)),
            CornerRadius::same(8),
            pal.hover,
        );
    }
    let y = rect.center().y;
    p.text(
        pos2(rect.left() + 12.0, y),
        Align2::CENTER_CENTER,
        if open {
            icon::CARET_DOWN
        } else {
            icon::CARET_RIGHT
        },
        theme::body(12.0),
        pal.mute,
    );
    let summary = feed::group_summary(rows);
    let last_state = rows.last().map(|r| r.state.as_str()).unwrap_or("");
    let failed = rows
        .iter()
        .any(|r| matches!(r.state.as_str(), "failed" | "error"));
    let x = rect.left() + GUTTER;
    let mut right = rect.right();
    if failed {
        let n = rows
            .iter()
            .filter(|r| matches!(r.state.as_str(), "failed" | "error"))
            .count();
        let fg = widgets::line_galley(
            ui,
            &format!("{n} 条失败"),
            theme::body(theme::TINY),
            pal.fail.fg,
            120.0,
        );
        let chip = Rect::from_min_size(
            pos2(right - fg.size().x - 12.0, y - fg.size().y / 2.0 - 1.5),
            fg.size() + vec2(12.0, 3.0),
        );
        p.rect_filled(chip, CornerRadius::same(5), pal.fail.bg);
        p.galley(chip.min + vec2(6.0, 1.5), fg, pal.fail.fg);
        right = chip.left();
    }
    let color = if last_state == "failed" {
        pal.fail.fg
    } else {
        pal.mute
    };
    let g = widgets::line_galley(ui, &summary, theme::body(12.5), color, right - x - 16.0);
    p.galley(pos2(x, y - g.size().y / 2.0), g, color);
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if resp.clicked() {
        if open {
            open_groups.remove(&key);
        } else {
            open_groups.insert(key);
        }
    }
    if !open {
        return;
    }
    ui.add_space(4.0);
    let resp = Frame::new()
        .inner_margin(Margin {
            left: GUTTER as i8 + 2,
            right: 0,
            top: 2,
            bottom: 4,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 2.0;
            for r in rows {
                progress_row(ui, r, snap, pal);
            }
        });
    let rr = resp.response.rect;
    ui.painter().rect_filled(
        Rect::from_min_size(pos2(rect.left() + 11.0, rr.top()), vec2(2.0, rr.height())),
        CornerRadius::same(1),
        pal.line,
    );
}

fn progress_row(ui: &mut Ui, r: &ProgressRow, snap: &Snapshot, pal: &Palette) {
    let agent = if r.agent.is_empty() {
        r.job
            .and_then(|j| snap.jobs.iter().find(|x| x.id == j))
            .map(|j| j.agent.clone())
            .unwrap_or_default()
    } else {
        r.agent.clone()
    };
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 24.0), Sense::hover());
    let p = ui.painter();
    let y = rect.center().y;
    let tg = widgets::line_galley(
        ui,
        &activity_time(r.ts),
        theme::mono(11.5),
        pal.faint,
        140.0,
    );
    p.galley(pos2(rect.left(), y - tg.size().y / 2.0), tg, pal.faint);
    let x = rect.left() + 70.0;
    widgets::avatar_at(ui, pos2(x + 8.0, y), 8.0, &agent, pal);
    let mut right = rect.right();
    if !r.state.is_empty() {
        let tone = pal.state_tone(&r.state);
        let sg = widgets::line_galley(
            ui,
            &activity_state(&r.state),
            theme::body(theme::TINY),
            tone.fg,
            100.0,
        );
        let chip = Rect::from_min_size(
            pos2(right - sg.size().x - 12.0, y - sg.size().y / 2.0 - 1.5),
            sg.size() + vec2(12.0, 3.0),
        );
        p.rect_filled(chip, CornerRadius::same(5), tone.bg);
        p.galley(chip.min + vec2(6.0, 1.5), sg, tone.fg);
        right = chip.left() - 8.0;
    }
    let lg = widgets::line_galley(ui, &r.label, theme::body(13.0), pal.ink, right - x - 24.0);
    p.galley(pos2(x + 24.0, y - lg.size().y / 2.0), lg, pal.ink);
    resp.on_hover_text(&r.label);

    let mut meta = vec![agent_name(&agent)];
    if let Some(j) = r.job {
        meta.push(format!("平台任务 #{j}"));
    }
    if let Some(t) = r.turn {
        meta.push(format!("第 {t} 轮"));
    }
    if !r.tool.is_empty() {
        meta.push(format!("工具 {}", labels::compact(&r.tool, 80)));
    }
    if !r.task_id.is_empty() {
        meta.push(format!("内部任务 {}", labels::compact(&r.task_id, 40)));
    }
    if !r.title.is_empty() {
        meta.push(labels::compact(&r.title, 80));
    }
    let steps: Vec<String> = r
        .steps
        .iter()
        .map(|(t, s)| {
            [t.clone(), activity_state(s)]
                .into_iter()
                .filter(|x| !x.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    if !steps.is_empty() {
        meta.push(format!("步骤 {}", steps.join("、")));
    }
    let (mrect, _) = ui.allocate_exact_size(vec2(w, 18.0), Sense::hover());
    let mg = widgets::line_galley(
        ui,
        &meta.join(" · "),
        theme::body(theme::TINY),
        pal.faint,
        w - 94.0,
    );
    ui.painter()
        .galley(pos2(mrect.left() + 94.0, mrect.top()), mg, pal.faint);
    ui.add_space(4.0);
}

/// Starter prompts on the welcome screen: (icon, title, hint, draft).
const STARTERS: [(&str, &str, &str, &str); 4] = [
    (
        icon::BUG,
        "修复问题",
        "复现、定位并修复，再运行相关测试",
        "修复下面的问题。先写出复现步骤，修改后运行相关测试，再请另一个 agent 审查：
",
    ),
    (
        icon::MAGNIFYING_GLASS,
        "审查改动",
        "多个 agent 交叉检查当前 Git 变更",
        "审查当前工作区的 Git 变更，指出正确性问题、风险和可简化之处，并给出修改建议。",
    ),
    (
        icon::BOOK_OPEN_TEXT,
        "理解项目",
        "梳理结构、入口和关键流程",
        "阅读这个项目，说明目录结构、构建与测试方法、主要入口和关键数据流，并记录到项目知识库。",
    ),
    (
        icon::FLASK,
        "补充测试",
        "为薄弱模块编写并运行测试",
        "找出测试覆盖薄弱的模块，补充有意义的测试并运行，报告结果。",
    ),
];

fn greeting() -> &'static str {
    use chrono::Timelike;
    match chrono::Local::now().hour() {
        5..=10 => "早上好，今天要完成什么？",
        11..=13 => "中午好，今天要完成什么？",
        14..=18 => "下午好，今天要完成什么？",
        _ => "晚上好，今天要完成什么？",
    }
}

/// Clickable starter card.
fn starter_card(ui: &mut Ui, w: f32, ic: &str, title: &str, text: &str, pal: &Palette) -> egui::Response {
    let h = 74.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, title));
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool(resp.id.with("hover"), resp.hovered());
        let p = ui.painter();
        let fill = pal.surface.lerp_to_gamma(pal.hover, t);
        p.rect(
            rect,
            CornerRadius::same(14),
            fill,
            Stroke::new(1.0_f32, pal.line_soft.lerp_to_gamma(pal.line, t)),
            egui::StrokeKind::Inside,
        );
        let tile = egui::Rect::from_min_size(rect.min + vec2(14.0, 17.0), vec2(40.0, 40.0));
        p.rect_filled(tile, CornerRadius::same(11), pal.pale);
        p.text(tile.center(), egui::Align2::CENTER_CENTER, ic, theme::body(20.0), pal.accent);
        let x = tile.right() + 12.0;
        let tw = rect.right() - x - 12.0;
        let tg = widgets::line_galley(ui, title, theme::strong(14.0), pal.ink, tw);
        let sg = widgets::line_galley(ui, text, theme::body(12.5), pal.mute, tw);
        p.galley(egui::pos2(x, rect.top() + 18.0), tg, pal.ink);
        p.galley(egui::pos2(x, rect.top() + 41.0), sg, pal.mute);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
