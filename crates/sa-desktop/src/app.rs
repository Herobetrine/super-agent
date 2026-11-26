//! Application state, message handling, persistence and the top-level layout.

use crate::cli::Options;
use crate::composer::Composer;
use crate::host::Host;
use crate::ops::Pending;
use crate::feed::{self, Feed};
use crate::labels;
use crate::notifications::{self, Delivery, Mode, NoticeKind, Visual};
use crate::theme::{self, Palette};
use crate::widgets;
use crate::worker::{Cmd, Msg, Worker};
use egui::{Color32, Frame, Margin, Stroke, Vec2, ViewportCommand};
use egui_phosphor::regular as icon;
use sa_store::{EventView, JobsPage, Snapshot, TicketSummary};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Events kept per ticket in memory (the snapshot window is 180).
const KEEP_EVENTS: usize = 1500;

fn activity_badge(activity: &sa_engine::JobActivity, now: f64, pal: &Palette) -> (String, theme::Tone) {
    if activity.phase == sa_engine::ActivityPhase::RetryWait
        && activity.retry.as_ref().is_some_and(|retry| matches!(retry.class.as_str(), "quota_window" | "quota_reserve"))
    {
        return ("等待额度重置".into(), pal.wait);
    }
    let tone = match activity.badge().1 {
        sa_engine::BadgeTone::Active => pal.run,
        sa_engine::BadgeTone::Waiting => pal.wait,
        sa_engine::BadgeTone::Busy => pal.report,
        sa_engine::BadgeTone::Warning => pal.warn,
        sa_engine::BadgeTone::Danger => pal.fail,
    };
    (activity.badge_text(now), tone)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Collab,
    Native,
    Plan,
    Approvals,
    Diff,
    Knowledge,
}

impl Tab {
    pub fn key(self) -> &'static str {
        match self {
            Tab::Collab => "collab",
            Tab::Native => "native",
            Tab::Plan => "plan",
            Tab::Approvals => "approvals",
            Tab::Diff => "diff",
            Tab::Knowledge => "knowledge",
        }
    }
    pub fn from_key(s: &str) -> Option<Self> {
        Some(match s {
            "collab" | "tasks" => Tab::Collab,
            "native" => Tab::Native,
            "plan" => Tab::Plan,
            "approvals" | "approval" => Tab::Approvals,
            "diff" | "git" => Tab::Diff,
            "knowledge" | "memory" => Tab::Knowledge,
            _ => return None,
        })
    }
}

/// The observed ticket. The snapshot and the feed are shared with the
/// frame that draws them, so painting never copies the conversation.
pub struct TicketView {
    pub id: i64,
    pub snap: Option<Arc<Snapshot>>,
    pub events: Vec<EventView>,
    pub feed: Arc<Feed>,
    /// Markdown parsed by earlier feed builds.
    pub parsed: feed::ParseCache,
    pub error: Option<String>,
    /// More events exist before the first loaded one.
    pub partial_history: bool,
    pub open_groups: HashSet<i64>,
    pub open_jobs: HashSet<i64>,
    pub paging: Option<Paging>,
}

pub struct Paging {
    pub cursors: Vec<Option<i64>>,
    pub page: Option<JobsPage>,
    pub busy: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct SourceState {
    pub label: Option<String>,
    pub error: Option<String>,
}

enum Shot {
    Resize { frames: u32 },
    Wait { since: Instant, frames: u32 },
    Requested { at: Instant },
    Done,
}

pub struct ShotState {
    path: PathBuf,
    size: Option<(f32, f32)>,
    started: Instant,
    stage: Shot,
    result: Arc<Mutex<Option<Result<(), String>>>>,
}

pub struct App {
    pub opts: Options,
    pub worker: Worker,
    pub pal: Palette,
    pub dark: bool,
    pub sidebar_open: bool,
    pub inspector_open: bool,
    pub tab: Tab,
    pub search: String,
    pub history: Vec<TicketSummary>,
    pub history_loaded: bool,
    pub collapsed_projects: HashSet<String>,
    pub source: SourceState,
    pub epoch: u64,
    pub view: Option<TicketView>,
    pub scroll_to_bottom: bool,
    pub auto_selected: bool,
    pub persist: bool,
    window_size_checked: bool,
    shot: Option<ShotState>,
    applied_dark: Option<bool>,
    /// Scheduler host; `None` in observer mode.
    pub host: Option<Host>,
    /// Why the window is an observer (another workbench owns the journal, …).
    pub host_error: Option<String>,
    /// Journal diagnostics and manual recovery instructions.
    pub(crate) recovery: Option<crate::recovery_ui::RecoveryPanel>,
    pub(crate) recovery_probe: Option<crate::ops::Pending<Option<crate::recovery_ui::RecoveryPanel>>>,
    pub composer: Composer,
    pub models: Option<crate::models::ModelDialog>,
    pub diff: DiffState,
    pub workflow: crate::workflow::WorkflowState,
    pub export: Option<Pending<PathBuf>>,
    pub knowledge: crate::knowledge::KnowledgeState,
    pub providers: Option<crate::providers::ProvidersDialog>,
    pub efficiency: Option<crate::efficiency::EfficiencyDialog>,
    pub observer_efficiency: crate::efficiency::ObserverEfficiency,
    /// "从 T3 Code 导入": welcome card, imported projects and the dialog.
    pub t3: crate::t3_import_ui::T3State,
    pub notify: Option<crate::notify_ui::NotifyDialog>,
    /// Background queue for automatic phone pushes.
    pub pusher: crate::push::Pusher,
    /// Pushes that passed the gate; the real queue is bypassed under test.
    #[cfg(test)]
    pub pushed: Vec<crate::push::Push>,
    /// Overrides the viewport focus state under test.
    #[cfg(test)]
    pub test_focus: Option<bool>,
    pub palette: Option<crate::palette::PaletteState>,
    /// Focus the sidebar search on the next frame.
    pub focus_search: bool,
    pub show_archived: bool,
    /// Engine activity of the selected ticket, refreshed at most once a second.
    live: Option<(i64, Instant, Vec<sa_engine::JobActivity>)>,
    /// Sidebar rename dialog: `(ticket, draft title)`.
    pub rename: Option<(i64, String)>,
    notifications: notifications::Notifications,
    notification_preferences: notifications::Preferences,
    notification_watch: notifications::Watch,
    pub(crate) notification_gate: notifications::Gate,
    notification_cards: notifications::Cards,
    notification_badge: notifications::Badge,
    approval_watch: crate::notifications::ApprovalWatch,
    question_watch: crate::notifications::QuestionWatch,
    pub(crate) completion_watch: crate::notifications::CompletionWatch,
    pub extensions: Option<crate::extensions::ExtDialog>,
    /// For background work started outside `update`.
    pub ctx: egui::Context,
}

/// Git changes of one project (inspector "变更" tab).
pub type DiffState = crate::review::ReviewState;

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        opts: Options,
        worker: Worker,
        shot_result: Arc<Mutex<Option<Result<(), String>>>>,
        host: Result<Host, String>,
    ) -> Self {
        let screenshot = opts.screenshot.is_some();
        let storage = if screenshot { None } else { cc.storage };
        let get = |k: &str| storage.and_then(|s| s.get_string(k));
        let dark = opts.dark || get("dark").as_deref() == Some("1");
        let sidebar_open = get("sidebar").as_deref() != Some("0");
        let mut inspector_open = get("inspector").map(|v| v == "1").unwrap_or(true);
        let mut tab = get("tab")
            .and_then(|t| Tab::from_key(&t))
            .unwrap_or(Tab::Collab);
        if let Some(t) = &opts.inspector {
            inspector_open = true;
            if let Some(t) = t.as_deref().and_then(Tab::from_key) {
                tab = t;
            }
        } else if screenshot {
            inspector_open = false;
        }
        let remembered: Option<i64> = if opts.demo {
            None
        } else {
            get("ticket").and_then(|t| t.parse().ok())
        };
        let shot = opts.screenshot.clone().map(|path| ShotState {
            path,
            size: opts.size,
            started: Instant::now(),
            stage: Shot::Resize { frames: 0 },
            result: shot_result,
        });
        let mut app = Self {
            pal: Palette::of(dark),
            dark,
            sidebar_open,
            inspector_open,
            tab,
            search: String::new(),
            history: Vec::new(),
            history_loaded: false,
            collapsed_projects: HashSet::new(),
            source: SourceState::default(),
            epoch: 0,
            view: None,
            scroll_to_bottom: false,
            auto_selected: false,
            persist: !screenshot,
            window_size_checked: false,
            shot,
            applied_dark: None,
            composer: Composer::new(
                opts.project.clone().or_else(|| get("project").map(PathBuf::from)).filter(|p| p.is_dir()),
                get("lead"),
            ),
            models: None,
            diff: DiffState::default(),
            workflow: Default::default(),
            export: None,
            knowledge: Default::default(),
            providers: None,
            efficiency: None,
            observer_efficiency: Default::default(),
            t3: crate::t3_import_ui::T3State::new(get(crate::t3_import_ui::DISMISS_KEY).as_deref() == Some("1")),
            notify: None,
            pusher: crate::push::Pusher::new(),
            #[cfg(test)]
            pushed: Vec::new(),
            #[cfg(test)]
            test_focus: None,
            palette: None,
            focus_search: false,
            show_archived: false,
            live: None,
            rename: None,
            notifications: Default::default(),
            notification_preferences: notifications::Preferences::load(get),
            notification_watch: Default::default(),
            notification_gate: Default::default(),
            notification_cards: Default::default(),
            notification_badge: Default::default(),
            approval_watch: Default::default(),
            question_watch: Default::default(),
            completion_watch: Default::default(),
            extensions: None,
            ctx: cc.egui_ctx.clone(),
            host_error: host.as_ref().err().cloned(),
            recovery: None,
            recovery_probe: None,
            host: host.ok(),
            worker,
            opts,
        };
        if let Some(t) = app.opts.ticket.or(remembered) {
            app.select(Some(t));
            app.auto_selected = true;
        }
        match app.opts.dialog.as_deref() {
            Some("models") => app.open_models(crate::models::Scope::Defaults),
            Some("advisor") => {
                app.open_models(crate::models::Scope::Defaults);
                if let Some(d) = app.models.as_mut() {
                    d.show_advisor();
                }
            }
            Some("providers") => app.open_providers(),
            Some("efficiency") => app.open_efficiency(),
            Some("t3import") => app.open_t3_import(),
            Some("t3import-run") => app.open_t3_import_auto(),
            Some("notify") => app.open_notify(),
            Some("extensions") => app.open_extensions(crate::extensions::ExtScope::Defaults),
            Some("palette") => app.palette = Some(Default::default()),
            Some("rotation") => {
                app.open_providers();
                if let Some(d) = app.providers.as_mut() {
                    d.page = crate::providers::Page::Rotation;
                }
            }
            _ => {}
        }
        app
    }

    pub fn select(&mut self, ticket: Option<i64>) {
        self.view_notification_ticket(ticket);
        if self.view.as_ref().map(|v| v.id) == ticket && ticket.is_some() {
            return;
        }
        self.composer.attachments.clear();
        self.composer.target = None;
        self.workflow.delivery = Default::default();
        self.epoch += 1;
        self.view = ticket.map(|id| TicketView {
            id,
            snap: None,
            events: Vec::new(),
            feed: Arc::default(),
            parsed: Default::default(),
            error: None,
            partial_history: false,
            open_groups: HashSet::new(),
            open_jobs: HashSet::new(),
            paging: None,
        });
        self.scroll_to_bottom = true;
        let _ = self.worker.tx.send(Cmd::Select {
            ticket,
            epoch: self.epoch,
        });
    }

    pub fn request_jobs_page(&mut self, cursors: Vec<Option<i64>>) {
        let Some(v) = self.view.as_mut() else { return };
        let before = cursors.last().copied().flatten();
        let paging = v.paging.get_or_insert(Paging {
            cursors: vec![],
            page: None,
            busy: false,
            error: None,
        });
        paging.busy = true;
        paging.cursors = cursors;
        let _ = self.worker.tx.send(Cmd::JobsPage {
            ticket: v.id,
            before,
            epoch: self.epoch,
        });
    }

    fn drain(&mut self) {
        while let Ok(msg) = self.worker.rx.try_recv() {
            match msg {
                Msg::Connected(label) => {
                    self.source.label = Some(label);
                    self.source.error = None;
                }
                Msg::SourceError(e) => {
                    self.source.error = Some(e);
                    self.history_loaded = true;
                }
                Msg::History { query, rows } => {
                    if query != self.search {
                        continue;
                    }
                    self.history = rows;
                    self.t3_history_changed();
                    self.history_loaded = true;
                    if !self.auto_selected && self.view.is_none() && query.is_empty() && !self.writable() {
                        self.auto_selected = true;
                        if let Some(first) = self.history.first().map(|r| r.id) {
                            self.select(Some(first));
                        }
                    }
                }
                Msg::Snapshot {
                    epoch,
                    ticket,
                    full,
                    snap,
                } => {
                    if epoch != self.epoch {
                        continue;
                    }
                    let Some(v) = self.view.as_mut().filter(|v| v.id == ticket) else {
                        continue;
                    };
                    if !full && v.snap.is_none() {
                        continue;
                    }
                    let mut snap = *snap;
                    let events = std::mem::take(&mut snap.events);
                    if full {
                        v.partial_history = events.iter().filter(|e| e.kind != "delta").count() >= 180;
                        v.events = events;
                        self.scroll_to_bottom = true;
                    } else {
                        let last = v.events.last().map(|e| e.seq).unwrap_or(0);
                        v.events.extend(events.into_iter().filter(|e| e.seq > last));
                        if v.events.len() > KEEP_EVENTS {
                            let cut = v.events.len() - KEEP_EVENTS;
                            v.events.drain(..cut);
                            v.partial_history = true;
                        }
                    }
                    // Incremental loads keep earlier rows; fold them by the store's rule.
                    feed::fold_settled_rounds(&mut v.events, &snap.jobs, snap.lead_resumed);
                    v.feed = Arc::new(feed::build_with(&snap.ticket, &snap.jobs, &v.events, &mut v.parsed));
                    if self.opts.expand_all {
                        for item in &v.feed.items {
                            if let feed::Item::Progress { key, .. } = item {
                                v.open_groups.insert(*key);
                            }
                        }
                        v.open_jobs.extend(snap.jobs.iter().map(|j| j.id));
                    }
                    v.error = None;
                    v.snap = Some(Arc::new(snap));
                }
                Msg::SnapshotError {
                    epoch,
                    ticket,
                    error,
                } => {
                    if epoch == self.epoch
                        && let Some(v) = self.view.as_mut().filter(|v| v.id == ticket)
                    {
                        v.error = Some(error);
                    }
                }
                Msg::JobsPage {
                    epoch,
                    ticket,
                    page,
                } => {
                    if epoch != self.epoch {
                        continue;
                    }
                    if let Some(v) = self.view.as_mut().filter(|v| v.id == ticket)
                        && let Some(p) = v.paging.as_mut()
                    {
                        p.busy = false;
                        match page {
                            Ok(page) => {
                                p.page = Some(page);
                                p.error = None;
                            }
                            Err(e) => p.error = Some(e),
                        }
                    }
                }
            }
        }
    }

    pub fn set_search(&mut self, q: String) {
        self.search = q.clone();
        let _ = self.worker.tx.send(Cmd::Search(q));
    }

    fn guard_window_size(&mut self, ctx: &egui::Context) {
        if self.window_size_checked || !self.persist || self.opts.size.is_some() {
            return;
        }
        let (minimized, inner) = ctx.input(|i| (i.viewport().minimized, i.viewport().inner_rect));
        if minimized == Some(true) {
            return;
        }
        let Some(rect) = inner else {
            return;
        };
        self.window_size_checked = true;
        if rect.width() < crate::MIN_WINDOW.0 || rect.height() < crate::MIN_WINDOW.1 {
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(
                crate::DEFAULT_WINDOW.0,
                crate::DEFAULT_WINDOW.1,
            )));
        }
    }

    fn screenshot(&mut self, ctx: &egui::Context) {
        let ready = self.history_loaded
            && (self.source.error.is_some()
                || self
                    .view
                    .as_ref()
                    .is_none_or(|v| v.snap.is_some() || v.error.is_some()));
        let Some(shot) = self.shot.as_mut() else {
            return;
        };
        ctx.request_repaint();
        let timed_out = shot.started.elapsed() > Duration::from_secs(25);
        match &mut shot.stage {
            Shot::Resize { frames } => {
                // Render at exactly one physical pixel per point so the PNG has the requested size.
                if *frames == 0 {
                    if let Some(native) = ctx.native_pixels_per_point() {
                        ctx.set_zoom_factor(1.0 / native);
                    }
                } else if *frames == 2
                    && let Some((w, h)) = shot.size
                {
                    ctx.send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(w, h)));
                }
                *frames += 1;
                if *frames > 6 {
                    shot.stage = Shot::Wait {
                        since: Instant::now(),
                        frames: 0,
                    };
                }
            }
            Shot::Wait { since, frames } => {
                *frames += 1;
                if (ready && *frames > 20 && since.elapsed() > Duration::from_millis(1500))
                    || timed_out
                {
                    ctx.send_viewport_cmd(ViewportCommand::Screenshot(egui::UserData::default()));
                    shot.stage = Shot::Requested { at: Instant::now() };
                }
            }
            Shot::Requested { at } => {
                let image = ctx.input(|i| {
                    i.events.iter().find_map(|e| match e {
                        egui::Event::Screenshot { image, .. } => Some(image.clone()),
                        _ => None,
                    })
                });
                if let Some(image) = image {
                    let res = save_png(&shot.path, &image);
                    if let Err(e) = &res {
                        eprintln!("截图保存失败：{e}");
                    } else {
                        eprintln!(
                            "截图已保存：{} ({}x{})",
                            shot.path.display(),
                            image.size[0],
                            image.size[1]
                        );
                    }
                    *shot.result.lock().unwrap_or_else(|p| p.into_inner()) = Some(res);
                    shot.stage = Shot::Done;
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                } else if at.elapsed() > Duration::from_secs(10) {
                    *shot.result.lock().unwrap_or_else(|p| p.into_inner()) =
                        Some(Err("截图请求超时".into()));
                    shot.stage = Shot::Done;
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            }
            Shot::Done => {}
        }
    }

    /// Whether the left sidebar has room next to the inspector.
    pub fn sidebar_visible(&self, width: f32) -> bool {
        self.sidebar_open && !(self.inspector_open && width < 1180.0)
    }
}

fn save_png(path: &std::path::Path, image: &egui::ColorImage) -> Result<(), String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let [w, h] = image.size;
    let mut bytes = Vec::with_capacity(w * h * 4);
    for c in &image.pixels {
        bytes.extend_from_slice(&c.to_srgba_unmultiplied());
    }
    let buf = image::RgbaImage::from_raw(w as u32, h as u32, bytes).ok_or("截图尺寸无效")?;
    buf.save_with_format(path, image::ImageFormat::Png)
        .map_err(|e| e.to_string())
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.shortcuts(ctx);
        self.take_dropped_files(ctx);
        self.poll_host();
        self.poll_ops();
        self.poll_knowledge();
        self.poll_extensions();
        self.drain();
        self.poll_desktop_notifications();
        self.t3_poll_recent();
        if self.applied_dark != Some(self.dark) {
            self.pal = Palette::of(self.dark);
            theme::apply(ctx, &self.pal);
            self.applied_dark = Some(self.dark);
        }
        theme::measure_fonts(ctx);
        self.guard_window_size(ctx);
        self.screenshot(ctx);
        if self
            .view
            .as_ref()
            .and_then(|v| v.snap.as_ref())
            .is_some_and(|s| labels::is_active(&s.ticket.state))
        {
            ctx.request_repaint_after(Duration::from_secs(1));
        }

        let pal = self.pal.clone();
        let width = ctx.content_rect().width();

        // A grey canvas carrying the sidebar and one or two white sheets.
        const GAP: i8 = 8;
        let sidebar = self.sidebar_visible(width);
        if sidebar {
            egui::SidePanel::left("sidebar")
                .default_width(268.0)
                .width_range(236.0..=400.0)
                .resizable(true)
                .show_separator_line(false)
                .frame(Frame::new().fill(pal.sidebar))
                .show(ctx, |ui| self.sidebar(ui));
        }

        if self.inspector_open {
            egui::SidePanel::right("inspector")
                .default_width(388.0)
                .width_range(320.0..=600.0)
                .resizable(true)
                .show_separator_line(false)
                .frame(Frame::new().fill(pal.canvas).inner_margin(Margin {
                    left: 0,
                    right: GAP,
                    top: GAP,
                    bottom: GAP,
                }))
                .show(ctx, |ui| widgets::sheet(ui, &pal, |ui| self.inspector(ui)));
        }

        egui::CentralPanel::default()
            .frame(Frame::new().fill(pal.canvas).inner_margin(Margin {
                left: if sidebar { 0 } else { GAP },
                right: GAP,
                top: GAP,
                bottom: GAP,
            }))
            .show(ctx, |ui| {
                widgets::sheet(ui, &pal, |ui| {
                    let bar = egui::TopBottomPanel::top("topbar")
                        .exact_height(46.0)
                        .show_separator_line(false)
                        .frame(Frame::new().inner_margin(Margin {
                            left: 12,
                            right: 10,
                            top: 0,
                            bottom: 0,
                        }))
                        .show_inside(ui, |ui| self.topbar(ui));
                    let r = bar.response.rect;
                    ui.painter()
                        .hline(r.x_range(), r.bottom(), Stroke::new(1.0_f32, pal.line_soft));
                    let conversation = egui::Rect::from_min_max(
                        egui::pos2(ui.clip_rect().left(), r.bottom()),
                        ui.clip_rect().max,
                    );
                    self.conversation(ui);
                    if !ui.is_sizing_pass()
                        && !ctx.will_discard()
                        && let Some(view) = &self.view
                        && view.snap.is_some()
                        && let Some(event) = view.events.last()
                    {
                        let window = self.notification_window(Some(view.id));
                        self.notification_gate.displayed(window, event.seq);
                    }
                    if let Some(ticket) = self.notification_cards.show(ctx, conversation, &pal) {
                        self.select(Some(ticket));
                    }
                });
            });
        self.confirm_dialog(ctx);
        self.models_dialog(ctx);
        self.providers_dialog(ctx);
        self.efficiency_dialog(ctx);
        self.recovery_dialog(ctx);
        self.t3_import_dialog(ctx);
        self.notify_dialog(ctx);
        self.extensions_dialog(ctx);
        self.palette_ui(ctx);
        self.rename_dialog(ctx);
        if self.main_notification_window() {
            self.notification_badge.update(frame, self.notification_gate.count());
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if !self.persist {
            return;
        }
        storage.set_string("dark", if self.dark { "1" } else { "0" }.into());
        storage.set_string("sidebar", if self.sidebar_open { "1" } else { "0" }.into());
        storage.set_string(
            "inspector",
            if self.inspector_open { "1" } else { "0" }.into(),
        );
        storage.set_string("tab", self.tab.key().into());
        storage.set_string("lead", self.composer.lead.clone());
        if self.main_notification_window() {
            self.notification_preferences.save(storage);
        }
        storage.set_string(crate::t3_import_ui::DISMISS_KEY, if self.t3.dismissed() { "1" } else { "0" }.into());
        if let Some(p) = &self.composer.project {
            storage.set_string("project", p.to_string_lossy().into_owned());
        }
        if !self.opts.demo
            && let Some(v) = &self.view
        {
            storage.set_string("ticket", v.id.to_string());
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.t3_finish_on_exit();
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        // Screenshots must not depend on where the mouse happens to be.
        if self.shot.is_some() {
            raw.events.retain(|e| {
                !matches!(
                    e,
                    egui::Event::PointerMoved(_)
                        | egui::Event::PointerButton { .. }
                        | egui::Event::MouseWheel { .. }
                        | egui::Event::MouseMoved(_)
                )
            });
            raw.events.push(egui::Event::PointerGone);
        }
    }

    fn persist_egui_memory(&self) -> bool {
        self.persist
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        self.pal.canvas.to_normalized_gamma_f32()
    }
}

impl App {
    pub(crate) fn start_export(&mut self, ctx: &egui::Context, ticket: i64) {
        let Some(file) = rfd::FileDialog::new()
            .set_title("导出完整工单记录（请选择新文件）")
            .set_file_name(format!("ticket-{ticket}.jsonl"))
            .add_filter("JSON Lines", &["jsonl"])
            .save_file()
        else {
            return;
        };
        if file.exists() {
            self.composer.message = Some(("请选择新文件，导出不会覆盖已有文件".into(), true));
            return;
        }
        let data = self.opts.data.clone().unwrap_or_else(sa_store::default_data_dir);
        self.export = Some(Pending::spawn(ctx, move || crate::ops::export(&data, ticket, &file)));
    }

    pub(crate) fn shot_active(&self) -> bool {
        self.shot.is_some()
    }

    fn main_notification_window(&self) -> bool {
        self.writable() && !self.opts.demo && self.shot.is_none()
    }

    fn notification_window(&self, ticket: Option<i64>) -> notifications::Window {
        notifications::Window {
            main: self.main_notification_window(),
            focused: self.window_focused(),
            ticket,
            mode: self.notification_mode(),
        }
    }

    fn view_notification_ticket(&mut self, ticket: Option<i64>) {
        let window = self.notification_window(ticket);
        self.notification_gate.view(window);
        if window.main && window.focused && let Some(ticket) = ticket {
            self.notification_watch.view(ticket);
            self.notification_cards.remove_ticket(ticket);
        }
    }

    fn poll_desktop_notifications(&mut self) {
        let ticket = self.view.as_ref().map(|view| view.id);
        let window = self.notification_window(ticket);
        if !window.main {
            return;
        }
        self.view_notification_ticket(ticket);
        let data = self.opts.data.clone().unwrap_or_else(sa_store::default_data_dir);
        for delivery in self.notification_watch.poll(
            &self.ctx, &data, &mut self.notification_gate, window,
        ) {
            self.deliver_notification(&data, delivery);
        }
    }

    fn deliver_notification(&mut self, data: &std::path::Path, delivery: Delivery) {
        let Delivery { notice, visual, sound, title } = delivery;
        if visual == Visual::Toast {
            self.attention(notice.kind != NoticeKind::Completion);
            if !cfg!(test) {
                self.notifications.post(&self.ctx, data, notifications::Request::Toast(
                    notifications::ToastRequest { notice, sound },
                ));
            }
        } else {
            if visual == Visual::Card {
                self.notification_cards.push(notice, title, Instant::now());
            }
            if sound && !cfg!(test) {
                self.notifications.post(&self.ctx, data, notifications::Request::Sound);
            }
        }
    }

    /// Live badges per job of the selected ticket (writable windows only):
    /// 运行中 / 等待上游 / 工具执行中 / 疑似卡住 / 重试中 n/max / 熔断.
    pub fn live_badges(&mut self) -> std::collections::HashMap<i64, (String, crate::theme::Tone)> {
        let (Some(host), Some(ticket)) = (&self.host, self.view.as_ref().map(|v| v.id)) else {
            return Default::default();
        };
        let stale = self
            .live
            .as_ref()
            .is_none_or(|(t, at, _)| *t != ticket || at.elapsed() >= Duration::from_secs(1));
        if stale {
            self.live = Some((ticket, Instant::now(), host.activity(ticket)));
            self.ctx.request_repaint_after(Duration::from_secs(1));
        }
        let now = labels::now_secs();
        let pal = &self.pal;
        self.live
            .as_ref()
            .map(|(_, _, list)| {
                list.iter()
                    .map(|a| {
                        (a.job, activity_badge(a, now, pal))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Flash the taskbar button when the window is not focused.
    fn attention(&self, critical: bool) {
        if !self.window_focused() {
            let kind = if critical {
                egui::UserAttentionType::Critical
            } else {
                egui::UserAttentionType::Informational
            };
            self.ctx.send_viewport_cmd(ViewportCommand::RequestUserAttention(kind));
        }
    }

    pub(crate) fn start_export_markdown(&mut self, ctx: &egui::Context, ticket: i64) {
        let Some(file) = rfd::FileDialog::new()
            .set_title("导出对话（请选择新文件）")
            .set_file_name(format!("ticket-{ticket}.md"))
            .add_filter("Markdown", &["md"])
            .save_file()
        else {
            return;
        };
        if file.exists() {
            self.composer.message = Some(("请选择新文件，导出不会覆盖已有文件".into(), true));
            return;
        }
        let data = self.opts.data.clone().unwrap_or_else(sa_store::default_data_dir);
        self.export = Some(Pending::spawn(ctx, move || crate::ops::export_markdown(&data, ticket, &file)));
    }

    fn poll_ops(&mut self) {
        let completion_epoch = self.host.as_ref()
            .filter(|_| !self.opts.demo && self.shot.is_none())
            .and_then(|host| host.notify_settings_cache().completion_epoch());
        self.completion_watch.set_epoch(completion_epoch);
        if self.writable() && !self.opts.demo && self.shot.is_none() {
            let data = self.opts.data.clone().unwrap_or_else(sa_store::default_data_dir);
            let notices = self.approval_watch.poll(&self.ctx, &data).into_iter()
                .chain(self.question_watch.poll(&self.ctx, &data));
            for notice in notices {
                match notice.kind {
                    crate::notifications::NoticeKind::Approval => {
                        self.push_event(crate::push::Event::Approval { ticket: notice.ticket });
                    }
                    crate::notifications::NoticeKind::Question => {
                        self.push_event(crate::push::Event::Question { ticket: notice.ticket });
                    }
                    crate::notifications::NoticeKind::Completion => {}
                }
            }
            if completion_epoch.is_some() {
                for (ticket, state) in self.completion_watch.poll(&self.ctx, &data) {
                    self.push_event(crate::push::Event::Finished { ticket, state });
                }
            }
        }
        if let Some(r) = self.export.as_ref().and_then(Pending::poll) {
            self.export = None;
            self.composer.message = Some(match r {
                Ok(f) => (format!("已导出至 {}", f.display()), false),
                Err(e) => (e, true),
            });
        }
        self.diff.poll();
        for feedback in self.notifications.poll() {
            match feedback {
                crate::notifications::Feedback::Open(ticket) => {
                    self.select(Some(ticket));
                    self.ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
                    self.ctx.send_viewport_cmd(ViewportCommand::Focus);
                }
                crate::notifications::Feedback::Failed(error) => self.composer.message = Some((error, true)),
            }
        }
    }

    pub(crate) fn notification_mode(&self) -> Mode {
        self.notification_preferences.mode()
    }

    #[cfg(test)]
    pub(crate) fn hold_notification_snapshot(&mut self) -> impl FnOnce() + use<> {
        let path = self.opts.data.as_ref().unwrap().join(sa_store::DB_FILE);
        self.notification_watch.hold_snapshot(path, &self.notification_gate)
    }

    #[cfg(test)]
    pub(crate) fn test_notification_worker(&mut self) -> std::sync::mpsc::Receiver<notifications::Request> {
        self.notifications.test_worker()
    }

    pub(crate) fn toggle_notifications(&mut self) {
        self.set_notification_mode(self.notification_preferences.toggled());
    }

    pub(crate) fn set_notification_mode(&mut self, mode: Mode) {
        let previous = self.notification_mode();
        if mode == previous {
            return;
        }
        self.notification_preferences.set(mode);
        if previous.visual() && !mode.visual() {
            self.notifications.stop();
            self.notification_cards.clear();
        }
    }

    fn topbar(&mut self, ui: &mut egui::Ui) {
        let pal = self.pal.clone();
        let sidebar = self.sidebar_visible(ui.ctx().content_rect().width());
        let snap = self.view.as_ref().and_then(|v| v.snap.as_ref());
        let state = snap.map(|s| s.ticket.state.clone());
        let approvals = snap.map_or(0, |s| s.approvals.len());
        let title = snap
            .map(|s| s.ticket.goal.clone())
            .or_else(|| {
                let id = self.view.as_ref()?.id;
                self.history
                    .iter()
                    .find(|r| r.id == id)
                    .map(|r| r.goal.clone())
            })
            .unwrap_or_else(|| match self.view.as_ref() {
                Some(v) => format!("工单 #{}", v.id),
                None if self.host.is_some() => "新对话".into(),
                None => "未选择工单".into(),
            });
        let crumb = snap
            .map(|s| labels::basename(&s.ticket.cwd))
            .filter(|c| !c.is_empty() && !c.contains("super-agent-project-"));
        let title_line = labels::compact(&title, 200);
        // In a narrow column the title needs the room: drop the folder crumb
        // and the "all is well" scheduler note (the sidebar foot repeats it).
        let compact = ui.available_width() < 520.0;
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if !sidebar {
                if widgets::panel_button(ui, false, 30.0, &pal, false)
                    .on_hover_text("展开会话侧栏")
                    .clicked()
                {
                    self.sidebar_open = true;
                    // Narrow windows show one side at a time.
                    if !self.sidebar_visible(ui.ctx().content_rect().width()) {
                        self.inspector_open = false;
                    }
                }
                if self.writable() {
                    let r = widgets::icon_button(ui, icon::NOTE_PENCIL, 30.0, pal.mute, pal.hover, false)
                        .on_hover_text("新建对话 · Ctrl N");
                    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "新建对话"));
                    if r.clicked() {
                        self.new_conversation();
                    }
                }
                ui.add_space(6.0);
            } else {
                ui.add_space(6.0);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if widgets::panel_button(ui, true, 30.0, &pal, self.inspector_open)
                    .on_hover_text("展开或收起项目面板 · Ctrl J")
                    .clicked()
                {
                    self.inspector_open = !self.inspector_open;
                }
                if let Some(id) = self.view.as_ref().map(|v| v.id) {
                    let busy = self.export.is_some();
                    let r = ui
                        .add_enabled_ui(!busy, |ui| {
                            widgets::icon_button(ui, icon::EXPORT, 30.0, pal.mute, pal.hover, false)
                        })
                        .inner
                        .on_hover_text("导出");
                    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, !busy, "导出"));
                    let mut pick = None;
                    egui::Popup::menu(&r).show(|ui| {
                        ui.set_min_width(230.0);
                        if ui.button(format!("{}  导出对话为 Markdown", icon::FILE_TEXT)).clicked() {
                            pick = Some(true);
                        }
                        if ui.button(format!("{}  导出完整记录（JSON Lines）", icon::DATABASE)).clicked() {
                            pick = Some(false);
                        }
                    });
                    match pick {
                        Some(true) => self.start_export_markdown(ui.ctx(), id),
                        Some(false) => self.start_export(ui.ctx(), id),
                        None => {}
                    }
                }
                ui.add_space(4.0);
                if approvals > 0
                    && widgets::badge(
                        ui,
                        icon::SHIELD_WARNING,
                        &format!("待审批 {approvals}"),
                        pal.warn,
                    )
                    .interact(egui::Sense::click())
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text("在项目面板中查看待审批请求")
                    .clicked()
                {
                    self.inspector_open = true;
                    self.tab = Tab::Approvals;
                }
                // A healthy scheduler is stated quietly; trouble gets a tinted badge.
                let quiet = theme::Tone {
                    bg: Color32::TRANSPARENT,
                    fg: pal.mute,
                };
                match (&self.host, &self.host_error) {
                    (Some(h), _) => {
                        use sa_engine::engine::SchedulerState;
                        let health = h.scheduler_health();
                        match health.state {
                            SchedulerState::Running if compact => {}
                            SchedulerState::Running => {
                                let text = match h.try_active_count() {
                                    Some(n) if n > 0 => format!("调度中 · {n}"),
                                    _ => "调度中".into(),
                                };
                                widgets::badge(ui, icon::PULSE, &text, quiet)
                                    .on_hover_text("后台调度检查正常，正在执行任务或等待新的工作。");
                            }
                            SchedulerState::DatabaseBusy => {
                                widgets::badge(ui, icon::HOURGLASS, "等待数据库", pal.warn)
                                    .on_hover_text("其他操作正在写入工单账本，调度会稍后重试。尚未开始新的派工，不会重复执行已运行的任务。");
                            }
                            SchedulerState::Unresponsive => {
                                widgets::badge(ui, icon::WARNING_CIRCLE, "调度无响应", pal.fail)
                                    .on_hover_text(format!(
                                        "后台已 {:.0} 秒未完成调度检查。可能有任务仍在执行；请保留现场并检查故障，勿重复派工。",
                                        health.last_progress_seconds_ago.unwrap_or_default()
                                    ));
                            }
                            SchedulerState::Failed => {
                                let reason = h.fault().unwrap_or_else(|| "后台调度异常结束".into());
                                widgets::badge(ui, icon::WARNING_CIRCLE, "调度已停止", pal.fail)
                                    .on_hover_text(format!("{reason}\n后续任务不会自动启动。请保留现场并检查故障后恢复工作台。"));
                            }
                            SchedulerState::NotStarted | SchedulerState::Stopped => {
                                widgets::badge(ui, icon::PAUSE, "调度未运行", pal.warn);
                            }
                        }
                    }
                    (None, _) if compact => {}
                    (None, err) => {
                        let tip = match err {
                            Some(e) => format!("只读观察：{e}"),
                            None => "此窗口只读取工单日志，不调度、不写入，也不调用模型。".into(),
                        };
                        widgets::badge(ui, icon::EYE, "只读观察", quiet).on_hover_text(tip);
                    }
                }
                if let Some(st) = &state {
                    ui.add_space(2.0);
                    widgets::status(
                        ui,
                        &labels::job_state(st),
                        pal.state_tone(st).fg,
                        labels::is_active(st),
                    );
                }
                ui.add_space(10.0);
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 7.0;
                    if let Some(crumb) = crumb.as_ref().filter(|_| !compact) {
                        ui.label(
                            egui::RichText::new(crumb)
                                .font(theme::body(theme::UI))
                                .color(pal.mute),
                        );
                        ui.label(
                            egui::RichText::new("/")
                                .font(theme::body(theme::UI))
                                .color(pal.faint),
                        );
                    }
                    let w = ui.available_width();
                    let g = widgets::line_galley(ui, &title_line, theme::strong(13.5), pal.ink, w);
                    ui.add(egui::Label::new(g).selectable(false))
                        .on_hover_text(title);
                });
            });
        });
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    #[test]
    fn queued_quota_badges_are_waits_and_route_recovery_keeps_its_badge() {
        let pal = Palette::light();
        let mut activity = sa_engine::JobActivity {
            job: 1,
            agent: "codex".into(),
            phase: sa_engine::ActivityPhase::RetryWait,
            idle_ms: 0,
            open_tools: Vec::new(),
            last_frame_at: None,
            started_at: None,
            threshold_ms: None,
            health: sa_engine::resilience::Health::Open,
            retry: Some(sa_engine::RetryInfo {
                attempt: 0, max: None, class: "quota_window".into(), next_at: 1060.0,
            }),
        };
        for reason in ["quota_window", "quota_reserve"] {
            activity.retry.as_mut().unwrap().class = reason.into();
            let (label, tone) = activity_badge(&activity, 1000.0, &pal);
            assert_eq!(label, "等待额度重置");
            assert_eq!(tone.fg, pal.wait.fg);
        }
        activity.retry.as_mut().unwrap().class = "circuit_open".into();
        let (label, tone) = activity_badge(&activity, 1000.0, &pal);
        assert_eq!(label, activity.badge_text(1000.0));
        assert!(label.starts_with("执行端熔断"));
        assert_eq!(tone.fg, pal.fail.fg);
    }
}
