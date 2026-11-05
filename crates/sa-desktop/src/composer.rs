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
    Resume { ticket: i64, body: String, files: Vec<String> },
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
    /// None follows the saved policy for new conversations.
    pub allow_lead_routing: Option<bool>,
    /// MCP/skill grants for the next new conversation; `None` follows the
    /// global defaults.
    pub extensions: Option<serde_json::Value>,
    /// Body of the last submitted create/follow/note (cleared on success).
    last_body: Option<String>,
    last_attachments: Option<Vec<String>>,
    last_create_context: Option<(u64, Option<PathBuf>)>,
    last_plan: Option<String>,
    pub focus: bool,
    /// When the current message first appeared (success notices fade out).
    shown: Option<(String, std::time::Instant)>,
    /// File index of the project being mentioned with `@`.
    pub index: Option<FileIndex>,
    /// Highlighted row of the `@` suggestions.
    pub mention_row: usize,
    /// IME composition persists across frames without new IME events.
    ime_active: bool,
}

pub struct FileIndex {
    pub root: PathBuf,
    pub files: Option<Result<Vec<String>, String>>,
    pending: Option<crate::ops::Pending<Vec<String>>>,
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
            allow_lead_routing: None,
            extensions: None,
            last_body: None,
            last_attachments: None,
            last_create_context: None,
            last_plan: None,
            focus: false,
            shown: None,
            index: None,
            mention_row: 0,
            ime_active: false,
        }
    }

    /// TextEdit's IME state is private in egui 0.33. Keep the shortcut gate
    /// across frames too, and leave the commit/cancel frame to the editor.
    fn keyboard_ready(&mut self, ui: &Ui, editor: egui::Id) -> bool {
        let focused = ui.is_enabled()
            && ui.input(|i| i.focused)
            && ui.memory(|m| m.has_focus(editor) && m.allows_interaction(ui.layer_id()));
        if !focused {
            self.ime_active = false;
            return false;
        }
        let mut composing = self.ime_active;
        ui.input(|i| {
            for event in &i.events {
                if let Event::Ime(ime) = event {
                    composing = true;
                    match ime {
                        egui::ImeEvent::Enabled | egui::ImeEvent::Preedit(_) => self.ime_active = true,
                        egui::ImeEvent::Commit(text) if text == "\n" || text == "\r" => {}
                        egui::ImeEvent::Commit(_) | egui::ImeEvent::Disabled => self.ime_active = false,
                    }
                }
            }
        });
        !composing
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
        if let Action::Create { files, .. } | Action::Follow { files, .. } = &action {
            self.composer.last_attachments = Some(files.clone());
        }
        if let Action::Create { cwd, .. } = &action {
            self.composer.last_create_context = Some((self.epoch, cwd.clone()));
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
                d.answer_for(&c.agent, c.request_id, c.result.clone());
            }
            if let Some(d) = self.providers.as_mut() {
                d.answer_for(&c.agent, c.request_id, c.result);
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
            let same_create_context = self.composer.last_create_context.as_ref()
                .is_some_and(|(epoch, project)| *epoch == self.epoch && project == &self.composer.project);
            let sent = ok && matches!(o.kind, ActionKind::Create | ActionKind::Follow | ActionKind::Note)
                && (o.kind != ActionKind::Create || same_create_context);
            if sent && self.composer.last_body.as_deref() == Some(self.composer.draft.as_str()) {
                self.composer.draft.clear();
            }
            if sent && matches!(o.kind, ActionKind::Create | ActionKind::Follow)
                && self.composer.last_attachments.as_ref() == Some(&self.composer.attachments)
            {
                self.composer.attachments.clear();
            }
            match (o.kind, o.result) {
                (ActionKind::Create, Ok(id)) => {
                    if let Some(id) = id && same_create_context {
                        let unsent = std::mem::take(&mut self.composer.attachments);
                        self.select(Some(id));
                        self.composer.attachments = unsent;
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
        if self.composer.target.is_some() && !self.composer.attachments.is_empty() {
            self.composer.message = Some(("请将附件发给主控，或先移除附件再发送定向留言".into(), true));
            return;
        }
        let lead = if cur.ticket.is_some() { &cur.lead } else { &self.composer.lead };
        if self.composer.attachments.iter().any(|path| sa_engine::images::is_image_name(path))
            && !sa_engine::images::supports_native_images(lead)
        {
            self.composer.message = Some((sa_engine::images::UNSUPPORTED.into(), true));
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
                allow_lead_routing: self.composer.allow_lead_routing,
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
                    self.composer.confirm = Some(Confirm::Resume { ticket, body, files: self.composer.attachments.clone() });
                    return;
                }
                None => Action::Follow {
                    ticket,
                    body,
                    resume: false,
                    files: self.composer.attachments.clone(),
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

    /// Start (or reuse) the `@` file index of `root`.
    fn ensure_index(&mut self, ctx: &egui::Context, root: &std::path::Path) {
        let idx = self.composer.index.get_or_insert_with(|| FileIndex {
            root: PathBuf::new(),
            files: None,
            pending: None,
        });
        if let Some(r) = idx.pending.as_ref().and_then(crate::ops::Pending::poll) {
            idx.pending = None;
            idx.files = Some(r);
        }
        if idx.root != root && idx.pending.is_none() {
            idx.root = root.to_path_buf();
            idx.files = None;
            let dir = root.to_path_buf();
            idx.pending = Some(crate::ops::Pending::spawn(ctx, move || crate::files::scan(&dir)));
        }
    }

    /// Ctrl+V with an image on the clipboard: save it into the project and
    /// attach it to the next message.
    fn paste_image(&mut self, root: Option<&std::path::Path>) {
        let Some(root) = root else {
            if arboard::Clipboard::new().is_ok_and(|mut c| c.get_image().is_ok()) {
                self.composer.message = Some(("粘贴图片前请先选择项目目录".into(), true));
            }
            return;
        };
        match crate::files::paste_image(root) {
            Ok(Some(rel)) => {
                if self.composer.attachments.len() < MAX_ATTACHMENTS {
                    self.composer.attachments.push(rel.clone());
                } else {
                    self.composer.message = Some((format!("图片已保存为 {rel}，单次最多附加 16 个文件"), true));
                    return;
                }
                self.composer.message = Some((format!("已保存粘贴的图片：{rel}"), false));
            }
            Ok(None) => {}
            Err(e) => self.composer.message = Some((format!("{e:#}"), true)),
        }
    }

    /// Files dropped on the window: a folder becomes the draft's project,
    /// files inside the project are attached.
    pub fn take_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if dropped.is_empty() || !self.writable() {
            return;
        }
        if dropped.len() == 1 && dropped[0].is_dir() {
            if self.view.is_some() {
                self.composer.message = Some(("更换项目目录请新建对话".into(), true));
                return;
            }
            if let Ok(real) = sa_engine::services::realpath(&dropped[0]) {
                if self.composer.project.as_ref() != Some(&real) {
                    self.composer.attachments.clear();
                }
                self.composer.project = Some(real);
            }
            return;
        }
        let Some(project) = self.attachment_project() else {
            self.composer.message = Some(("先选择项目目录，或把项目文件夹拖进窗口".into(), true));
            return;
        };
        match host::relative_files(&project, &dropped) {
            Ok(rel) => {
                for r in rel {
                    if !self.composer.attachments.contains(&r) && self.composer.attachments.len() < MAX_ATTACHMENTS {
                        self.composer.attachments.push(r);
                    }
                }
            }
            Err(e) => self.composer.message = Some((format!("{e:#}"), true)),
        }
    }

    fn attachment_project(&self) -> Option<PathBuf> {
        match &self.view {
            Some(view) => view.snap.as_ref().map(|snapshot| PathBuf::from(&snapshot.ticket.cwd)),
            None => self.composer.project.clone(),
        }
    }

    pub(crate) fn pick_attachments(&mut self) {
        let Some(project) = self.attachment_project() else { return };
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
                let has_chips = target.is_some() || !self.composer.attachments.is_empty();
                if has_chips {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        if let Some((job, agent)) = &target {
                            let text = format!("仅发给 {} #{job}", agent_name(agent));
                            if removable_chip(ui, &text, &pal, "改为发给主控") {
                                self.composer.target = None;
                            }
                        }
                        if !self.composer.attachments.is_empty() {
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

                // `@` mentions: pick project files (attached to a new
                // conversation, or inserted as a path in a follow-up).
                let editor_id = ui.make_persistent_id("composer");
                let keyboard_ready = self.composer.keyboard_ready(ui, editor_id);
                let root = if fixed {
                    (!cur.cwd.is_empty()).then(|| PathBuf::from(&cur.cwd))
                } else {
                    self.composer.project.clone()
                };
                let mention = crate::files::mention(&self.composer.draft).map(|(at, q)| (at, q.to_string()));
                if let (Some((at, q)), Some(root)) = (&mention, &root) {
                    self.ensure_index(ui.ctx(), root);
                    let hits: Vec<String> = match self.composer.index.as_ref().and_then(|i| i.files.as_ref()) {
                        Some(Ok(files)) => crate::files::search(files, q, 8).into_iter().map(str::to_string).collect(),
                        _ => Vec::new(),
                    };
                    let n = hits.len();
                    if n > 0 {
                        let row = &mut self.composer.mention_row;
                        *row = (*row).min(n - 1);
                        let mut pick = None;
                        if keyboard_ready {
                            ui.input_mut(|i| {
                                if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                                    *row = (*row + 1) % n;
                                }
                                if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                                    *row = (*row + n - 1) % n;
                                }
                            });
                            pick = ui.input_mut(|i| {
                                i.consume_key(Modifiers::NONE, Key::Enter) || i.consume_key(Modifiers::NONE, Key::Tab)
                            })
                            .then(|| hits[*row].clone());
                        }
                        Frame::new()
                            .fill(pal.surface_alt)
                            .stroke(Stroke::new(1.0_f32, pal.line_soft))
                            .corner_radius(CornerRadius::same(10))
                            .inner_margin(Margin::same(4))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                for (i, f) in hits.iter().enumerate() {
                                    let (rect, r) =
                                        ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), Sense::click());
                                    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, f));
                                    if i == self.composer.mention_row || r.hovered() {
                                        ui.painter().rect_filled(rect, CornerRadius::same(7), pal.selected);
                                    }
                                    ui.painter().text(
                                        rect.left_center() + Vec2::new(10.0, 0.0),
                                        Align2::LEFT_CENTER,
                                        icon::FILE,
                                        theme::body(13.0),
                                        pal.mute,
                                    );
                                    let g = widgets::line_galley(ui, f, theme::body(12.5), pal.ink, rect.width() - 40.0);
                                    ui.painter().galley(
                                        rect.left_center() + Vec2::new(30.0, -g.size().y / 2.0),
                                        g,
                                        pal.ink,
                                    );
                                    if r.clicked() {
                                        pick = Some(f.clone());
                                    }
                                }
                                ui.label(
                                    RichText::new("↑↓ 选择 · Enter 附加到消息")
                                    .font(theme::body(theme::TINY))
                                    .color(pal.faint),
                                );
                            });
                        if let Some(f) = pick {
                            self.composer.draft.truncate(*at);
                            if !self.composer.attachments.contains(&f) {
                                if self.composer.attachments.len() < MAX_ATTACHMENTS {
                                    self.composer.attachments.push(f);
                                } else {
                                    self.composer.message = Some(("单次最多附加 16 个文件".into(), true));
                                }
                            }
                            self.composer.mention_row = 0;
                            self.composer.focus = true;
                        }
                    }
                } else {
                    self.composer.mention_row = 0;
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
                                .id(editor_id)
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
                if resp.has_focus()
                    && ui.input(|i| i.events.iter().any(|e| matches!(e, Event::Key { key: Key::V, pressed: true, modifiers, .. } if modifiers.command)))
                {
                    self.paste_image(root.as_deref());
                }
                if resp.has_focus()
                    && keyboard_ready
                    && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter))
                {
                    send = true;
                }
                if !resp.has_focus() {
                    self.composer.ime_active = false;
                }

                let has_draft = !self.composer.draft.trim().is_empty();
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if fixed {
                        if cur.loaded {
                            if widgets::icon_button(ui, icon::PAPERCLIP, 26.0, pal.mute, pal.hover, false)
                                .on_hover_text("附加项目文件到本条消息").clicked()
                            {
                                pick_files = true;
                            }
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
            Confirm::Resume { ticket, body, files } => self.submit(Action::Follow {
                ticket,
                body,
                resume: true,
                files,
            }),
        }
    }
}

#[cfg(test)]
mod keyboard_tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};
    use std::sync::{Arc, Mutex};

    fn harness(data: &std::path::Path, project: &std::path::Path) -> Harness<'static, App> {
        let data = data.to_path_buf();
        let project = project.to_path_buf();
        Harness::builder().with_size(egui::vec2(1280.0, 860.0)).build_eframe(move |cc| {
            crate::theme::install_fonts(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let host = host::Host::start_with(data.clone(), host::tests::fakes, move || ctx.request_repaint())
                .map_err(|e| format!("{e:#}"));
            let worker = crate::worker::spawn(crate::data::StoreSource::opener(data.clone()), cc.egui_ctx.clone());
            let opts = crate::cli::Options { data: Some(data), ..Default::default() };
            let mut app = App::new(cc, opts, worker, Arc::new(Mutex::new(None)), host);
            app.composer.project = Some(project.clone());
            app.composer.index = Some(FileIndex {
                root: project,
                files: Some(Ok(vec!["zh-one.txt".into(), "zh-two.txt".into()])),
                pending: None,
            });
            app.composer.draft = "@".into();
            app
        })
    }

    fn assert_not_selected_or_sent(h: &Harness<'_, App>) {
        assert!(h.state().composer.attachments.is_empty());
        assert_eq!(h.state().composer.pending, 0);
        assert!(h.state().view.is_none());
    }

    #[test]
    fn unsupported_image_lead_keeps_draft_and_attachment() {
        let data = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut h = harness(data.path(), project.path());
        h.state_mut().composer.draft = "描述这张图".into();
        h.state_mut().composer.attachments = vec!["截图.PNG".into()];
        h.run_steps(3);
        h.get_by_label("输入框").focus();
        h.run_steps(2);
        h.key_press(Key::Enter);
        h.run_steps(2);
        assert!(h.state().view.is_none());
        assert_eq!(h.state().composer.pending, 0);
        assert_eq!(h.state().composer.draft, "描述这张图");
        assert_eq!(h.state().composer.attachments, ["截图.PNG"]);
        assert!(h.state().composer.message.as_ref().is_some_and(|(text, error)| *error && text.contains("Codex")));
    }

    fn ime_and_enter(h: &mut Harness<'_, App>, ime: egui::ImeEvent) {
        // Harness::event runs each event in a separate frame. Raw input is
        // required to reproduce a platform commit/cancel and key together.
        h.input_mut().events.extend([
            Event::Ime(ime),
            Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
        ]);
        h.run_steps(2);
    }

    #[test]
    fn mention_keyboard_requires_editor_focus() {
        let data = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut h = harness(data.path(), project.path());
        h.run_steps(3);
        for key in [Key::ArrowDown, Key::ArrowUp, Key::Enter, Key::Tab] {
            h.state().ctx.memory_mut(|m| {
                if let Some(id) = m.focused() {
                    m.surrender_focus(id);
                }
            });
            h.key_press(key);
            h.run_steps(2);
            assert_not_selected_or_sent(&h);
            assert_eq!(h.state().composer.mention_row, 0);
            assert_eq!(h.state().composer.draft, "@");
        }
        h.get_by_label("输入框").focus();
        h.run_steps(2);
        h.input_mut().focused = false;
        h.key_press(Key::Enter);
        h.run_steps(2);
        assert_not_selected_or_sent(&h);
    }

    #[test]
    fn mention_keyboard_selects_with_arrows_enter_and_tab() {
        let data = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut h = harness(data.path(), project.path());
        h.run_steps(3);
        h.get_by_label("输入框").focus();
        h.run_steps(2);
        h.key_press(Key::ArrowDown);
        h.run_steps(2);
        assert_eq!(h.state().composer.mention_row, 1);
        h.key_press(Key::Enter);
        h.run_steps(2);
        assert_eq!(h.state().composer.attachments, ["zh-two.txt"]);
        assert!(h.state().composer.draft.is_empty());
        assert!(h.state().view.is_none());
        h.state_mut().composer.draft = "@".into();
        h.run_steps(2);
        h.key_press(Key::Tab);
        h.run_steps(2);
        assert_eq!(h.state().composer.attachments, ["zh-two.txt", "zh-one.txt"]);
        assert!(h.state().view.is_none());
    }

    #[test]
    fn mention_and_send_leave_multiframe_ime_keys_to_the_editor() {
        let data = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut h = harness(data.path(), project.path());
        h.run_steps(3);
        h.get_by_label("输入框").focus();
        h.run_steps(2);
        h.event(Event::Ime(egui::ImeEvent::Enabled));
        h.event(Event::Ime(egui::ImeEvent::Preedit("zh".into())));
        h.run_steps(3);
        assert!(h.state().composer.ime_active);
        assert_eq!(h.state().composer.draft, "@zh");
        // These later frames have no IME event: the persisted state must
        // protect both the file menu and normal Enter-to-send.
        for key in [Key::ArrowDown, Key::ArrowUp, Key::Enter, Key::Tab] {
            h.key_press(key);
            h.run_steps(2);
            assert_not_selected_or_sent(&h);
            assert_eq!(h.state().composer.mention_row, 0);
        }
        h.state_mut().composer.draft = "普通消息".into();
        h.key_press(Key::Enter);
        h.run_steps(2);
        assert_not_selected_or_sent(&h);
        ime_and_enter(&mut h, egui::ImeEvent::Commit("中文".into()));
        assert_not_selected_or_sent(&h);
        assert!(!h.state().composer.ime_active);

        // Cancelling a composition blocks that frame, then restores menu keys.
        h.state_mut().composer.draft = "@".into();
        h.event(Event::Ime(egui::ImeEvent::Enabled));
        h.run_steps(2);
        ime_and_enter(&mut h, egui::ImeEvent::Disabled);
        assert_not_selected_or_sent(&h);
        h.key_press(Key::Enter);
        h.run_steps(2);
        assert_eq!(h.state().composer.attachments, ["zh-one.txt"]);
    }
}
