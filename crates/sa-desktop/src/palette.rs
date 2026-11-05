//! Command palette (Ctrl+K) and global keyboard shortcuts, like the
//! Electron workbench's palette (`studio.js` `commandItems`).

use crate::app::{App, Tab};
use crate::extensions::ExtScope;
use crate::host::Action;
use crate::labels;
use crate::models::Scope;
use crate::theme;
use crate::widgets;
use egui::{Align2, CornerRadius, Key, KeyboardShortcut, Modifiers, RichText, Sense, Stroke, vec2};
use egui_phosphor::regular as icon;

#[derive(Default)]
pub struct PaletteState {
    pub query: String,
    pub index: usize,
    focused: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    New,
    Search,
    Sidebar,
    Inspector,
    Theme,
    Models,
    TicketModels,
    Extensions,
    TicketExtensions,
    Providers,
    Project,
    Attach,
    Tab(Tab),
    Export,
    Stop,
    Open(i64),
}

pub const NEW: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::N);
pub const PALETTE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::K);
pub const SEARCH: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::F);
pub const SIDEBAR: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::B);
pub const INSPECTOR: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::J);
pub const THEME: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::L);
pub const MODELS: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::M);

struct Row {
    cmd: Command,
    icon: &'static str,
    title: String,
    hint: String,
}

impl App {
    /// Global shortcuts; called once per frame before the panels.
    pub fn shortcuts(&mut self, ctx: &egui::Context) {
        let pressed = |s: &KeyboardShortcut| ctx.input_mut(|i| i.consume_shortcut(s));
        if pressed(&PALETTE) {
            self.palette = match self.palette {
                Some(_) => None,
                None => Some(PaletteState::default()),
            };
        }
        let modal_open = self.palette.is_some()
            || self.models.is_some()
            || self.providers.is_some()
            || self.extensions.is_some()
            || self.composer.confirm.is_some();
        if modal_open {
            return;
        }
        for (s, cmd) in [
            (NEW, Command::New),
            (SEARCH, Command::Search),
            (SIDEBAR, Command::Sidebar),
            (INSPECTOR, Command::Inspector),
            (THEME, Command::Theme),
            (MODELS, Command::Models),
        ] {
            if pressed(&s) {
                self.run_command(cmd);
            }
        }
    }

    fn rows(&self) -> Vec<Row> {
        let row = |cmd, icon, title: &str, hint: &str| Row {
            cmd,
            icon,
            title: title.into(),
            hint: hint.into(),
        };
        let writable = self.writable();
        let ticket = self.view.as_ref().map(|v| v.id);
        let active = self
            .view
            .as_ref()
            .and_then(|v| v.snap.as_ref())
            .is_some_and(|s| labels::is_active(&s.ticket.state));
        let mut rows = Vec::new();
        if writable {
            rows.push(row(Command::New, icon::NOTE_PENCIL, "新建对话", "Ctrl N"));
        }
        rows.push(row(Command::Search, icon::MAGNIFYING_GLASS, "搜索对话或项目", "Ctrl Shift F"));
        rows.push(row(Command::Sidebar, icon::SIDEBAR_SIMPLE, "显示或隐藏会话侧栏", "Ctrl B"));
        rows.push(row(Command::Inspector, icon::SQUARES_FOUR, "显示或隐藏项目面板", "Ctrl J"));
        rows.push(row(Command::Theme, if self.dark { icon::SUN } else { icon::MOON }, "切换浅色/暗色主题", "Ctrl Shift L"));
        if writable {
            rows.push(row(Command::Models, icon::SLIDERS_HORIZONTAL, "模型与执行端 · 新会话默认", "Ctrl Shift M"));
            rows.push(row(Command::Extensions, icon::PUZZLE_PIECE, "MCP 与技能 · 新会话默认", ""));
            rows.push(row(Command::Providers, icon::PLUGS, "供应商与轮换", ""));
            if ticket.is_none() {
                rows.push(row(Command::Project, icon::FOLDER_SIMPLE, "选择项目目录", ""));
                rows.push(row(Command::Attach, icon::PAPERCLIP, "附加项目文件", ""));
            } else {
                rows.push(row(Command::TicketModels, icon::SLIDERS_HORIZONTAL, "本次会话的模型与执行端", ""));
                rows.push(row(Command::TicketExtensions, icon::PUZZLE_PIECE, "本次会话的 MCP 与技能", ""));
            }
            if active {
                rows.push(row(Command::Stop, icon::STOP_CIRCLE, "停止当前工单", ""));
            }
        }
        for (tab, ic, name) in [
            (Tab::Collab, icon::USERS_THREE, "查看协作任务"),
            (Tab::Plan, icon::LIST_CHECKS, "打开项目计划"),
            (Tab::Approvals, icon::SHIELD_CHECK, "查看待审批请求"),
            (Tab::Diff, icon::GIT_DIFF, "查看 Git 变更"),
            (Tab::Knowledge, icon::BRAIN, "打开项目知识库"),
        ] {
            rows.push(row(Command::Tab(tab), ic, name, ""));
        }
        if ticket.is_some() {
            rows.push(row(Command::Export, icon::EXPORT, "导出完整记录", ""));
        }
        for t in self.history.iter().take(40) {
            rows.push(Row {
                cmd: Command::Open(t.id),
                icon: icon::CHAT_CIRCLE_TEXT,
                title: labels::compact(&t.goal, 80),
                hint: format!("#{} · {}", t.id, labels::basename(&t.cwd)),
            });
        }
        rows
    }

    pub fn run_command(&mut self, cmd: Command) {
        match cmd {
            Command::New => {
                if self.writable() {
                    self.new_conversation();
                }
            }
            Command::Search => {
                self.sidebar_open = true;
                self.focus_search = true;
            }
            Command::Sidebar => self.sidebar_open = !self.sidebar_open,
            Command::Inspector => self.inspector_open = !self.inspector_open,
            Command::Theme => self.dark = !self.dark,
            Command::Models => self.open_models(Scope::Defaults),
            Command::TicketModels => {
                if let Some(t) = self.view.as_ref().map(|v| v.id) {
                    self.open_models(Scope::Ticket(t));
                }
            }
            Command::Extensions => self.open_extensions(ExtScope::Defaults),
            Command::TicketExtensions => {
                if let Some(t) = self.view.as_ref().map(|v| v.id) {
                    self.open_extensions(ExtScope::Ticket(t));
                }
            }
            Command::Providers => self.open_providers(),
            Command::Project => self.pick_project(),
            Command::Attach => self.pick_attachments(),
            Command::Tab(t) => {
                self.inspector_open = true;
                self.tab = t;
            }
            Command::Export => {
                if let Some(t) = self.view.as_ref().map(|v| v.id) {
                    let ctx = self.ctx.clone();
                    self.start_export(&ctx, t);
                }
            }
            Command::Stop => {
                if let Some(t) = self.view.as_ref().map(|v| v.id) {
                    self.submit(Action::Cancel { ticket: t });
                }
            }
            Command::Open(t) => self.select(Some(t)),
        }
    }

    pub fn palette_ui(&mut self, ctx: &egui::Context) {
        let Some(state) = self.palette.as_mut() else { return };
        let pal = self.pal.clone();
        let q = state.query.to_lowercase();
        let all = {
            let query = q.clone();
            let _ = state;
            self.rows()
                .into_iter()
                .filter(|r| query.is_empty() || r.title.to_lowercase().contains(&query) || r.hint.to_lowercase().contains(&query))
                .take(60)
                .collect::<Vec<_>>()
        };
        let state = self.palette.as_mut().expect("checked");
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
            state.index = (state.index + 1).min(all.len().saturating_sub(1));
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
            state.index = state.index.saturating_sub(1);
        }
        state.index = state.index.min(all.len().saturating_sub(1));
        let enter = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
        let escape = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
        let mut run: Option<Command> = None;
        if enter {
            run = all.get(state.index).map(|r| r.cmd);
        }
        let mut close = escape;
        let screen = ctx.content_rect();
        let w = 620.0_f32.min(screen.width() - 40.0);
        let modal = egui::Modal::new(egui::Id::new("sa-palette"))
            .area(
                egui::Modal::default_area(egui::Id::new("sa-palette-area"))
                    .anchor(Align2::CENTER_TOP, vec2(0.0, (screen.height() * 0.12).max(40.0))),
            )
            .frame(widgets::modal_frame(&pal).inner_margin(egui::Margin::same(10)))
            .show(ctx, |ui| {
                ui.set_width(w);
                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    ui.label(RichText::new(icon::MAGNIFYING_GLASS).font(theme::body(17.0)).color(pal.mute));
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut state.query)
                            .id_salt("palette-query")
                            .frame(false)
                            .font(theme::body(15.0))
                            .hint_text("输入命令或会话…")
                            .desired_width(f32::INFINITY),
                    );
                    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "命令搜索"));
                    if !state.focused {
                        r.request_focus();
                        state.focused = true;
                    }
                    if r.changed() {
                        state.index = 0;
                    }
                });
                ui.add_space(6.0);
                widgets::rule(ui, pal.line_soft);
                ui.add_space(6.0);
                egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                    if all.is_empty() {
                        ui.add_space(12.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("没有匹配的命令").color(pal.mute));
                        });
                        ui.add_space(12.0);
                    }
                    for (i, r) in all.iter().enumerate() {
                        let selected = i == state.index;
                        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click());
                        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &r.title));
                        if resp.hovered() {
                            state.index = i;
                        }
                        let p = ui.painter();
                        if selected || resp.hovered() {
                            p.rect_filled(rect, CornerRadius::same(8), pal.selected);
                        }
                        p.text(rect.left_center() + vec2(14.0, 0.0), Align2::LEFT_CENTER, r.icon, theme::body(16.0), pal.accent);
                        let tw = rect.width() - 200.0;
                        let g = widgets::line_galley(ui, &r.title, theme::body(13.5), pal.ink, tw);
                        p.galley(egui::pos2(rect.left() + 40.0, rect.center().y - g.size().y / 2.0), g, pal.ink);
                        if !r.hint.is_empty() {
                            let hg = widgets::line_galley(ui, &r.hint, theme::body(theme::TINY), pal.faint, 150.0);
                            p.galley(
                                egui::pos2(rect.right() - 12.0 - hg.size().x, rect.center().y - hg.size().y / 2.0),
                                hg,
                                pal.faint,
                            );
                        }
                        if resp.clicked() {
                            run = Some(r.cmd);
                        }
                        if selected && (enter || resp.hovered()) {
                            ui.scroll_to_rect(rect, None);
                        }
                    }
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("↑↓ 选择 · Enter 执行 · Esc 关闭 · Ctrl K 随时打开")
                            .font(theme::body(theme::TINY))
                            .color(pal.faint),
                    );
                });
                let _ = Stroke::NONE;
            });
        if modal.should_close() {
            close = true;
        }
        if let Some(cmd) = run {
            self.palette = None;
            self.run_command(cmd);
        } else if close {
            self.palette = None;
        }
    }
}
