//! Composer and operator actions: new conversations, follow-ups, directed
//! notes, stop, approvals, retries and plan items (the write paths of
//! `desktop/ui/studio.js` and `desktop/ui/panels.js`).
//!
//! Actions go to the scheduler [`Host`](crate::host::Host) and run in
//! submission order; outcomes come back through [`App::poll_host`].

use crate::app::App;
use crate::host::{self, Action, ActionKind};
use crate::labels::{self, agent_name};
use crate::theme::{self, Palette};
use crate::widgets;
use crate::worker::Cmd;
use egui::{
    Align, Align2, Color32, CornerRadius, Event, Frame, Key, KeyboardShortcut, Label, Layout,
    Margin, Modifiers, Response, RichText, Sense, Stroke, Ui, Vec2,
};
use egui_phosphor::regular as icon;
use std::path::PathBuf;

/// Lead choices, in the order of the Electron picker.
pub const LEADS: [&str; 4] = ["deepseek", "codex", "claude", "antigravity"];
pub const PROFILES: [&str; 5] = ["readonly", "ask", "partial", "full", "auto"];
const MAX_ATTACHMENTS: usize = 16;

const RISK_PREFIX: &str = "参与者可用当前系统用户权限读取凭据、修改或删除项目外文件、运行命令并对外发送数据。";
const RISK_AUTO: &str = "自动模式不设累计轮数上限，会在阶段完成后继续搜寻和调用模型，可能反复产生费用。";
const RISK_FULL: &str = "本次会话不会逐项询问审批。";
const RISK_SUFFIX: &str = "仅在信任的项目和环境中授予此权限。软件按原样提供，在适用法律允许的范围内不提供担保。";

pub enum Confirm {
    /// Switch the draft permission to `full` or `auto`.
    Profile(String),
    /// Resume a stopped continuous ticket with a new instruction.
    Resume { ticket: i64, body: String },
}

pub struct Composer {
    pub draft: String,
    pub project: Option<PathBuf>,
    pub lead: String,
    pub profile: String,
    pub attachments: Vec<String>,
    /// Directed note target: `(job, agent)`.
    pub target: Option<(i64, String)>,
    /// Actions submitted and not yet answered.
    pub pending: usize,
    /// Last operator message: `(text, is_error)`.
    pub message: Option<(String, bool)>,
    pub confirm: Option<Confirm>,
    pub plan_draft: String,
    /// Explicit routes for the next new conversation; `None` follows
    /// `route_defaults`.
    pub routes: Option<serde_json::Value>,
    /// MCP/skill grants for the next new conversation; `None` follows the
    /// global defaults.
    pub extensions: Option<serde_json::Value>,
    /// Body of the last submitted create/follow/note (cleared on success).
    last_body: Option<String>,
    last_plan: Option<String>,
    pub focus: bool,
    /// When the current message first appeared (success notices fade out).
    shown: Option<(String, std::time::Instant)>,
}

impl Composer {
    pub fn new(project: Option<PathBuf>, lead: Option<String>) -> Self {
        Self {
            draft: String::new(),
            project,
            lead: lead.filter(|l| LEADS.contains(&l.as_str())).unwrap_or_else(|| "deepseek".into()),
            profile: "readonly".into(),
            attachments: Vec::new(),
            target: None,
            pending: 0,
            message: None,
            confirm: None,
            plan_draft: String::new(),
            routes: None,
            extensions: None,
            last_body: None,
            last_plan: None,
            focus: false,
            shown: None,
        }
    }
}

/// What the selected ticket allows right now.
struct Current {
    ticket: Option<i64>,
    /// Snapshot loaded (or no ticket selected).
    loaded: bool,
    state: String,
    profile: String,
    lead: String,
    cwd: String,
    target_active: bool,
}

impl Current {
    fn in_flight(&self) -> bool {
        self.ticket.is_some() && labels::is_active(&self.state)
    }
    fn stopped(&self) -> bool {
        self.profile == "auto" && self.state == "cancelled"
    }
}

fn round_button(ui: &mut Ui, ic: &str, label: &str, fill: Color32, fg: Color32, enabled: bool) -> Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(34.0), sense);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    if ui.is_rect_visible(rect) {
        let fill = if !enabled {
            fill.gamma_multiply(0.35)
        } else if resp.hovered() {
            fill.gamma_multiply(0.85)
        } else {
            fill
        };
        let p = ui.painter();
        p.circle_filled(rect.center(), 17.0, fill);
        p.text(rect.center(), Align2::CENTER_CENTER, ic, theme::body(17.0), fg);
    }
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// Removable chip; returns true when its close mark was clicked.
fn removable_chip(ui: &mut Ui, text: &str, pal: &Palette, hover: &str) -> bool {
    widgets::quiet_button(ui, icon::X, text, pal, true)
        .on_hover_text(hover)
        .clicked()
}

impl App {
    fn current(&self) -> Current {
        let ticket = self.view.as_ref().map(|v| v.id);
        let snap = self.view.as_ref().and_then(|v| v.snap.as_ref());
        let target_active = match (&self.composer.target, snap) {
            (Some((job, _)), Some(s)) => s
                .jobs
                .iter()
                .find(|j| j.id == *job)
                .is_none_or(|j| labels::is_active(&j.state)),
            _ => true,
        };
        Current {
            ticket,
            loaded: ticket.is_none() || snap.is_some(),
            state: snap.map(|s| s.ticket.state.clone()).unwrap_or_default(),
            profile: snap.map(|s| s.ticket.permission_profile.clone()).unwrap_or_default(),
            lead: snap.map(|s| s.ticket.lead.clone()).unwrap_or_default(),
            cwd: snap.map(|s| s.ticket.cwd.clone()).unwrap_or_default(),
            target_active,
        }
    }

    /// Whether this window schedules (and can write) the journal.
    pub fn writable(&self) -> bool {
        self.host.is_some()
    }

    pub fn submit(&mut self, action: Action) {
        let Some(host) = &self.host else {
            self.composer.message = Some(("只读观察模式不能修改工单".into(), true));
            return;
        };
        match &action {
            Action::Create { goal, .. } | Action::Follow { body: goal, .. } | Action::Note { body: goal, .. } => {
                self.composer.last_body = Some(goal.clone());
            }
            Action::PlanAdd { body, .. } => self.composer.last_plan = Some(body.clone()),
            _ => {}
        }
        if host.submit(action) {
            self.composer.pending += 1;
            self.composer.message = None;
        } else {
            self.composer.message = Some(("调度线程已退出，请重新打开窗口".into(), true));
        }
    }

    /// Start a new conversation in the composer.
    pub fn new_conversation(&mut self) {
        self.select(None);
        self.composer.target = None;
        self.composer.attachments.clear();
        self.composer.focus = true;
    }

    /// Direct the next message to one job of the selected ticket.
    pub fn set_target(&mut self, job: i64, agent: String) {
        self.composer.target = Some((job, agent));
        self.composer.focus = true;
    }

    /// Drain scheduler outcomes; called once per frame.
    pub fn poll_host(&mut self) {
        let Some(host) = &self.host else { return };
        let outcomes: Vec<_> = host.rx.try_iter().collect();
        let catalogs: Vec<_> = host.catalogs.try_iter().collect();
        let probes: Vec<_> = host.probes.try_iter().collect();
        for c in catalogs {
            if let Some(d) = self.models.as_mut() {
                d.answer(&c.agent, c.result.clone());
            }
            if let Some(d) = self.providers.as_mut() {
                d.answer(&c.agent, c.result);
            }
        }
        if let Some(d) = self.providers.as_mut() {
            for p in probes {
                d.probed(p);
            }
        }
        if outcomes.is_empty() {
            return;
        }
        for o in outcomes {
            self.composer.pending = self.composer.pending.saturating_sub(1);
            let ok = o.result.is_ok();
            let sent = ok && matches!(o.kind, ActionKind::Create | ActionKind::Follow | ActionKind::Note);
            if sent && self.composer.last_body.as_deref() == Some(self.composer.draft.as_str()) {
                self.composer.draft.clear();
            }
            match (o.kind, o.result) {
                (ActionKind::Create, Ok(id)) => {
                    self.composer.attachments.clear();
                    if let Some(id) = id {
                        self.select(Some(id));
                    }
                }
                (ActionKind::Note, Ok(_)) => {
                    self.composer.target = None;
                    self.composer.message = Some(("定向留言已提交。".into(), false));
                }
                (ActionKind::Cancel, Ok(_)) => {
                    self.composer.message = Some(("已停止此工单的后续调度。".into(), false));
                }
                (ActionKind::Retry, Ok(_)) => {
                    self.composer.message = Some(("已重新排队该任务。".into(), false));
                }
                (ActionKind::Decide { allow }, Ok(_)) => {
                    let text = if allow { "已允许本次请求。" } else { "已拒绝本次请求。" };
                    self.composer.message = Some((text.into(), false));
                }
                (ActionKind::Plan, Ok(_)) => {
                    if self.composer.last_plan.as_deref() == Some(self.composer.plan_draft.as_str()) {
                        self.composer.plan_draft.clear();
                    }
                }
                (ActionKind::Follow, Ok(_)) => {}
                (ActionKind::Routes, r) => self.routes_saved(r.map(|_| ())),
                (ActionKind::Knowledge, r) => self.knowledge_saved(r.map(|_| ())),
                (ActionKind::Providers, r) => self.providers_saved(r.map(|_| ())),
                (ActionKind::Meta, Ok(_)) => {}
                (_, Err(e)) => self.composer.message = Some((e, true)),
            }
        }
        let _ = self.worker.tx.send(Cmd::Refresh);
    }

    fn send(&mut self, cur: &Current) {
        let body = self.composer.draft.clone();
        if body.trim().is_empty() || self.composer.pending > 0 || !cur.loaded {
            return;
        }
        let action = match cur.ticket {
            None => Action::Create {
                goal: body,
                cwd: self.composer.project.clone(),
                lead: self.composer.lead.clone(),
                profile: self.composer.profile.clone(),
                files: self.composer.attachments.clone(),
                selections: self.composer.routes.clone(),
                extensions: self.composer.extensions.clone(),
            },
            Some(ticket) => match &self.composer.target {
                Some(_) if !cur.target_active => return,
                Some((job, _)) => Action::Note {
                    ticket,
                    job: *job,
                    body,
                },
                None if cur.stopped() => {
                    self.composer.confirm = Some(Confirm::Resume { ticket, body });
                    return;
                }
                None => Action::Follow {
                    ticket,
                    body,
                    resume: false,
                },
            },
        };
        self.submit(action);
    }

    pub(crate) fn pick_project(&mut self) {
        let mut dialog = rfd::FileDialog::new().set_title("选择项目目录");
        if let Some(p) = &self.composer.project {
            dialog = dialog.set_directory(p);
        }
        if let Some(dir) = dialog.pick_folder() {
            match sa_engine::services::realpath(&dir) {
                Ok(real) if real.is_dir() => {
                    if self.composer.project.as_ref() != Some(&real) {
                        self.composer.attachments.clear();
                    }
                    self.composer.project = Some(real);
                }
                _ => self.composer.message = Some(("项目目录不存在".into(), true)),
            }
        }
    }

    pub(crate) fn pick_attachments(&mut self) {
        let Some(project) = self.composer.project.clone() else { return };
        let Some(files) = rfd::FileDialog::new()
            .set_title("选择要附加的项目文件")
            .set_directory(&project)
            .pick_files()
        else {
            return;
        };
        match host::relative_files(&project, &files) {
            Ok(rel) => {
                for r in rel {
                    if !self.composer.attachments.contains(&r) {
                        self.composer.attachments.push(r);
                    }
                }
                if self.composer.attachments.len() > MAX_ATTACHMENTS {
                    self.composer.attachments.truncate(MAX_ATTACHMENTS);
                    self.composer.message = Some(("单次最多附加 16 个文件".into(), true));
                }
            }
            Err(e) => self.composer.message = Some((format!("{e:#}"), true)),
        }
    }

    /// The input area at the bottom of the conversation column.
    pub fn composer(&mut self, ui: &mut Ui) {
        let pal = self.pal.clone();
        let cur = self.current();
        let fixed = cur.ticket.is_some();
        let in_flight = cur.in_flight();
        let busy = self.composer.pending > 0;
        let note_only = self.composer.target.is_some() && !cur.target_active;

        if let Some((text, error)) = self.composer.message.clone() {
            let since = match &self.composer.shown {
                Some((t, at)) if *t == text => *at,
                _ => {
                    let now = std::time::Instant::now();
                    self.composer.shown = Some((text.clone(), now));
                    now
                }
            };
            if !error {
                let left = std::time::Duration::from_secs(6).saturating_sub(since.elapsed());
                if left.is_zero() {
                    self.composer.message = None;
                } else {
                    ui.ctx().request_repaint_after(left);
                }
            }
        }
        if let Some((text, error)) = self.composer.message.clone() {
            ui.horizontal(|ui| {
                ui.add_space(6.0);
                let (ic, color) = if error {
                    (icon::WARNING_CIRCLE, pal.fail.fg)
                } else {
                    (icon::CHECK_CIRCLE, pal.ok.fg)
                };
                ui.label(RichText::new(ic).font(theme::body(14.0)).color(color));
                ui.add(Label::new(RichText::new(text).font(theme::body(theme::SMALL)).color(color)).wrap());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::icon_button(ui, icon::X, 20.0, pal.mute, pal.hover, false)
                        .on_hover_text("关闭提示")
                        .clicked()
                    {
                        self.composer.message = None;
                    }
                });
            });
            ui.add_space(6.0);
        }

        let mut send = false;
        let mut stop = false;
        let mut pick_project = false;
        let mut pick_files = false;
        let mut models = None;
        let mut exts = None;
        Frame::new()
            .fill(pal.surface)
            .stroke(Stroke::new(1.0_f32, pal.line))
            .corner_radius(CornerRadius::same(18))
            .shadow(egui::Shadow {
                offset: [0, 6],
                blur: 24,
                spread: 0,
                color: Color32::from_black_alpha(if pal.dark { 70 } else { 18 }),
            })
            .inner_margin(Margin { left: 16, right: 12, top: 12, bottom: 10 })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 6.0;

                let target = self.composer.target.clone();
                let has_chips = target.is_some() || (!fixed && !self.composer.attachments.is_empty());
                if has_chips {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        if let Some((job, agent)) = &target {
                            let text = format!("仅发给 {} #{job}", agent_name(agent));
                            if removable_chip(ui, &text, &pal, "改为发给主控") {
                                self.composer.target = None;
                            }
                        }
                        if !fixed {
                            let mut remove = None;
                            for (i, f) in self.composer.attachments.iter().enumerate() {
                                if removable_chip(ui, &labels::basename(f), &pal, &format!("移除 {f}")) {
                                    remove = Some(i);
                                }
                            }
                            if let Some(i) = remove {
                                self.composer.attachments.remove(i);
                            }
                        }
                    });
                }

                let hint = if target.is_some() {
                    "给这个执行者留言…"
                } else if cur.stopped() {
                    "输入要恢复的工作…"
                } else if fixed {
                    "继续这次对话，或补充新的要求…"
                } else {
                    "描述你的目标，Enter 发送，Shift+Enter 换行…"
                };
                let resp = egui::ScrollArea::vertical()
                    .id_salt("composer-scroll")
                    .max_height(180.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.composer.draft)
                                .id_salt("composer")
                                .frame(false)
                                .hint_text(RichText::new(hint).color(pal.faint))
                                .font(theme::body(14.0))
                                .desired_rows(2)
                                .desired_width(f32::INFINITY)
                                .lock_focus(true)
                                .return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter)),
                        )
                    })
                    .inner;
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "输入框"));
                if std::mem::take(&mut self.composer.focus) {
                    resp.request_focus();
                }
                let composing = ui.input(|i| i.events.iter().any(|e| matches!(e, Event::Ime(_))));
                if resp.has_focus()
                    && !composing
                    && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter))
                {
                    send = true;
                }

                let has_draft = !self.composer.draft.trim().is_empty();
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if fixed {
                        if cur.loaded {
                            widgets::chip(ui, &labels::profile(&cur.profile), pal.neutral, theme::TINY)
                                .on_hover_text("权限已锁定 · 新建对话可更改");
                            ui.label(
                                RichText::new(format!("主控 · {}", agent_name(&cur.lead)))
                                    .font(theme::body(theme::TINY + 0.5))
                                    .color(pal.mute),
                            );
                            let folder = format!("{} {}", icon::FOLDER_SIMPLE, labels::basename(&cur.cwd));
                            ui.label(RichText::new(folder).font(theme::body(theme::TINY + 0.5)).color(pal.mute))
                                .on_hover_text(&cur.cwd);
                            if let Some(t) = cur.ticket
                                && widgets::icon_button(ui, icon::SLIDERS_HORIZONTAL, 26.0, pal.mute, pal.hover, false)
                                    .on_hover_text("本次会话的模型与执行端")
                                    .clicked()
                            {
                                models = Some(crate::models::Scope::Ticket(t));
                            }
                            if let Some(t) = cur.ticket
                                && widgets::icon_button(ui, icon::PUZZLE_PIECE, 26.0, pal.mute, pal.hover, false)
                                    .on_hover_text("本次会话的 MCP 与技能")
                                    .clicked()
                            {
                                exts = Some(crate::extensions::ExtScope::Ticket(t));
                            }
                        }
                    } else {
                        let folder = self
                            .composer
                            .project
                            .as_ref()
                            .map(|p| labels::basename(&p.to_string_lossy()))
                            .unwrap_or_else(|| "选择项目".into());
                        let tip = self
                            .composer
                            .project
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "未选择时使用新的临时目录".into());
                        if widgets::quiet_button(ui, icon::FOLDER_SIMPLE, &folder, &pal, false)
                            .on_hover_text(tip)
                            .clicked()
                        {
                            pick_project = true;
                        }
                        let can_attach = self.composer.project.is_some() && !busy;
                        if ui
                            .add_enabled_ui(can_attach, |ui| {
                                widgets::icon_button(ui, icon::PAPERCLIP, 30.0, pal.mute, pal.hover, false)
                            })
                            .inner
                            .on_hover_text("附加项目文件")
                            .on_disabled_hover_text("先选择项目目录")
                            .clicked()
                        {
                            pick_files = true;
                        }
                        widgets::combo("lead")
                            .width(150.0)
                            .selected_text(format!("主控 · {}", agent_name(&self.composer.lead)))
                            .show_ui(ui, |ui| {
                                for a in LEADS {
                                    ui.selectable_value(&mut self.composer.lead, a.to_string(), agent_name(a));
                                }
                            });
                        let mut next = self.composer.profile.clone();
                        widgets::combo("profile")
                            .width(118.0)
                            .selected_text(labels::profile(&self.composer.profile))
                            .show_ui(ui, |ui| {
                                for p in PROFILES {
                                    ui.selectable_value(&mut next, p.to_string(), labels::profile(p));
                                }
                            });
                        let tip = App::routes_summary(self.composer.routes.as_ref());
                        if widgets::icon_button(ui, icon::SLIDERS_HORIZONTAL, 30.0, pal.mute, pal.hover, self.composer.routes.is_some())
                            .on_hover_text(format!("模型与执行端
{tip}"))
                            .clicked()
                        {
                            models = Some(crate::models::Scope::Draft);
                        }
                        if widgets::icon_button(ui, icon::PUZZLE_PIECE, 30.0, pal.mute, pal.hover, self.composer.extensions.is_some())
                            .on_hover_text("本次新会话的 MCP 与技能")
                            .clicked()
                        {
                            exts = Some(crate::extensions::ExtScope::Draft);
                        }
                        if next != self.composer.profile {
                            if matches!(next.as_str(), "full" | "auto") {
                                self.composer.confirm = Some(Confirm::Profile(next));
                            } else {
                                self.composer.profile = next;
                            }
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        if in_flight {
                            if round_button(ui, icon::STOP, "停止工单", pal.fail.fg, pal.surface, !busy)
                                .on_hover_text("停止工单")
                                .clicked()
                            {
                                stop = true;
                            }
                            if has_draft {
                                let text = if target.is_some() { "定向留言" } else { "追加" };
                                if ui
                                    .add_enabled_ui(!busy && !note_only, |ui| {
                                        widgets::quiet_button(ui, icon::CHAT_CIRCLE_DOTS, text, &pal, false)
                                    })
                                    .inner
                                    .clicked()
                                {
                                    send = true;
                                }
                            }
                        } else {
                            let enabled = has_draft && !busy && !note_only && cur.loaded;
                            let tip = if cur.stopped() { "恢复并发送" } else { "发送" };
                            if round_button(ui, icon::ARROW_UP, tip, pal.accent, pal.surface, enabled)
                                .on_hover_text(tip)
                                .clicked()
                            {
                                send = true;
                            }
                        }
                        if busy {
                            ui.spinner();
                        } else if ui.available_width() > 150.0 {
                            let text = if note_only {
                                "任务已停止 · 留言草稿已保留"
                            } else if in_flight {
                                "Enter 追加指令 · Shift+Enter 换行"
                            } else {
                                "Enter 发送 · Shift+Enter 换行"
                            };
                            let g = widgets::line_galley(ui, text, theme::body(theme::TINY), pal.faint, ui.available_width());
                            ui.add(Label::new(g).selectable(false));
                        }
                    });
                });
            });
        if pick_project {
            self.pick_project();
        }
        if pick_files {
            self.pick_attachments();
        }
        if let Some(scope) = models {
            self.open_models(scope);
        }
        if let Some(scope) = exts {
            self.open_extensions(scope);
        }
        if stop && let Some(ticket) = cur.ticket {
            self.submit(Action::Cancel { ticket });
        } else if send {
            self.send(&cur);
        }
    }

    /// High-risk permission and resume confirmations.
    pub fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(confirm) = &self.composer.confirm else { return };
        let pal = self.pal.clone();
        let (title, text, ok) = match confirm {
            Confirm::Profile(p) => (
                format!("确认高风险授权 · {}", labels::profile(p)),
                format!(
                    "{RISK_PREFIX}{}{RISK_SUFFIX}",
                    if p == "auto" { RISK_AUTO } else { RISK_FULL }
                ),
                "了解风险并启用",
            ),
            Confirm::Resume { .. } => (
                "恢复持续自动运行".to_string(),
                "将以这条新指令恢复完全访问和持续模型调用，可能重复产生费用。".to_string(),
                "恢复并发送",
            ),
        };
        let mut decision = None;
        let modal = egui::Modal::new(egui::Id::new("sa-confirm"))
            .frame(
                Frame::new()
                    .fill(pal.surface)
                    .stroke(Stroke::new(1.0_f32, pal.line))
                    .corner_radius(CornerRadius::same(14))
                    .inner_margin(Margin::same(20)),
            )
            .show(ctx, |ui| {
                ui.set_max_width(440.0);
                ui.label(
                    RichText::new(format!("{}  {title}", icon::SHIELD_WARNING))
                        .font(theme::strong(16.0))
                        .color(pal.warn.fg),
                );
                ui.add_space(10.0);
                ui.add(Label::new(RichText::new(text).font(theme::body(13.5)).color(pal.ink)).wrap());
                ui.add_space(16.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if widgets::quiet_button(ui, icon::SHIELD_WARNING, ok, &pal, true).clicked() {
                        decision = Some(true);
                    }
                    if widgets::quiet_button(ui, "", "取消", &pal, false).clicked() {
                        decision = Some(false);
                    }
                });
            });
        if modal.should_close() && decision.is_none() {
            decision = Some(false);
        }
        let Some(yes) = decision else { return };
        let Some(confirm) = self.composer.confirm.take() else { return };
        if !yes {
            return;
        }
        match confirm {
            Confirm::Profile(p) => self.composer.profile = p,
            Confirm::Resume { ticket, body } => self.submit(Action::Follow {
                ticket,
                body,
                resume: true,
            }),
        }
    }
}
