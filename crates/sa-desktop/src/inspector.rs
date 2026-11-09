//! Right inspector: collaboration tree, native tasks, plan and approvals.

use crate::app::{App, Tab};
use crate::host::Action;
use crate::feed::split_reply;
use crate::labels::{self, activity_state, activity_time, agent_name, job_state};
use crate::theme::{self, Palette};
use crate::widgets;
use egui::{
    Align, Align2, CornerRadius, Frame, Label, Layout, Margin, RichText, Sense, Stroke, Ui, pos2,
    vec2,
};
use egui_phosphor::regular as icon;
use sa_store::{JobView, Snapshot};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

fn route_text(route: &Option<Value>) -> Option<String> {
    let r = route.as_ref()?;
    let r = match r {
        Value::String(s) => serde_json::from_str::<Value>(s).ok()?,
        other => other.clone(),
    };
    let mut parts: Vec<String> = ["provider", "model", "mode"]
        .iter()
        .filter_map(|k| {
            r.get(*k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .collect();
    if let Some(effort) = r.get("reasoning_effort").and_then(Value::as_str) {
        parts.push(format!("思考强度 {effort}"));
    }
    (!parts.is_empty()).then(|| parts.join(" / "))
}

pub(crate) fn note(ui: &mut Ui, text: &str, pal: &Palette) {
    ui.label(
        RichText::new(text)
            .font(theme::body(theme::TINY + 0.5))
            .color(pal.mute),
    );
}

pub(crate) fn empty(ui: &mut Ui, ic: &str, text: &str, pal: &Palette) {
    ui.add_space(36.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(ic).font(theme::body(28.0)).color(pal.faint));
        ui.add_space(6.0);
        ui.label(RichText::new(text).font(theme::body(13.0)).color(pal.mute));
    });
}

impl App {
    pub fn inspector(&mut self, ui: &mut Ui) {
        let pal = self.pal.clone();
        let snap = self.view.as_ref().and_then(|v| v.snap.clone());
        Frame::new()
            .inner_margin(Margin {
                left: 16,
                right: 12,
                top: 14,
                bottom: 8,
            })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("项目面板")
                            .font(theme::strong(15.5))
                            .color(pal.ink),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::icon_button(ui, icon::X, 28.0, pal.mute, pal.hover, false)
                            .on_hover_text("关闭项目面板")
                            .clicked()
                        {
                            self.inspector_open = false;
                        }
                    });
                });
                ui.add_space(10.0);
                self.tabs(ui, snap.as_ref());
            });
        let writable = self.writable();
        let mut action: Option<Action> = None;
        egui::ScrollArea::vertical()
            .id_salt(("inspector", self.tab.key()))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                Frame::new()
                    .inner_margin(Margin {
                        left: 16,
                        right: 16,
                        top: 4,
                        bottom: 16,
                    })
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        if matches!(self.tab, Tab::Diff | Tab::Knowledge) {
                            let cwd = snap
                                .as_ref()
                                .map(|s| s.ticket.cwd.clone())
                                .or_else(|| {
                                    self.view.is_none().then(|| {
                                        self.composer.project.as_ref().map(|p| p.display().to_string())
                                    })?
                                });
                            if self.tab == Tab::Diff {
                                self.diff_tab(ui, cwd);
                            } else {
                                let ticket = snap.as_ref().map(|s| s.ticket.id);
                                self.knowledge_tab(ui, cwd, ticket);
                            }
                            return;
                        }
                        let Some(snap) = snap else {
                            let text = if self.view.is_some() {
                                "正在读取工单…"
                            } else {
                                "选择工单后，在这里查看协作分工。"
                            };
                            empty(ui, icon::SQUARES_FOUR, text, &pal);
                            return;
                        };
                        match self.tab {
                            Tab::Collab => self.collab_tab(ui, &snap),
                            Tab::Native => native_tab(ui, &snap, &pal),
                            Tab::Plan => {
                                let draft = writable.then_some(&mut self.composer.plan_draft);
                                action = plan_tab(ui, &snap, &pal, draft);
                            }
                            Tab::Approvals => action = approvals_tab(ui, &snap, &pal, writable),
                            Tab::Diff | Tab::Knowledge => {}
                        }
                    });
            });
        if let Some(a) = action {
            self.submit(a);
        }
    }

    fn tabs(&mut self, ui: &mut Ui, snap: Option<&Snapshot>) {
        let pal = self.pal.clone();
        let tabs = [
            (Tab::Collab, "协作", snap.map(|s| s.total_jobs.to_string())),
            (
                Tab::Native,
                "原生任务",
                snap.map(|s| s.native_activity.tasks.len())
                    .filter(|n| *n > 0)
                    .map(|n| n.to_string()),
            ),
            (
                Tab::Plan,
                "计划",
                snap.filter(|s| !s.plan.is_empty()).map(|s| {
                    format!(
                        "{}/{}",
                        s.plan.iter().filter(|p| p.done).count(),
                        s.plan.len()
                    )
                }),
            ),
            (
                Tab::Approvals,
                "审批",
                snap.map(|s| s.approvals.len())
                    .filter(|n| *n > 0)
                    .map(|n| n.to_string()),
            ),
            (Tab::Diff, "变更", None),
            (Tab::Knowledge, "知识", None),
        ];
        let resp = Frame::new()
            .fill(if pal.dark { pal.surface } else { pal.hover })
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(3))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let n = tabs.len() as f32;
                let avail = ui.available_width() - 6.0 - 2.0 * (n - 1.0);
                // Natural widths; counts are dropped when the panel is too narrow.
                let galleys: Vec<_> = tabs
                    .iter()
                    .map(|(tab, name, count)| {
                        let selected = self.tab == *tab;
                        let font = if selected { theme::strong(13.0) } else { theme::body(13.0) };
                        let color = if selected { pal.ink } else { pal.mute };
                        let ng = widgets::line_galley(ui, name, font, color, avail);
                        let tone = if *tab == Tab::Approvals { pal.warn.fg } else { pal.faint };
                        let cg = count
                            .as_ref()
                            .map(|c| widgets::line_galley(ui, c, theme::body(theme::TINY), tone, 60.0));
                        (ng, cg, color)
                    })
                    .collect();
                let natural = |with_counts: bool| -> Vec<f32> {
                    galleys
                        .iter()
                        .map(|(ng, cg, _)| {
                            let c = if with_counts { cg.as_ref().map_or(0.0, |g| g.size().x + 5.0) } else { 0.0 };
                            ng.size().x + c + 14.0
                        })
                        .collect()
                };
                let mut widths = natural(true);
                let counts = widths.iter().sum::<f32>() <= avail;
                if !counts {
                    widths = natural(false);
                }
                let extra = ((avail - widths.iter().sum::<f32>()) / n).max(0.0);
                ui.horizontal(|ui| {
                    for (((tab, _, _), (ng, cg, color)), w) in tabs.iter().zip(galleys).zip(widths) {
                        let (rect, resp) = ui.allocate_exact_size(vec2(w + extra, 30.0), Sense::click());
                        let selected = self.tab == *tab;
                        let p = ui.painter();
                        if selected {
                            p.rect_filled(
                                rect.translate(vec2(0.0, 1.0)),
                                CornerRadius::same(8),
                                egui::Color32::from_black_alpha(10),
                            );
                            p.rect_filled(
                                rect,
                                CornerRadius::same(8),
                                if pal.dark { pal.selected } else { pal.surface },
                            );
                        } else if resp.hovered() {
                            p.rect_filled(
                                rect,
                                CornerRadius::same(8),
                                pal.line_soft.gamma_multiply(0.6),
                            );
                        }
                        let cg = cg.filter(|_| counts);
                        let total = ng.size().x + cg.as_ref().map_or(0.0, |g| g.size().x + 5.0);
                        let x = rect.center().x - total / 2.0;
                        let nh = ng.size().y;
                        p.galley(pos2(x, rect.center().y - nh / 2.0), ng.clone(), color);
                        if let Some(cg) = cg {
                            let ch = cg.size().y;
                            p.galley(
                                pos2(x + ng.size().x + 5.0, rect.center().y - ch / 2.0 + 0.5),
                                cg,
                                pal.faint,
                            );
                        }
                        if resp
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .clicked()
                        {
                            self.tab = *tab;
                        }
                    }
                });
            });
        let _ = resp;
        ui.add_space(6.0);
    }

    /// Per-file review and persisted worktree checkpoints for this project.
    fn diff_tab(&mut self, ui: &mut Ui, cwd: Option<String>) {
        let pal = self.pal.clone();
        let Some(cwd) = cwd else {
            empty(ui, icon::GIT_DIFF, "选择工单或项目目录后查看 Git 变更。", &pal);
            return;
        };
        let data = self.opts.data.clone().unwrap_or_else(sa_store::default_data_dir);
        self.diff.show(ui, &pal, cwd, data, self.host.is_some());
    }
    fn collab_tab(&mut self, ui: &mut Ui, snap: &Snapshot) {
        let pal = self.pal.clone();
        if let Some(host)=&self.host {
            let action=self.workflow.show(ui,&pal,host,snap.ticket.id);
            if let Some(action)=action {self.submit(action);}
        }
        let badges = self.live_badges();
        if let Some(host) = &self.host {
            let ticket = snap.ticket.id;
            let mut on = host.failover(ticket);
            let before = on;
            widgets::toggle(ui, &mut on, "只读子任务的执行端不可用时自动改派", &pal)
                .on_hover_text("仅对只读子任务生效；写入任务和主控从不自动改派。");
            if on != before {
                self.submit(Action::SetFailover { ticket, enabled: on });
            }
            ui.add_space(6.0);
        }
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let fresh: HashMap<i64, &JobView> = snap.jobs.iter().map(|j| (j.id, j)).collect();
        let paging = view.paging.as_ref();
        let jobs: Vec<JobView> = match paging.and_then(|p| p.page.as_ref()) {
            Some(page) => page
                .jobs
                .iter()
                .map(|j| fresh.get(&j.id).map(|f| (*f).clone()).unwrap_or(j.clone()))
                .collect(),
            None => snap.jobs.clone(),
        };
        let total = snap.total_jobs.max(jobs.len() as i64);
        let mut action: Option<Vec<Option<i64>>> = None;
        let mut overview = false;

        ui.horizontal(|ui| {
            let text = match paging {
                Some(p) if p.page.is_some() => format!(
                    "第 {} 页 · 显示 {} / 共 {} 个任务",
                    p.cursors.len(),
                    jobs.len(),
                    total
                ),
                _ => format!("任务概览 · 显示 {} / 共 {} 个任务", jobs.len(), total),
            };
            note(ui, &text, &pal);
            if paging.is_some_and(|p| p.busy) {
                ui.spinner();
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let busy = paging.is_some_and(|p| p.busy);
            match paging {
                Some(p) if p.page.is_some() => {
                    if widgets::quiet_button(ui, icon::SQUARES_FOUR, "概览", &pal, false).clicked()
                        && !busy
                    {
                        overview = true;
                    }
                    if p.cursors.len() > 1
                        && widgets::quiet_button(ui, icon::CARET_LEFT, "上一页", &pal, false)
                            .clicked()
                        && !busy
                    {
                        action = Some(p.cursors[..p.cursors.len() - 1].to_vec());
                    }
                    if let Some(page) = &p.page
                        && page.more
                        && widgets::quiet_button(ui, icon::CARET_RIGHT, "下一页", &pal, false)
                            .clicked()
                        && !busy
                    {
                        let mut c = p.cursors.clone();
                        c.push(page.next);
                        action = Some(c);
                    }
                    if widgets::quiet_button(ui, icon::ARROWS_CLOCKWISE, "刷新本页", &pal, false)
                        .clicked()
                        && !busy
                    {
                        action = Some(p.cursors.clone());
                    }
                }
                _ if (jobs.len() as i64) < total
                    && widgets::quiet_button(ui, icon::STACK, "分页浏览全部任务", &pal, false)
                        .clicked()
                    && !busy =>
                {
                    action = Some(vec![None]);
                }
                _ => {}
            }
        });
        if let Some(err) = paging.and_then(|p| p.error.clone()) {
            ui.label(
                RichText::new(err)
                    .font(theme::body(theme::TINY))
                    .color(pal.fail.fg),
            );
        }
        ui.add_space(6.0);

        if jobs.is_empty() {
            empty(ui, icon::TREE_STRUCTURE, "正在等候首个任务。", &pal);
        }
        // Tree by parent; orphans (parent outside this page) become roots.
        let ids: HashSet<i64> = jobs.iter().map(|j| j.id).collect();
        let mut children: HashMap<i64, Vec<&JobView>> = HashMap::new();
        let mut roots: Vec<&JobView> = Vec::new();
        for j in &jobs {
            match j.parent.filter(|p| ids.contains(p)) {
                Some(p) => children.entry(p).or_default().push(j),
                None => roots.push(j),
            }
        }
        roots.sort_by_key(|j| (j.parent.is_some(), j.id));
        for list in children.values_mut() {
            list.sort_by_key(|j| j.id);
        }
        let mut order: Vec<(&JobView, usize)> = Vec::new();
        fn walk<'a>(
            j: &'a JobView,
            depth: usize,
            children: &HashMap<i64, Vec<&'a JobView>>,
            out: &mut Vec<(&'a JobView, usize)>,
        ) {
            out.push((j, depth));
            if depth > 32 {
                return;
            }
            for c in children.get(&j.id).into_iter().flatten() {
                walk(c, depth + 1, children, out);
            }
        }
        for r in &roots {
            walk(r, 0, &children, &mut order);
        }
        let permission = snap.ticket.permission_profile.clone();
        let writable = self.host.is_some();
        let stopped = snap.ticket.state == "cancelled";
        let mut toggle = None;
        let mut act: Option<(CardAction, i64, String)> = None;
        ui.spacing_mut().item_spacing.y = 6.0;
        for (job, depth) in order {
            let indent = (depth.min(4) as f32) * 16.0;
            let open = view.open_jobs.contains(&job.id);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                if depth > 0 {
                    let (r, _) = ui.allocate_exact_size(vec2(indent, 40.0), Sense::hover());
                    let x = r.right() - 8.0;
                    let p = ui.painter();
                    p.line_segment(
                        [pos2(x, r.top() - 8.0), pos2(x, r.center().y)],
                        Stroke::new(1.0_f32, pal.line),
                    );
                    p.line_segment(
                        [pos2(x, r.center().y), pos2(r.right(), r.center().y)],
                        Stroke::new(1.0_f32, pal.line),
                    );
                }
                ui.vertical(|ui| {
                    let card = CardCtx { permission: &permission, writable, stopped, badge: badges.get(&job.id) };
                    match job_card(ui, job, open, &card, &pal) {
                        Some(CardAction::Toggle) => toggle = Some(job.id),
                        Some(a) => act = Some((a, job.id, job.agent.clone())),
                        None => {}
                    }
                });
            });
        }
        if let Some(id) = toggle
            && !view.open_jobs.remove(&id)
        {
            view.open_jobs.insert(id);
        }
        if overview {
            view.paging = None;
            view.open_jobs.clear();
        }
        if let Some(c) = action {
            self.request_jobs_page(c);
        }
        match act {
            Some((CardAction::Note, job, agent)) => self.set_target(job, agent),
            Some((CardAction::Retry, job, _)) => self.submit(Action::Retry { job }),
            _ => {}
        }
    }
}

/// Returns true when the header was clicked.
/// What the operator did on a job card.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CardAction {
    Toggle,
    Note,
    Retry,
}

/// Ticket-wide inputs of a job card.
struct CardCtx<'a> {
    permission: &'a str,
    /// This window schedules the journal.
    writable: bool,
    /// The ticket is cancelled.
    stopped: bool,
    /// Live engine badge replacing the state chip.
    badge: Option<&'a (String, crate::theme::Tone)>,
}

/// `writable`: this window schedules the journal; `stopped`: the ticket is cancelled.
fn job_card(
    ui: &mut Ui,
    job: &JobView,
    open: bool,
    card: &CardCtx,
    pal: &Palette,
) -> Option<CardAction> {
    let CardCtx { permission: ticket_permission, writable, stopped, badge } = *card;
    let mut clicked = None;
    let stroke = if job.state == "running" {
        pal.run.fg.gamma_multiply(0.45)
    } else {
        pal.line_soft
    };
    Frame::new()
        .fill(pal.surface)
        .stroke(Stroke::new(1.0_f32, stroke))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin {
            left: 10,
            right: 10,
            top: 8,
            bottom: 8,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 3.0;
            let head = ui
                .horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 7.0;
                    widgets::avatar(ui, &job.agent, 22.0, pal);
                    ui.label(
                        RichText::new(agent_name(&job.agent))
                            .font(theme::strong(13.5))
                            .color(pal.ink),
                    );
                    ui.label(
                        RichText::new(format!("#{}", job.id))
                            .font(theme::body(theme::TINY))
                            .color(pal.faint),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(if open {
                                icon::CARET_UP
                            } else {
                                icon::CARET_DOWN
                            })
                            .font(theme::body(12.0))
                            .color(pal.faint),
                        );
                        match badge {
                            Some((text, tone)) => {
                                widgets::chip(ui, text, *tone, theme::TINY);
                            }
                            None => {
                                widgets::chip(
                                    ui,
                                    &job_state(&job.state),
                                    pal.state_tone(&job.state),
                                    theme::TINY,
                                );
                            }
                        }
                    });
                })
                .response;
            let role = if !job.role.is_empty() {
                job.role.clone()
            } else if job.parent.is_none() {
                "主控".into()
            } else {
                "协作者".into()
            };
            let w = ui.available_width();
            let line = format!("{} · 第 {} 轮", labels::compact(&role, 60), job.turn.max(1));
            let g = widgets::line_galley(ui, &line, theme::body(12.5), pal.mute, w);
            ui.add(Label::new(g).selectable(false));
            if let Some(r) = route_text(&job.route) {
                let g = widgets::line_galley(ui, &r, theme::mono(11.5), pal.faint, w);
                ui.add(Label::new(g).selectable(false))
                    .on_hover_text("模型路由");
            }
            let area = head.rect.union(ui.min_rect());
            let resp = ui.interact(area, ui.id().with(("job", job.id)), Sense::click());
            if resp
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                clicked = Some(CardAction::Toggle);
            }
            let can_note = writable && matches!(job.state.as_str(), "running" | "queued" | "waiting");
            let can_retry = writable && !stopped && matches!(job.state.as_str(), "failed" | "interrupted");
            if can_note || can_retry {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    if can_note
                        && widgets::quiet_button(ui, icon::CHAT_CIRCLE_DOTS, "定向留言", pal, false)
                            .on_hover_text("只发给这个任务")
                            .clicked()
                    {
                        clicked = Some(CardAction::Note);
                    }
                    if can_retry
                        && widgets::quiet_button(ui, icon::ARROW_CLOCKWISE, "重试", pal, false)
                            .on_hover_text("重试同一逻辑轮次，保留已提交消息的回执")
                            .clicked()
                    {
                        clicked = Some(CardAction::Retry);
                    }
                });
            }
            if open {
                ui.add_space(6.0);
                widgets::rule(ui, pal.line_soft);
                ui.add_space(6.0);
                let permission = job
                    .permission_override
                    .clone()
                    .unwrap_or(ticket_permission.to_string());
                ui.label(
                    RichText::new(format!("权限 · {}", labels::profile(&permission)))
                        .font(theme::body(theme::TINY + 0.5))
                        .color(pal.mute),
                );
                if !job.body.trim().is_empty() {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("任务说明")
                            .font(theme::strong(12.5))
                            .color(pal.ink),
                    );
                    let body: String = job.body.chars().take(1200).collect();
                    ui.add(
                        Label::new(
                            RichText::new(body.trim())
                                .font(theme::body(12.5))
                                .color(pal.ink),
                        )
                        .wrap(),
                    );
                }
                if let Some(result) = &job.result {
                    let (clean, _) = split_reply(result);
                    if !clean.is_empty() {
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new("回报")
                                .font(theme::strong(12.5))
                                .color(pal.ink),
                        );
                        Frame::new()
                            .fill(pal.code_bg)
                            .corner_radius(CornerRadius::same(8))
                            .inner_margin(Margin::symmetric(10, 8))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                let text: String = clean.chars().take(1600).collect();
                                let look = crate::markdown::Look {
                                    pal,
                                    size: 12.5,
                                    color: pal.ink,
                                };
                                crate::markdown::show(
                                    ui,
                                    &crate::markdown::parse(&text),
                                    &look,
                                    job.id as u64,
                                );
                            });
                    }
                }
            }
        });
    clicked
}

fn native_tab(ui: &mut Ui, snap: &Snapshot, pal: &Palette) {
    let tasks = &snap.native_activity.tasks;
    note(
        ui,
        "执行端内部派生的子任务（如 Claude 子代理），按最近更新排列。",
        pal,
    );
    ui.add_space(8.0);
    if tasks.is_empty() {
        empty(ui, icon::ROBOT, "尚未收到内部任务事件。", pal);
        return;
    }
    let mut list: Vec<_> = tasks.iter().collect();
    list.sort_by(|a, b| b.ts.partial_cmp(&a.ts).unwrap_or(std::cmp::Ordering::Equal));
    ui.spacing_mut().item_spacing.y = 6.0;
    for t in list {
        let owner = snap
            .jobs
            .iter()
            .find(|j| j.id == t.job)
            .map(|j| j.agent.clone())
            .unwrap_or_default();
        widgets::card(pal).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 3.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 7.0;
                if !owner.is_empty() {
                    widgets::avatar(ui, &owner, 18.0, pal);
                }
                let state_label = activity_state(&t.state);
                let w = ui.available_width() - 70.0;
                let g = widgets::line_galley(
                    ui,
                    &labels::compact(&t.label, 120),
                    theme::strong(13.0),
                    pal.ink,
                    w,
                );
                ui.add(Label::new(g).selectable(false))
                    .on_hover_text(&t.label);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    widgets::chip(ui, &state_label, pal.state_tone(&t.state), theme::TINY);
                });
            });
            if let Some(title) = t.title.as_deref().filter(|s| !s.trim().is_empty()) {
                ui.add(
                    Label::new(RichText::new(title).font(theme::body(12.5)).color(pal.ink)).wrap(),
                );
            }
            let mut meta = vec![format!("平台任务 #{}", t.job), format!("第 {} 轮", t.turn)];
            if let Some(n) = t.tool_uses.filter(|n| *n >= 0) {
                meta.push(format!("已用工具 {n} 次"));
            }
            if !t.last_tool.is_empty() {
                meta.push(format!("最近工具 {}", labels::compact(&t.last_tool, 40)));
            }
            let time = activity_time(t.ts);
            if !time.is_empty() {
                meta.push(time);
            }
            ui.add(
                Label::new(
                    RichText::new(meta.join(" · "))
                        .font(theme::body(theme::TINY))
                        .color(pal.mute),
                )
                .wrap(),
            );
            if !t.id.is_empty() {
                let w = ui.available_width();
                let g = widgets::line_galley(
                    ui,
                    &format!("内部任务 {}", t.id),
                    theme::mono(11.0),
                    pal.faint,
                    w,
                );
                ui.add(Label::new(g).selectable(false));
            }
        });
    }
}

fn plan_tab(ui: &mut Ui, snap: &Snapshot, pal: &Palette, draft: Option<&mut String>) -> Option<Action> {
    let ticket = snap.ticket.id;
    let writable = draft.is_some();
    let mut action = None;
    match draft {
        Some(draft) => {
            note(ui, "这份清单属于当前会话，主控每轮都会收到它。", pal);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let edit = egui::TextEdit::singleline(draft)
                    .hint_text(RichText::new("添加计划项").color(pal.faint))
                    .font(theme::body(13.0))
                    .char_limit(2000)
                    .desired_width(ui.available_width() - 72.0);
                let resp = ui.add(edit);
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "计划项输入"));
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let add = widgets::quiet_button(ui, icon::PLUS, "添加", pal, false).clicked();
                if (enter || add) && !draft.trim().is_empty() {
                    action = Some(Action::PlanAdd { ticket, body: draft.trim().to_string() });
                }
            });
        }
        None => note(ui, "这份清单属于当前会话，由持有调度的工作台维护；此处只读。", pal),
    }
    ui.add_space(8.0);
    if snap.plan.is_empty() {
        empty(ui, icon::LIST_CHECKS, "这个会话还没有计划项。", pal);
        return action;
    }
    let done = snap.plan.iter().filter(|p| p.done).count();
    let total = snap.plan.len();
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("已完成 {done} / {total}"))
                .font(theme::strong(13.0))
                .color(pal.ink),
        );
    });
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
    ui.painter()
        .rect_filled(r, CornerRadius::same(3), pal.line_soft);
    let mut fill = r;
    fill.set_width(r.width() * done as f32 / total.max(1) as f32);
    ui.painter()
        .rect_filled(fill, CornerRadius::same(3), pal.accent);
    ui.add_space(10.0);
    widgets::card(pal)
        .inner_margin(Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            for (i, item) in snap.plan.iter().enumerate() {
                if i > 0 {
                    widgets::rule(ui, pal.line_soft);
                }
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let sense = if writable { Sense::click() } else { Sense::hover() };
                    let (rect, resp) = ui.allocate_exact_size(vec2(26.0, 34.0), sense);
                    let what = if item.done { "标记为未完成" } else { "标记为已完成" };
                    resp.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Checkbox, writable, format!("{what} {}", item.body))
                    });
                    if writable
                        && resp
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .on_hover_text(if item.done { "标记为未完成" } else { "标记为已完成" })
                            .clicked()
                    {
                        action = Some(Action::PlanToggle { ticket, id: item.id });
                    }
                    let (ic, color) = if item.done {
                        (icon::CHECK_SQUARE, pal.accent)
                    } else {
                        (icon::SQUARE, pal.faint)
                    };
                    ui.painter().text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        ic,
                        theme::body(17.0),
                        color,
                    );
                    ui.vertical(|ui| {
                        ui.add_space(7.0);
                        let mut text = RichText::new(&item.body).font(theme::body(13.5));
                        text = if item.done {
                            text.color(pal.mute).strikethrough()
                        } else {
                            text.color(pal.ink)
                        };
                        ui.add(Label::new(text).wrap());
                        ui.add_space(7.0);
                    });
                });
            }
        });
    action
}

fn approvals_tab(ui: &mut Ui, snap: &Snapshot, pal: &Palette, writable: bool) -> Option<Action> {
    let mut action = None;
    if !writable {
        widgets::callout(pal.neutral).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.label(RichText::new(icon::LOCK_SIMPLE).font(theme::body(15.0)).color(pal.mute));
                ui.add(
                    Label::new(
                        RichText::new("只读观察模式只展示待审批请求，不会允许或拒绝。请在持有调度的工作台中处理这些请求。")
                            .font(theme::body(12.5))
                            .color(pal.mute),
                    )
                    .wrap(),
                );
            });
        });
        ui.add_space(10.0);
    }
    if snap.approvals.is_empty() {
        empty(ui, icon::SHIELD_CHECK, "没有待审批的请求。", pal);
        return None;
    }
    ui.spacing_mut().item_spacing.y = 8.0;
    for a in &snap.approvals {
        let agent = snap
            .jobs
            .iter()
            .find(|j| j.id == a.job)
            .map(|j| j.agent.clone())
            .unwrap_or_default();
        Frame::new()
            .fill(pal.surface)
            .stroke(Stroke::new(1.0_f32, pal.warn.fg.gamma_multiply(0.35)))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 7.0;
                    ui.label(
                        RichText::new(icon::SHIELD_WARNING)
                            .font(theme::body(16.0))
                            .color(pal.warn.fg),
                    );
                    ui.label(
                        RichText::new(format!("任务 #{} · 请求 #{}", a.job, a.id))
                            .font(theme::strong(13.5))
                            .color(pal.ink),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        widgets::chip(ui, "等待处理", pal.warn, theme::TINY);
                    });
                });
                let tool = a
                    .action
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("请求授权");
                let who = if agent.is_empty() {
                    String::new()
                } else {
                    format!("{} · ", agent_name(&agent))
                };
                ui.label(
                    RichText::new(format!("{who}请求使用 {tool}"))
                        .font(theme::body(13.0))
                        .color(pal.ink),
                );
                let pretty = match &a.action {
                    Value::String(s) => s.clone(),
                    v => serde_json::to_string_pretty(v).unwrap_or_default(),
                };
                let pretty: String = pretty.lines().take(24).collect::<Vec<_>>().join("\n");
                Frame::new()
                    .fill(pal.code_bg)
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let mut job = egui::text::LayoutJob::simple(
                            pretty,
                            theme::mono(11.5),
                            pal.ink,
                            ui.available_width(),
                        );
                        job.wrap.break_anywhere = true;
                        // Pre-laid-out galley: Label would reset `break_anywhere`.
                        let galley = ui.painter().layout_job(job);
                        ui.add(Label::new(galley));
                    });
                if writable {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        if widgets::quiet_button(ui, icon::CHECK, "允许本次请求", pal, false)
                            .on_hover_text("只批准这一项请求")
                            .clicked()
                        {
                            action = Some(Action::Decide { id: a.id, allow: true });
                        }
                        if widgets::quiet_button(ui, icon::X, "拒绝", pal, false).clicked() {
                            action = Some(Action::Decide { id: a.id, allow: false });
                        }
                    });
                }
            });
    }
    action
}
