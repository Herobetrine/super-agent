//! Central column: banners, the autonomy strip, the feed, and — docked above
//! the input — one line saying who is working right now.

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

/// Reading width of the conversation; the composer shares it.
const COLUMN: f32 = 800.0;
/// Width of the avatar gutter; reply bodies start after it.
const GUTTER: f32 = 32.0;

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
        ControlKind::ReportDone | ControlKind::CheckPass => icon::CHECK,
        ControlKind::ReportPartial | ControlKind::ReportBlocked | ControlKind::CheckFail => icon::WARNING,
        ControlKind::Ask => icon::CHAT_TEXT,
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

#[derive(Clone, Default)]
struct QuestionDraft {
    body: String,
    pending: bool,
    answered: bool,
    ime_active: bool,
}

pub fn answer_finished(ctx: &egui::Context, question: i64, ok: bool) {
    let id = egui::Id::new(("question-answer", question));
    ctx.data_mut(|data| {
        let mut draft = data.get_temp::<QuestionDraft>(id).unwrap_or_default();
        draft.pending = false;
        draft.answered = ok;
        data.insert_temp(id, draft);
    });
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
        let snap = self.view.as_ref().and_then(|v| v.snap.clone());
        egui::TopBottomPanel::bottom("footer")
            .frame(Frame::new().inner_margin(Margin {
                left: 20,
                right: 20,
                top: 0,
                bottom: 10,
            }))
            .show_separator_line(false)
            .show_inside(ui, |ui| {
                centered(ui, COLUMN, 0.0, |ui| {
                    if let Some(s) = snap.as_ref().filter(|s| {
                        labels::is_active(&s.ticket.state) || s.ticket.pause_status.is_some()
                    }) {
                        self.status_strip(ui, s);
                        ui.add_space(6.0);
                    }
                    if let Some(s) = &snap {
                        self.question_cards(ui, s);
                    }
                    if self.writable() {
                        self.composer(ui);
                    } else {
                        self.footer(ui);
                    }
                    if let Some(s) = &snap {
                        ui.add_space(6.0);
                        crate::usage::footer(ui, &s.usage, &s.quota, &pal);
                    }
                });
            });

        ui.add_space(12.0);
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
                            .font(theme::strong(theme::UI))
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
        let Some(snap) = snap else {
            if view.error.is_none() {
                ui.add_space(80.0);
                ui.vertical_centered(|ui| {
                    widgets::spinner(ui);
                    ui.add_space(8.0);
                    ui.label(RichText::new("正在读取工单…").font(theme::body(theme::UI)).color(pal.mute));
                });
            }
            return;
        };

        let auto = snap.ticket.permission_profile == "auto" || snap.run.is_some();
        if auto {
            centered(ui, COLUMN, 24.0, |ui| self.autonomy_strip(ui, &snap));
            ui.add_space(4.0);
        }
        self.feed_area(ui, &snap);
    }

    fn question_cards(&mut self, ui: &mut Ui, snap: &Snapshot) {
        if snap.questions.is_empty() {
            return;
        }
        let pal = self.pal.clone();
        let writable = self.writable() && !self.opts.demo;
        let mut questions: Vec<_> = snap.questions.iter().collect();
        questions.sort_by(|a, b| a.asked.total_cmp(&b.asked).then(a.id.cmp(&b.id)));
        let mut action = None;
        egui::ScrollArea::vertical()
            .id_salt(("questions", snap.ticket.id))
            .max_height((ui.available_height() * 0.45).clamp(140.0, 260.0))
            .show(ui, |ui| {
                for q in questions {
                    let id = egui::Id::new(("question-answer", q.id));
                    let mut draft = ui.ctx().data_mut(|data| data.get_temp::<QuestionDraft>(id).unwrap_or_default());
                    let who = if q.origin == "platform" {
                        "平台已暂停这项工作".to_owned()
                    } else if let Some(job) = snap.jobs.iter().find(|job| job.id == q.job) {
                        if job.parent.is_none() {
                            format!("主控 {} 想请你决定", agent_name(&job.agent))
                        } else {
                            format!("协作者 {} #{} 想请你决定", agent_name(&job.agent), job.id)
                        }
                    } else {
                        format!("协作者 #{} 想请你决定", q.job)
                    };
                    widgets::callout(pal.warn).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(who).font(theme::strong(theme::UI)).color(pal.ink));
                        ui.add(Label::new(RichText::new(&q.question).font(theme::body(theme::UI)).color(pal.ink)).wrap().selectable(true));
                        if writable {
                            ui.add_enabled_ui(!draft.pending && !draft.answered, |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    for option in &q.options {
                                        if widgets::outline_button(ui, "", option, &pal).clicked() {
                                            action = Some(crate::host::Action::Answer { question: q.id, body: option.clone() });
                                        }
                                    }
                                });
                                ui.horizontal(|ui| {
                                    let editor = egui::Id::new(("answer-input", q.id));
                                    let keyboard_ready = crate::composer::keyboard_ready(ui, editor, &mut draft.ime_active);
                                    let response = ui.add(
                                        egui::TextEdit::singleline(&mut draft.body)
                                            .id(editor)
                                            .font(theme::body(theme::UI))
                                            .margin(Margin::symmetric(8, 5))
                                            .desired_width((ui.available_width() - 70.0).max(70.0)),
                                    );
                                    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, ui.is_enabled(), format!("回答问题 #{}", q.id)));
                                    let enter = keyboard_ready && (response.has_focus() || response.lost_focus())
                                        && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                                    let can_answer = ui.is_enabled() && !draft.body.trim().is_empty();
                                    let clicked = widgets::primary_button(ui, "", "回答", &pal, can_answer).clicked();
                                    if (enter || clicked) && !draft.body.trim().is_empty() {
                                        action = Some(crate::host::Action::Answer { question: q.id, body: draft.body.trim().to_owned() });
                                    }
                                });
                            });
                        }
                        if let Some(fallback) = &q.fallback {
                            ui.add(Label::new(RichText::new(format!("无人回答时：{fallback}")).font(theme::body(theme::SMALL)).color(pal.mute)).wrap());
                        }
                    });
                    if action.as_ref().is_some_and(|a| matches!(a, crate::host::Action::Answer { question, .. } if *question == q.id)) {
                        draft.pending = true;
                    }
                    ui.ctx().data_mut(|data| data.insert_temp(id, draft));
                    ui.add_space(6.0);
                }
            });
        if let Some(action) = action {
            let question = match &action { crate::host::Action::Answer { question, .. } => *question, _ => return };
            let before = self.composer.pending;
            self.submit(action);
            if self.composer.pending == before {
                answer_finished(ui.ctx(), question, false);
            }
        }
    }

    fn error_banner(&self, ui: &mut Ui, err: &str) {
        let pal = &self.pal;
        widgets::callout(pal.fail).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(icon::WARNING_CIRCLE)
                        .font(theme::body(16.0))
                        .color(pal.fail.fg),
                );
                ui.label(
                    RichText::new("无法读取工单数据库")
                        .font(theme::strong(theme::UI))
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

    /// New-conversation home: greeting, starter prompts and recent projects,
    /// left-aligned with the composer below.
    fn welcome(&mut self, ui: &mut Ui) {
        let pal = self.pal.clone();
        let h = ui.available_height();
        // Sit a little above the middle of the free space.
        ui.add_space(((h - 330.0) * 0.4).max(16.0));
        let mut pick_draft: Option<String> = None;
        let mut pick_project: Option<std::path::PathBuf> = None;
        centered(ui, COLUMN, 20.0, |ui| {
            let indent = 10.0;
            ui.horizontal(|ui| {
                ui.add_space(indent);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    ui.label(RichText::new(greeting()).font(theme::body(25.0)).color(pal.ink));
                    let project = self
                        .composer
                        .project
                        .as_ref()
                        .map(|p| format!("项目 · {}", labels::basename(&p.to_string_lossy())))
                        .unwrap_or_else(|| "未选择项目时使用新的临时目录".into());
                    ui.label(
                        RichText::new(format!("{project}　·　主控 {}", labels::agent_name(&self.composer.lead)))
                            .font(theme::body(theme::UI))
                            .color(pal.mute),
                    );
                });
            });
            ui.add_space(24.0);
            ui.spacing_mut().item_spacing.y = 2.0;
            for (ic, title, text, prompt) in &STARTERS {
                if starter_row(ui, ic, title, text, &pal).clicked() {
                    pick_draft = Some(prompt.to_string());
                }
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
                ui.add_space(22.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
                    ui.add_space(indent);
                    ui.label(RichText::new("最近的项目").font(theme::body(theme::SMALL)).color(pal.faint));
                    ui.add_space(4.0);
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
                self.composer.extensions = None;
            }
            self.composer.project = Some(p);
        }
    }

    fn empty_state(&self, ui: &mut Ui) {
        let pal = &self.pal;
        let h = ui.available_height();
        ui.add_space((h * 0.3).max(30.0));
        ui.vertical_centered(|ui| {
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
                    .font(theme::strong(17.0))
                    .color(pal.ink),
            );
            ui.add_space(6.0);
            ui.label(RichText::new(hint).font(theme::body(theme::UI)).color(pal.mute));
        });
    }

    /// Continuous-run summary pinned above the feed: stage, goal, last lesson.
    fn autonomy_strip(&self, ui: &mut Ui, snap: &Snapshot) {
        let pal = &self.pal;
        widgets::well(pal)
            .inner_margin(Margin {
                left: 14,
                right: 14,
                top: 9,
                bottom: 10,
            })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(
                        RichText::new(icon::LIGHTNING)
                            .font(theme::body(14.0))
                            .color(pal.mute),
                    );
                    ui.label(
                        RichText::new("自动运行")
                            .font(theme::strong(theme::UI))
                            .color(pal.ink),
                    );
                    ui.label(
                        RichText::new(auto_stage(snap))
                            .font(theme::body(theme::UI))
                            .color(pal.mute),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if let Some(run) = &snap.run {
                            let phase = match run.phase.as_str() {
                                "work" => "执行中",
                                "discover" => "探查中",
                                "exhausted" => "已穷尽",
                                other => other,
                            };
                            let mut meta = format!("阶段 · {phase}");
                            if run.milestone > 0 {
                                meta = format!("已完成里程碑 {} · {meta}", run.milestone);
                            }
                            ui.label(
                                RichText::new(meta)
                                    .font(theme::body(theme::TINY))
                                    .color(pal.mute),
                            );
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
                    theme::body(theme::UI),
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
                let g = widgets::line_galley(ui, &text, theme::body(theme::SMALL), pal.mute, w);
                let r = ui.add(Label::new(g).selectable(false));
                if let Some(l) = learning {
                    r.on_hover_text(l);
                }
            });
    }

    /// One line above the input: who is working, on what, for how long, and
    /// the pause control. Also shown while a ticket is paused.
    fn status_strip(&mut self, ui: &mut Ui, snap: &Snapshot) {
        let pal = self.pal.clone();
        let status = snap.ticket.state.as_str();
        let active = labels::is_active(status);
        let pause = snap.ticket.pause_status.clone();
        let root = snap.jobs.iter().find(|j| j.parent.is_none());
        let mut badge = match (active, root) {
            (true, Some(r)) => self.live_badges().remove(&r.id),
            _ => None,
        };
        if let Some(root) = root.filter(|root| root.state == "running") {
            let latest = snap.events.iter()
                .filter(|event| event.job == Some(root.id) && matches!(event.kind.as_str(), "check.started" | "check.result" | "reply" | "turn.failed" | "cancelled"))
                .max_by_key(|event| event.seq);
            if let Some(event) = latest.filter(|event| event.kind == "check.started") {
                badge = Some((labels::collaboration_event("check.started", &event.payload).unwrap_or_default(), pal.run));
            }
        }
        // Running collaborators by agent, the lead excluded.
        let mut running: Vec<(String, usize)> = Vec::new();
        for j in &snap.jobs {
            if j.state != "running" || Some(j.id) == root.map(|r| r.id) {
                continue;
            }
            match running.iter_mut().find(|(a, _)| *a == j.agent) {
                Some((_, c)) => *c += 1,
                None => running.push((j.agent.clone(), 1)),
            }
        }
        let head = format!("{} {}", agent_name(&snap.ticket.lead), labels::job_state(status));
        // Prefer the native activity when it is at least as recent as the journal one.
        let feed_latest = self.view.as_ref().and_then(|v| v.feed.latest.clone());
        let native = snap.native_activity.latest.as_ref();
        let (label, ts, job, turn, agent) = match (native, &feed_latest) {
            (Some(n), Some(f)) if n.ts < f.ts => {
                (f.label.clone(), f.ts, f.job, f.turn, f.agent.clone())
            }
            (Some(n), _) => (n.label.clone(), n.ts, Some(n.job), Some(n.turn), None),
            (None, Some(f)) => (f.label.clone(), f.ts, f.job, f.turn, f.agent.clone()),
            (None, None) => (String::new(), 0.0, None, None, None),
        };
        let detail = if !active {
            String::new()
        } else if label.is_empty() {
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
                labels::compact(&label, 160),
                source,
                turn.map(|t| format!("第 {t} 轮")).unwrap_or_default(),
                if ts > 0.0 { labels::age(ts) } else { String::new() },
            ]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · ")
        };
        let (pause_label, pause_hint) = crate::composer::pause_text(pause.as_deref());
        let writable = self.writable();
        let mut action = None;
        ui.horizontal(|ui| {
            ui.set_min_height(28.0);
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.add_space(6.0);
            let (r, _) = ui.allocate_exact_size(vec2(12.0, 20.0), Sense::hover());
            if active {
                widgets::dot(ui, r.center(), pal.run.fg, true);
            } else {
                ui.painter().text(
                    r.center(),
                    Align2::CENTER_CENTER,
                    icon::PAUSE,
                    theme::body(13.0),
                    pal.mute,
                );
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                if writable {
                    action = self.pause_control(ui, snap.ticket.id, pause.as_deref());
                }
                if let Some((text, tone)) = &badge {
                    widgets::chip(ui, text, *tone, theme::TINY).on_hover_text("主控的实时状态");
                }
                if active {
                    ui.label(
                        RichText::new(labels::duration(labels::now_secs() - snap.ticket.created))
                            .font(theme::body(theme::TINY))
                            .color(pal.faint),
                    )
                    .on_hover_text(format!("创建于 {}", activity_time(snap.ticket.created)));
                }
                if !running.is_empty() {
                    let peers: Vec<String> = running
                        .iter()
                        .map(|(a, c)| {
                            if *c > 1 {
                                format!("{} ×{c}", agent_name(a))
                            } else {
                                agent_name(a)
                            }
                        })
                        .collect();
                    let partial = snap.total_jobs > snap.jobs.len() as i64;
                    let tip = format!(
                        "{} · {}",
                        if partial { "概览中执行的协作者" } else { "执行中的协作者" },
                        peers.join("、")
                    );
                    let n = running.len().min(4) as f32;
                    let (r, resp) =
                        ui.allocate_exact_size(vec2(n * 18.0 + (n - 1.0) * 3.0, 20.0), Sense::hover());
                    for (i, (a, _)) in running.iter().rev().take(4).enumerate() {
                        let x = r.right() - 9.0 - i as f32 * 21.0;
                        widgets::avatar_at(ui, pos2(x, r.center().y), 9.0, a, &pal);
                    }
                    resp.on_hover_text(tip);
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let w = ui.available_width();
                    let hg = widgets::line_galley(ui, &head, theme::strong(theme::SMALL), pal.ink, w);
                    ui.add(Label::new(hg).selectable(false));
                    if !pause_label.is_empty() {
                        let text = if !writable && pause.as_deref() == Some("paused") {
                            format!("{pause_label} · 需在主工作台恢复")
                        } else {
                            pause_label.to_string()
                        };
                        ui.label(
                            RichText::new(text)
                                .font(theme::body(theme::SMALL))
                                .color(pal.wait.fg),
                        )
                        .on_hover_text(pause_hint);
                    }
                    let w = ui.available_width();
                    if !detail.is_empty() && w > 40.0 {
                        let dg = widgets::line_galley(ui, &detail, theme::body(theme::SMALL), pal.mute, w);
                        let r = ui.add(Label::new(dg).selectable(false));
                        if ts > 0.0 {
                            r.on_hover_text(format!("最近活动 · {detail}\n{}", activity_time(ts)));
                        }
                    }
                });
            });
        });
        if let Some(action) = action {
            self.submit(action);
        }
    }

    fn feed_area(&mut self, ui: &mut Ui, snap: &Snapshot) {
        let pal = self.pal.clone();
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let ticket = view.id;
        let at_top = self.opts.at_top;
        let jump = std::mem::take(&mut self.scroll_to_bottom) && !at_top;
        let feed = view.feed.clone();
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
                ui.add_space(12.0);
                centered(ui, COLUMN, 24.0, |ui| {
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
                    for item in &feed.items {
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
        widgets::scroll_fades(ui, &out, pal.surface);
        let max = (out.content_size.y - out.inner_rect.height()).max(0.0);
        let at_bottom = out.state.offset.y >= max - 48.0;
        if !at_bottom && !jump {
            let size = vec2(108.0, 32.0);
            let rect =
                Rect::from_center_size(pos2(area.center().x, out.inner_rect.bottom() - 28.0), size);
            let resp = ui.interact(rect, ui.id().with("jump-latest"), Sense::click());
            let p = ui.painter();
            p.rect_filled(
                rect.translate(vec2(0.0, 2.0)).expand(1.0),
                CornerRadius::same(17),
                egui::Color32::from_black_alpha(if pal.dark { 60 } else { 16 }),
            );
            p.rect(
                rect,
                CornerRadius::same(16),
                if resp.hovered() { pal.surface_alt } else { pal.surface },
                Stroke::new(1.0_f32, pal.line),
                egui::StrokeKind::Inside,
            );
            p.text(
                rect.center(),
                Align2::CENTER_CENTER,
                format!("{}  跳到最新", icon::ARROW_DOWN),
                theme::body(theme::SMALL),
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

    /// Read-only windows show what the conversation is instead of an input.
    fn footer(&self, ui: &mut Ui) {
        let pal = &self.pal;
        let snap = self.view.as_ref().and_then(|v| v.snap.as_ref());
        widgets::well(pal)
            .inner_margin(Margin {
                left: 14,
                right: 14,
                top: 10,
                bottom: 10,
            })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.label(RichText::new(icon::LOCK_SIMPLE).font(theme::body(14.0)).color(pal.mute));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        // Project details only when there is room for them.
                        if let Some(s) = snap.filter(|_| ui.available_width() > 560.0) {
                            let meta = format!(
                                "{} {}　主控 · {}　{}",
                                icon::FOLDER_SIMPLE,
                                labels::basename(&s.ticket.cwd),
                                agent_name(&s.ticket.lead),
                                labels::profile(&s.ticket.permission_profile)
                            );
                            ui.label(RichText::new(meta).font(theme::body(theme::TINY)).color(pal.faint))
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
                            let g = widgets::line_galley(ui, text, theme::body(theme::UI), pal.mute, ui.available_width());
                            ui.add(Label::new(g).selectable(false)).on_hover_text(
                                "此窗口只读取工单日志：不创建工单、不发送消息、不处理审批，也不调用模型。",
                            );
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
        (true, true) => 2.0,
        _ if matches!(item, Item::User { .. }) => 28.0,
        (true, false) | (false, true) => 14.0,
        _ => 22.0,
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
                    widgets::avatar(ui, agent, 22.0, pal);
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new(agent_name(agent))
                            .font(theme::strong(13.5))
                            .color(pal.ink),
                    );
                    let part = if *peer {
                        if role.is_empty() {
                            "协作回报".to_string()
                        } else {
                            format!("协作回报 · {}", labels::compact(role, 30))
                        }
                    } else if *latest_result {
                        "最近回报".to_string()
                    } else {
                        "主控".to_string()
                    };
                    let mut meta = vec![part];
                    if let Some(j) = job {
                        meta.push(format!("#{j}"));
                    }
                    ui.label(
                        RichText::new(meta.join("  "))
                            .font(theme::body(theme::TINY))
                            .color(pal.mute),
                    );
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
                // A collaborator's report: set off by a lane in that agent's colour.
                let resp = Frame::new()
                    .inner_margin(Margin {
                        left: 14,
                        right: 0,
                        top: 2,
                        bottom: 4,
                    })
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        header(ui);
                        body(ui);
                    });
                widgets::left_bar(ui, resp.response.rect, pal.agent_color(agent));
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
                    ui.label(RichText::new(ic).font(theme::body(15.0)).color(tone.fg));
                    ui.label(
                        RichText::new(feed::alert_title(kind, agent))
                            .font(theme::strong(theme::UI))
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
                        size: theme::UI,
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
                            .font(theme::body(15.0))
                            .color(tone.fg),
                    );
                    let title = if open { "待审批" } else { "审批请求" };
                    ui.label(
                        RichText::new(title)
                            .font(theme::strong(theme::UI))
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
                        .font(theme::body(theme::UI))
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
            widgets::well(pal).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.label(
                        RichText::new(format!("{}  阶段汇报", icon::FLAG))
                            .font(theme::strong(theme::SMALL))
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
        }
        Item::Progress { key, rows } => progress_group(ui, *key, rows, snap, pal, open_groups),
    }
}

fn user_bubble(ui: &mut Ui, text: &str, ts: f64, follow: bool, pal: &Palette) {
    let col = ui.available_width();
    let max_w = (col * 0.8).min(620.0);
    let pad = vec2(14.0, 9.0);
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
            nw: 14,
            ne: 14,
            sw: 14,
            se: 4,
        },
        pal.user_bubble,
    );
    ui.put(rect.shrink2(pad), Label::new(galley).selectable(true));
    ui.add_space(pad.y + 5.0);
    let meta = format!("{} · {}", if follow { "追加指令" } else { "你" }, stamp(ts));
    let g = widgets::line_galley(ui, &meta, theme::body(theme::TINY), pal.faint, col);
    let (r, resp) = ui.allocate_exact_size(vec2(col, g.size().y), Sense::hover());
    ui.painter()
        .galley(pos2(r.right() - g.size().x - 2.0, r.top()), g, pal.faint);
    resp.on_hover_text(activity_time(ts));
}

/// Control lines of a reply (report, next step, lesson …) as a quiet block.
fn controls_box(ui: &mut Ui, controls: &[Control], pal: &Palette) {
    widgets::well(pal)
        .inner_margin(Margin {
            left: 10,
            right: 12,
            top: 5,
            bottom: 5,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            for c in controls {
                if matches!(c.kind, ControlKind::ReportDone | ControlKind::ReportPartial | ControlKind::ReportBlocked | ControlKind::Ask | ControlKind::CheckPass | ControlKind::CheckFail) {
                    let tone = match c.kind {
                        ControlKind::ReportDone | ControlKind::CheckPass => pal.ok,
                        _ => pal.warn,
                    };
                    ui.add_space(5.0);
                    if !c.title.is_empty() {
                        ui.label(RichText::new(&c.title).font(theme::strong(theme::SMALL)).color(tone.fg));
                    }
                    if !c.text.is_empty() {
                        ui.add(Label::new(RichText::new(&c.text).font(theme::body(theme::SMALL)).color(if matches!(c.kind, ControlKind::CheckPass | ControlKind::CheckFail) { tone.fg } else { pal.ink })).wrap().selectable(true));
                    }
                    ui.add_space(5.0);
                    continue;
                }
                let (rect, resp) =
                    ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
                let p = ui.painter();
                let y = rect.center().y;
                let color = match c.kind {
                    ControlKind::Blocked => pal.fail.fg,
                    _ => pal.mute,
                };
                p.text(
                    pos2(rect.left() + 8.0, y),
                    Align2::CENTER_CENTER,
                    control_icon(c.kind),
                    theme::body(13.5),
                    color,
                );
                let mut x = rect.left() + 24.0;
                if let Some(a) = &c.agent {
                    widgets::avatar_at(ui, pos2(x + 8.0, y), 8.0, a, pal);
                    x += 22.0;
                }
                let tg = widgets::line_galley(ui, &c.title, theme::strong(theme::SMALL), pal.ink, 200.0);
                let tw = tg.size().x;
                p.galley(pos2(x, y - tg.size().y / 2.0), tg, pal.ink);
                x += tw + 10.0;
                if !c.text.is_empty() {
                    let g = widgets::line_galley(
                        ui,
                        &c.text,
                        theme::body(theme::SMALL),
                        pal.mute,
                        rect.right() - x,
                    );
                    p.galley(pos2(x, y - g.size().y / 2.0), g, pal.mute);
                    resp.on_hover_ui(|ui| {
                        ui.label(RichText::new(&c.title).font(theme::strong(theme::SMALL)));
                        ui.label(RichText::new(&c.text).font(theme::body(theme::UI)));
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
        pos2(rect.left() + 11.0, y),
        Align2::CENTER_CENTER,
        ic,
        theme::body(13.5),
        if tone == Tone::Error { pal.fail.fg } else { pal.faint },
    );
    let time = stamp(ts);
    let tg = widgets::line_galley(ui, &time, theme::body(theme::TINY), pal.faint, 80.0);
    let right = rect.right() - tg.size().x;
    p.galley(pos2(right, y - tg.size().y / 2.0), tg, pal.faint);
    let mut x = rect.left() + GUTTER;
    if let (LineKind::Send, Some(from), Some(to)) = (kind, agent, to) {
        widgets::avatar_at(ui, pos2(x + 8.0, y), 8.0, from, pal);
        x += 22.0;
        let g = widgets::line_galley(ui, &agent_name(from), theme::strong(theme::SMALL), pal.ink, 160.0);
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
        x += 22.0;
        let g = widgets::line_galley(ui, &agent_name(to), theme::strong(theme::SMALL), pal.ink, 160.0);
        let w = g.size().x;
        p.galley(pos2(x, y - g.size().y / 2.0), g, pal.ink);
        x += w + 10.0;
    }
    let g = widgets::line_galley(ui, text, theme::body(theme::SMALL), color, right - x - 12.0);
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
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(
            rect.expand2(vec2(4.0, 0.0)),
            CornerRadius::same(theme::CONTROL_RADIUS),
            pal.hover,
        );
    }
    let y = rect.center().y;
    p.text(
        pos2(rect.left() + 11.0, y),
        Align2::CENTER_CENTER,
        if open {
            icon::CARET_DOWN
        } else {
            icon::CARET_RIGHT
        },
        theme::body(11.0),
        pal.faint,
    );
    let summary = feed::group_summary(rows);
    let last_state = rows.last().map(|r| r.state.as_str()).unwrap_or("");
    let failed = rows
        .iter()
        .filter(|r| matches!(r.state.as_str(), "failed" | "error"))
        .count();
    let x = rect.left() + GUTTER;
    let mut right = rect.right();
    if failed > 0 {
        let fg = widgets::line_galley(
            ui,
            &format!("{failed} 条失败"),
            theme::body(theme::TINY),
            pal.fail.fg,
            120.0,
        );
        right -= fg.size().x;
        p.galley(pos2(right, y - fg.size().y / 2.0), fg, pal.fail.fg);
        right -= 8.0;
    }
    let color = if last_state == "failed" {
        pal.fail.fg
    } else {
        pal.mute
    };
    let g = widgets::line_galley(ui, &summary, theme::body(theme::SMALL), color, right - x - 8.0);
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
    ui.add_space(2.0);
    let resp = Frame::new()
        .inner_margin(Margin {
            left: GUTTER as i8,
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
        Rect::from_min_size(pos2(rect.left() + 10.5, rr.top()), vec2(1.0, rr.height())),
        0.0,
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
        right -= sg.size().x;
        p.galley(pos2(right, y - sg.size().y / 2.0), sg, tone.fg);
        right -= 10.0;
    }
    let lg = widgets::line_galley(ui, &r.label, theme::body(theme::UI), pal.ink, right - x - 24.0);
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

/// Clickable starter: icon, title and what it sets up, on one line.
fn starter_row(ui: &mut Ui, ic: &str, title: &str, text: &str, pal: &Palette) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, title));
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool(resp.id.with("hover"), resp.hovered());
        let p = ui.painter();
        if t > 0.0 {
            p.rect_filled(rect, CornerRadius::same(theme::CONTROL_RADIUS), pal.hover.gamma_multiply(t));
        }
        let y = rect.center().y;
        p.text(pos2(rect.left() + 18.0, y), Align2::CENTER_CENTER, ic, theme::body(16.0), pal.mute);
        let tg = widgets::line_galley(ui, title, theme::body(13.5), pal.ink, 110.0);
        p.galley(pos2(rect.left() + 40.0, y - tg.size().y / 2.0), tg, pal.ink);
        let x = rect.left() + 140.0;
        let sg = widgets::line_galley(ui, text, theme::body(theme::UI), pal.mute, rect.right() - 12.0 - x);
        p.galley(pos2(x, y - sg.size().y / 2.0), sg, pal.mute);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
