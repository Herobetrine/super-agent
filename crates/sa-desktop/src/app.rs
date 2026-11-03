//! Application state, message handling, persistence and the top-level layout.

use crate::cli::Options;
use crate::composer::Composer;
use crate::host::Host;
use crate::feed::{self, Feed};
use crate::labels;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Collab,
    Native,
    Plan,
    Approvals,
}

impl Tab {
    pub fn key(self) -> &'static str {
        match self {
            Tab::Collab => "collab",
            Tab::Native => "native",
            Tab::Plan => "plan",
            Tab::Approvals => "approvals",
        }
    }
    pub fn from_key(s: &str) -> Option<Self> {
        Some(match s {
            "collab" | "tasks" => Tab::Collab,
            "native" => Tab::Native,
            "plan" => Tab::Plan,
            "approvals" | "approval" => Tab::Approvals,
            _ => return None,
        })
    }
}

/// The observed ticket.
pub struct TicketView {
    pub id: i64,
    pub snap: Option<Snapshot>,
    pub events: Vec<EventView>,
    pub feed: Feed,
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
    shot: Option<ShotState>,
    applied_dark: Option<bool>,
    /// Scheduler host; `None` in observer mode.
    pub host: Option<Host>,
    /// Why the window is an observer (another workbench owns the journal, …).
    pub host_error: Option<String>,
    pub composer: Composer,
}

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
            shot,
            applied_dark: None,
            composer: Composer::new(
                get("project").map(PathBuf::from).filter(|p| p.is_dir()),
                get("lead"),
            ),
            host_error: host.as_ref().err().cloned(),
            host: host.ok(),
            worker,
            opts,
        };
        if let Some(t) = app.opts.ticket.or(remembered) {
            app.select(Some(t));
            app.auto_selected = true;
        }
        app
    }

    pub fn select(&mut self, ticket: Option<i64>) {
        if self.view.as_ref().map(|v| v.id) == ticket && ticket.is_some() {
            return;
        }
        self.epoch += 1;
        self.view = ticket.map(|id| TicketView {
            id,
            snap: None,
            events: Vec::new(),
            feed: Feed::default(),
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
                    self.history_loaded = true;
                    if !self.auto_selected && self.view.is_none() && query.is_empty() {
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
                        v.partial_history = events.len() >= 180;
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
                    v.feed = feed::build(&snap.ticket, &snap.jobs, &v.events);
                    if self.opts.expand_all {
                        for item in &v.feed.items {
                            if let feed::Item::Progress { key, .. } = item {
                                v.open_groups.insert(*key);
                            }
                        }
                        v.open_jobs.extend(snap.jobs.iter().map(|j| j.id));
                    }
                    v.error = None;
                    v.snap = Some(snap);
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
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_host();
        self.drain();
        if self.applied_dark != Some(self.dark) {
            self.pal = Palette::of(self.dark);
            theme::apply(ctx, &self.pal);
            self.applied_dark = Some(self.dark);
        }
        theme::measure_fonts(ctx);
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

        egui::SidePanel::left("rail")
            .exact_width(56.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(
                Frame::new()
                    .fill(pal.rail)
                    .inner_margin(Margin::symmetric(0, 14)),
            )
            .show(ctx, |ui| self.rail(ui));

        if self.sidebar_visible(width) {
            egui::SidePanel::left("sidebar")
                .default_width(284.0)
                .width_range(236.0..=400.0)
                .resizable(true)
                .frame(
                    Frame::new()
                        .fill(pal.sidebar)
                        .stroke(Stroke::new(1.0_f32, pal.line)),
                )
                .show(ctx, |ui| self.sidebar(ui));
        }

        egui::TopBottomPanel::top("topbar")
            .exact_height(58.0)
            .frame(
                Frame::new()
                    .fill(pal.surface)
                    .stroke(Stroke::new(1.0_f32, pal.line_soft))
                    .inner_margin(Margin {
                        left: 20,
                        right: 16,
                        top: 0,
                        bottom: 0,
                    }),
            )
            .show(ctx, |ui| self.topbar(ui));

        if self.inspector_open {
            egui::SidePanel::right("inspector")
                .default_width(372.0)
                .width_range(300.0..=560.0)
                .resizable(true)
                .frame(
                    Frame::new()
                        .fill(pal.canvas)
                        .stroke(Stroke::new(1.0_f32, pal.line)),
                )
                .show(ctx, |ui| self.inspector(ui));
        }

        egui::CentralPanel::default()
            .frame(Frame::new().fill(pal.surface))
            .show(ctx, |ui| self.conversation(ui));
        self.confirm_dialog(ctx);
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
        if let Some(p) = &self.composer.project {
            storage.set_string("project", p.to_string_lossy().into_owned());
        }
        if !self.opts.demo
            && let Some(v) = &self.view
        {
            storage.set_string("ticket", v.id.to_string());
        }
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
    fn rail(&mut self, ui: &mut egui::Ui) {
        let pal = self.pal.clone();
        ui.vertical_centered(|ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            let (r, resp) = ui.allocate_exact_size(Vec2::splat(32.0), egui::Sense::hover());
            widgets::brand_mark(ui, r, &pal);
            resp.on_hover_text("Super Agent");
            ui.add_space(14.0);
            let hover = Color32::from_white_alpha(18);
            if widgets::icon_button(
                ui,
                icon::CHATS_CIRCLE,
                38.0,
                pal.rail_ink,
                hover,
                self.sidebar_open,
            )
            .on_hover_text("会话列表")
            .clicked()
            {
                self.sidebar_open = !self.sidebar_open;
            }
            if widgets::icon_button(
                ui,
                icon::SQUARES_FOUR,
                38.0,
                pal.rail_ink,
                hover,
                self.inspector_open,
            )
            .on_hover_text("项目面板")
            .clicked()
            {
                self.inspector_open = !self.inspector_open;
            }
            let approvals = self
                .view
                .as_ref()
                .and_then(|v| v.snap.as_ref())
                .map_or(0, |s| s.approvals.len());
            let r = widgets::icon_button(
                ui,
                icon::SHIELD_CHECK,
                38.0,
                pal.rail_ink,
                hover,
                self.inspector_open && self.tab == Tab::Approvals,
            )
            .on_hover_text(if approvals > 0 {
                format!("待审批 {approvals}")
            } else {
                "审批".into()
            });
            if approvals > 0 {
                let c = r.rect.right_top() + egui::vec2(-9.0, 9.0);
                ui.painter()
                    .circle_filled(c, 4.5, Color32::from_rgb(0xe0, 0x9a, 0x3c));
                ui.painter()
                    .circle_stroke(c, 4.5, Stroke::new(1.5_f32, pal.rail));
            }
            if r.clicked() {
                self.inspector_open = true;
                self.tab = Tab::Approvals;
            }
        });
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.y = 10.0;
            let (color, tip) = match (&self.source.error, &self.source.label) {
                (Some(e), _) => (Color32::from_rgb(0xe0, 0x7a, 0x52), format!("未连接 · {e}")),
                (None, Some(l)) => (Color32::from_rgb(0x7c, 0xc4, 0x8f), l.clone()),
                _ => (pal.rail_mute, "正在连接数据源".to_string()),
            };
            let (r, resp) = ui.allocate_exact_size(Vec2::splat(14.0), egui::Sense::hover());
            ui.painter().circle_filled(r.center(), 4.0, color);
            resp.on_hover_text(tip);
            let theme_icon = if self.dark { icon::SUN } else { icon::MOON };
            if widgets::icon_button(
                ui,
                theme_icon,
                38.0,
                pal.rail_ink,
                Color32::from_white_alpha(18),
                false,
            )
            .on_hover_text(if self.dark {
                "切换到浅色主题"
            } else {
                "切换到暗色主题"
            })
            .clicked()
            {
                self.dark = !self.dark;
            }
        });
    }

    fn topbar(&mut self, ui: &mut egui::Ui) {
        let pal = self.pal.clone();
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
                None => "未选择工单".into(),
            });
        let title_line = labels::compact(&title, 200);
        ui.horizontal_centered(|ui| {
            if widgets::icon_button(ui, icon::SIDEBAR_SIMPLE, 32.0, pal.mute, pal.hover, false)
                .on_hover_text("展开或收起会话侧栏")
                .clicked()
            {
                self.sidebar_open = !self.sidebar_open;
            }
            ui.add_space(4.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                if widgets::quiet_button(
                    ui,
                    icon::SQUARES_FOUR,
                    "项目面板",
                    &pal,
                    self.inspector_open,
                )
                .on_hover_text("展开或收起项目面板")
                .clicked()
                {
                    self.inspector_open = !self.inspector_open;
                }
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
                match (&self.host, &self.host_error) {
                    (Some(h), _) => match h.fault() {
                        Some(f) => {
                            widgets::badge(ui, icon::WARNING_CIRCLE, "调度已停止", pal.fail)
                                .on_hover_text(format!("调度器遇到无法恢复的错误：{f}"));
                        }
                        None => {
                            let n = h.active_count();
                            let text = if n > 0 { format!("调度中 · {n}") } else { "调度中".into() };
                            widgets::badge(ui, icon::PULSE, &text, pal.run)
                                .on_hover_text("此窗口持有工单库并负责调度与模型调用。");
                        }
                    },
                    (None, err) => {
                        let tip = match err {
                            Some(e) => format!("只读观察：{e}"),
                            None => "此窗口只读取工单日志，不调度、不写入，也不调用模型。".into(),
                        };
                        widgets::badge(ui, icon::EYE, "只读观察", pal.neutral).on_hover_text(tip);
                    }
                }
                if let Some(st) = &state {
                    widgets::chip(ui, &labels::job_state(st), pal.state_tone(st), theme::SMALL);
                }
                ui.add_space(8.0);
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(
                        egui::RichText::new("工作台")
                            .font(theme::body(14.0))
                            .color(pal.mute),
                    );
                    ui.label(
                        egui::RichText::new("/")
                            .font(theme::body(14.0))
                            .color(pal.faint),
                    );
                    let w = ui.available_width();
                    let g = widgets::line_galley(ui, &title_line, theme::strong(14.5), pal.ink, w);
                    ui.add(egui::Label::new(g).selectable(false))
                        .on_hover_text(title);
                });
            });
        });
    }
}
