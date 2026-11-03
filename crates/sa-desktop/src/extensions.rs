//! MCP servers and skills (`desktop/ui/extensions.js`): global default
//! grants, grants of the next new conversation, and the read-only grants
//! bound to a ticket. Management calls go to the host's
//! [`sa_ext::ProjectExtensions`] on background threads.

use crate::app::App;
use crate::labels;
use crate::ops::Pending;
use crate::theme;
use crate::widgets;
use egui::{Align, CornerRadius, Frame, Label, Layout, Margin, RichText, Stroke, Ui};
use egui_phosphor::regular as icon;
use serde_json::{Value, json};
use std::path::PathBuf;

const TRUST: &str =
    "stdio 会以当前用户权限启动本机程序。HTTP 会连接服务并可能传输任务内容。请只授权可信来源；不要在界面中粘贴密钥。";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtScope {
    Defaults,
    Draft,
    Ticket(i64),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ExtTab {
    Skills,
    Servers,
}

#[derive(Clone, Default)]
pub struct ServerForm {
    pub id: Option<String>,
    pub name: String,
    pub http: bool,
    pub command: String,
    /// JSON array of strings.
    pub args: String,
    pub url: String,
    pub token_env: String,
    pub enabled: bool,
    pub trusted: bool,
}

pub enum View {
    List,
    Edit(ServerForm),
    Probe { row: Value, result: Option<Result<Value, String>> },
    Preview { name: String, body: Option<Result<String, String>> },
    Remove(Value),
    Forget(Value),
}

enum Task {
    Reload,
    /// A write; on success show the notice and reload.
    Write(&'static str),
    Probe,
    Preview,
    /// Global defaults saved: close the dialog.
    Saved,
}

pub struct ExtDialog {
    pub scope: ExtScope,
    cwd: Option<PathBuf>,
    pub data: Option<Result<Value, String>>,
    /// Ticket grants (locked) or the draft's previous choice.
    saved: Option<Value>,
    pub skills: Vec<String>,
    pub servers: Vec<String>,
    selection_ready: bool,
    pub follow: bool,
    pub locked: bool,
    pub tab: ExtTab,
    pub query: String,
    pub view: View,
    pub notice: Option<(String, bool)>,
    task: Option<(Task, Pending<Value>)>,
}

fn ids(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str().map(str::to_string).or_else(|| x.get("id").and_then(Value::as_str).map(str::to_string)))
        .collect()
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

impl ExtDialog {
    fn selection(&self) -> Value {
        json!({"skills": self.skills, "servers": self.servers})
    }

    /// Reset the selection from `saved` or the defaults, keeping only usable ids.
    fn apply_data(&mut self) {
        let Some(Ok(data)) = &self.data else { return };
        if !self.selection_ready {
            let from = self.saved.as_ref().unwrap_or(&data["defaults"]);
            self.skills = ids(from.get("skills"));
            self.servers = ids(from.get("servers"));
            self.selection_ready = true;
        }
        if !self.locked {
            let servers = data["servers"].as_array().cloned().unwrap_or_default();
            self.servers.retain(|id| {
                servers
                    .iter()
                    .any(|s| s["id"].as_str() == Some(id) && s["enabled"] == json!(true) && s["tools"].is_array())
            });
            let skills = data["skills"].as_array().cloned().unwrap_or_default();
            self.skills.retain(|id| skills.iter().any(|s| s["id"].as_str() == Some(id)));
        }
    }
}

impl App {
    pub fn open_extensions(&mut self, scope: ExtScope) {
        let Some(host) = &self.host else { return };
        let snap = self.view.as_ref().and_then(|v| v.snap.as_ref());
        let (cwd, saved, locked) = match &scope {
            ExtScope::Defaults => (None, None, false),
            ExtScope::Draft => (self.composer.project.clone(), self.composer.extensions.clone(), false),
            ExtScope::Ticket(t) => {
                let summary = host.ext().summary(*t).unwrap_or_else(|_| json!({"skills": [], "servers": []}));
                let cwd = snap.filter(|s| s.ticket.id == *t).map(|s| PathBuf::from(&s.ticket.cwd));
                (cwd, Some(summary), true)
            }
        };
        let follow = scope == ExtScope::Draft && saved.is_none();
        self.extensions = Some(ExtDialog {
            scope,
            cwd,
            data: None,
            saved,
            skills: vec![],
            servers: vec![],
            selection_ready: false,
            follow,
            locked,
            tab: ExtTab::Skills,
            query: String::new(),
            view: View::List,
            notice: None,
            task: None,
        });
        self.ext_task(Task::Reload);
    }

    fn ext_task(&mut self, task: Task) {
        let (Some(host), Some(d)) = (&self.host, self.extensions.as_mut()) else { return };
        let ext = host.ext();
        let cwd = d.cwd.clone();
        let pending = match &task {
            Task::Reload => Pending::spawn(&self.ctx, move || ext.list(cwd.as_deref())),
            _ => return,
        };
        d.task = Some((task, pending));
    }

    fn ext_spawn(&mut self, task: Task, f: impl FnOnce(sa_ext::ProjectExtensions, tokio::runtime::Handle) -> anyhow::Result<Value> + Send + 'static) {
        // (`f` runs on a plain thread; MCP probes block on the host runtime.)
        let (Some(host), Some(d)) = (&self.host, self.extensions.as_mut()) else { return };
        let (ext, rt) = (host.ext(), host.runtime());
        d.notice = None;
        d.task = Some((task, Pending::spawn(&self.ctx, move || f(ext, rt))));
    }

    pub fn poll_extensions(&mut self) {
        let Some(d) = self.extensions.as_mut() else { return };
        let Some(result) = d.task.as_ref().and_then(|(_, p)| p.poll()) else { return };
        let (task, _) = d.task.take().expect("polled");
        match task {
            Task::Reload => {
                d.data = Some(result);
                d.apply_data();
            }
            Task::Write(msg) => match result {
                Ok(_) => {
                    d.notice = Some((msg.into(), false));
                    d.view = View::List;
                    self.ext_task(Task::Reload);
                }
                Err(e) => d.notice = Some((e, true)),
            },
            Task::Probe => {
                if let View::Probe { result: r, .. } = &mut d.view {
                    *r = Some(result);
                }
            }
            Task::Preview => {
                if let View::Preview { body, .. } = &mut d.view {
                    *body = Some(result.map(|v| s(&v, "body").chars().take(120_000).collect()));
                }
            }
            Task::Saved => match result {
                Ok(_) => {
                    self.extensions = None;
                    self.composer.message = Some(("MCP 与技能授权已保存。".into(), false));
                }
                Err(e) => d.notice = Some((e, true)),
            },
        }
    }

    pub fn extensions_dialog(&mut self, ctx: &egui::Context) {
        if self.extensions.is_none() {
            return;
        }
        let pal = self.pal.clone();
        let mut close = false;
        let mut next: Option<(Task, Job)> = None;
        let mut reload = false;
        let mut import = false;
        let mut save_draft: Option<Option<Value>> = None;
        let d = self.extensions.as_mut().expect("checked");
        let busy = d.task.is_some();
        let title = match d.scope {
            ExtScope::Defaults => "MCP 与技能",
            ExtScope::Draft => "本次新会话的 MCP 与技能",
            ExtScope::Ticket(_) => "本次会话的 MCP 与技能",
        };
        let modal = egui::Modal::new(egui::Id::new("sa-extensions"))
            .frame(
                Frame::new()
                    .fill(pal.surface)
                    .stroke(Stroke::new(1.0_f32, pal.line))
                    .corner_radius(CornerRadius::same(14))
                    .inner_margin(Margin::same(20)),
            )
            .show(ctx, |ui| {
                ui.set_width(600.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(title).font(theme::strong(16.0)).color(pal.ink));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if busy {
                            ui.spinner();
                        }
                    });
                });
                ui.add_space(8.0);
                let max = (ui.ctx().content_rect().height() - 240.0).max(240.0);
                let kind = Kind::of(&d.view);
                egui::ScrollArea::vertical()
                    .max_height(max)
                    .min_scrolled_height(max.min(420.0))
                    .show(ui, |ui| match &kind {
                        Kind::List => list_view(ui, d, &pal, busy, &mut next, &mut reload, &mut import),
                        Kind::Edit => edit_view(ui, d, &pal, busy, &mut next),
                        Kind::Probe => probe_view(ui, d, &pal, busy, &mut next),
                        Kind::Preview => {
                            if let View::Preview { name, body } = &d.view {
                                ui.label(RichText::new(name.as_str()).font(theme::strong(14.0)));
                                match body {
                                    None => {
                                        ui.label(RichText::new("正在读取技能说明…").color(pal.mute));
                                    }
                                    Some(Err(e)) => {
                                        ui.label(RichText::new(e.as_str()).color(pal.fail.fg));
                                    }
                                    Some(Ok(text)) => {
                                        ui.add(Label::new(RichText::new(text.as_str()).monospace().size(12.0)).wrap());
                                    }
                                }
                            }
                        }
                        Kind::Remove(_, name) => {
                            ui.label(RichText::new(format!("确定从本机配置移除 {name}？")).color(pal.warn.fg));
                        }
                        Kind::Forget(_, name) => {
                            ui.label(RichText::new(format!("取消登记 {name}？原有技能文件会保留。")).color(pal.warn.fg));
                        }
                    });
                if let Some((text, error)) = &d.notice {
                    ui.add_space(6.0);
                    let color = if *error { pal.fail.fg } else { pal.ok.fg };
                    ui.add(Label::new(RichText::new(text).font(theme::body(theme::SMALL)).color(color)).wrap());
                }
                ui.add_space(10.0);
                // Footer
                ui.horizontal(|ui| {
                    match &kind {
                        Kind::List => {
                            ui.label(
                                RichText::new(format!("技能 {} · MCP {}", d.skills.len(), d.servers.len()))
                                    .font(theme::body(theme::SMALL))
                                    .color(pal.mute),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if !d.locked {
                                    let ok = !busy && matches!(d.data, Some(Ok(_)));
                                    if ui
                                        .add_enabled_ui(ok, |ui| widgets::quiet_button(ui, icon::CHECK, "保存授权", &pal, true))
                                        .inner
                                        .clicked()
                                    {
                                        match d.scope {
                                            ExtScope::Defaults => {
                                                let sel = d.selection();
                                                next = Some((
                                                    Task::Saved,
                                                    Box::new(move |ext, _| ext.set_defaults(&sel, None).map(|_| Value::Null)),
                                                ));
                                            }
                                            _ => save_draft = Some((!d.follow).then(|| d.selection())),
                                        }
                                    }
                                }
                                if widgets::quiet_button(ui, "", if d.locked { "关闭" } else { "取消" }, &pal, false).clicked() {
                                    close = true;
                                }
                            });
                        }
                        Kind::Remove(id, _) | Kind::Forget(id, _) => {
                            let id = id.clone();
                            let remove = matches!(kind, Kind::Remove(..));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let text = if remove { "确认移除" } else { "确认取消登记" };
                                if ui
                                    .add_enabled_ui(!busy, |ui| widgets::quiet_button(ui, icon::CHECK, text, &pal, true))
                                    .inner
                                    .clicked()
                                {
                                    d.servers.retain(|x| *x != id);
                                    d.skills.retain(|x| *x != id);
                                    next = Some(if remove {
                                        (Task::Write("已移除服务。"), Box::new(move |ext, _| ext.remove(&id).map(|_| Value::Null)))
                                    } else {
                                        (Task::Write("已取消登记。"), Box::new(move |ext, _| ext.forget_skill(&id).map(|_| Value::Null)))
                                    });
                                }
                                if widgets::quiet_button(ui, "", "取消", &pal, false).clicked() {
                                    d.view = View::List;
                                }
                            });
                        }
                        _ => {
                            if widgets::quiet_button(ui, icon::ARROW_LEFT, "返回列表", &pal, false).clicked() {
                                d.view = View::List;
                                d.notice = None;
                                reload = true;
                            }
                        }
                    }
                });
            });
        if modal.should_close() && next.is_none() && save_draft.is_none() {
            close = true;
        }
        if close {
            self.extensions = None;
            return;
        }
        if let Some(sel) = save_draft {
            self.composer.extensions = sel;
            self.composer.message = Some(("本次新会话的 MCP 与技能授权已设置。".into(), false));
            self.extensions = None;
            return;
        }
        if import {
            if let Some(folder) = rfd::FileDialog::new().set_title("选择含有 SKILL.md 的技能文件夹").pick_folder() {
                self.ext_spawn(Task::Write("已登记本机技能，请勾选后保存授权。"), move |ext, _| {
                    ext.import_skill(&folder).map(Value::String)
                });
            }
            return;
        }
        if let Some((task, f)) = next {
            self.ext_spawn(task, f);
        } else if reload {
            self.ext_task(Task::Reload);
        }
    }
}

/// The view without its borrowed payload (for layout decisions).
enum Kind {
    List,
    Edit,
    Probe,
    Preview,
    Remove(String, String),
    Forget(String, String),
}

impl Kind {
    fn of(v: &View) -> Self {
        match v {
            View::List => Kind::List,
            View::Edit(_) => Kind::Edit,
            View::Probe { .. } => Kind::Probe,
            View::Preview { .. } => Kind::Preview,
            View::Remove(r) => Kind::Remove(s(r, "id").into(), s(r, "name").into()),
            View::Forget(r) => Kind::Forget(s(r, "id").into(), s(r, "name").into()),
        }
    }
}

type Job = Box<dyn FnOnce(sa_ext::ProjectExtensions, tokio::runtime::Handle) -> anyhow::Result<Value> + Send>;

fn list_view(
    ui: &mut Ui,
    d: &mut ExtDialog,
    pal: &theme::Palette,
    busy: bool,
    next: &mut Option<(Task, Job)>,
    reload: &mut bool,
    import: &mut bool,
) {
    let data = match &d.data {
        None => {
            ui.label(RichText::new("正在读取本机配置…").color(pal.mute));
            return;
        }
        Some(Err(e)) => {
            ui.label(RichText::new(e.as_str()).color(pal.fail.fg));
            if widgets::quiet_button(ui, icon::ARROWS_CLOCKWISE, "重试", pal, false).clicked() {
                *reload = true;
            }
            return;
        }
        Some(Ok(v)) => v.clone(),
    };
    ui.horizontal(|ui| {
        for (tab, key, name) in [(ExtTab::Skills, "skills", "技能"), (ExtTab::Servers, "servers", "MCP 服务")] {
            let n = data[key].as_array().map_or(0, Vec::len);
            if widgets::quiet_button(ui, "", &format!("{name} {n}"), pal, d.tab == tab).clicked() {
                d.tab = tab;
                d.query.clear();
            }
        }
    });
    if d.scope == ExtScope::Draft {
        let before = d.follow;
        ui.checkbox(&mut d.follow, "跟随全局默认授权");
        if d.follow && !before {
            d.skills = ids(data["defaults"].get("skills"));
            d.servers = ids(data["defaults"].get("servers"));
        }
    }
    let hint = if d.locked {
        "此会话的授权已绑定；新建会话可重新选择。"
    } else if d.scope == ExtScope::Defaults {
        "勾选要默认授予新会话的项目，保存后生效。"
    } else {
        "取消跟随后，可单独设置当前会话的授权。"
    };
    ui.label(RichText::new(hint).font(theme::body(theme::SMALL)).color(pal.mute));
    if d.tab == ExtTab::Servers {
        ui.add(Label::new(RichText::new(TRUST).font(theme::body(theme::SMALL)).color(pal.warn.fg)).wrap());
        ui.label(
            RichText::new("启用服务并探测工具后即可勾选。只读权限下，工单不能调用 MCP 工具。")
                .font(theme::body(theme::SMALL))
                .color(pal.mute),
        );
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let placeholder = if d.tab == ExtTab::Skills { "搜索技能或来源" } else { "搜索 MCP 服务" };
        let r = ui.add(
            egui::TextEdit::singleline(&mut d.query)
                .hint_text(placeholder)
                .background_color(pal.canvas)
                .desired_width(260.0),
        );
        r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, placeholder));
        if d.scope == ExtScope::Defaults {
            if d.tab == ExtTab::Servers {
                if widgets::quiet_button(ui, icon::PLUS, "添加服务", pal, false).clicked() {
                    d.view = View::Edit(ServerForm {
                        args: "[]".into(),
                        ..Default::default()
                    });
                    d.notice = None;
                }
            } else if ui
                .add_enabled_ui(!busy, |ui| widgets::quiet_button(ui, icon::FOLDER_SIMPLE, "导入文件夹", pal, false))
                .inner
                .clicked()
            {
                *import = true;
            }
        }
    });
    ui.add_space(6.0);
    let key = if d.tab == ExtTab::Skills { "skills" } else { "servers" };
    let mut rows: Vec<Value> = data[key].as_array().cloned().unwrap_or_default();
    if d.locked {
        let bound = if d.tab == ExtTab::Skills { &d.skills } else { &d.servers };
        for id in bound {
            if !rows.iter().any(|r| s(r, "id") == id) {
                rows.push(json!({"id": id, "name": id, "missing": true}));
            }
        }
    }
    let q = d.query.to_lowercase();
    rows.retain(|r| q.is_empty() || ["name", "description", "source"].iter().any(|k| s(r, k).to_lowercase().contains(&q)));
    if rows.is_empty() {
        let text = if !q.is_empty() {
            "没有匹配的项目。"
        } else if d.tab == ExtTab::Skills {
            "未发现已安装的技能。"
        } else {
            "尚未配置 MCP 服务。"
        };
        ui.label(RichText::new(text).color(pal.mute));
    }
    for row in rows {
        let id = s(&row, "id").to_string();
        let name = if s(&row, "name").is_empty() { id.clone() } else { s(&row, "name").to_string() };
        let servers = d.tab == ExtTab::Servers;
        let missing = row["missing"] == json!(true);
        let inactive = servers && row["enabled"] != json!(true);
        let unprobed = servers && !row["tools"].is_array();
        Frame::new()
            .fill(pal.canvas)
            .stroke(Stroke::new(1.0_f32, pal.line_soft))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let list = if servers { &mut d.servers } else { &mut d.skills };
                    let mut on = list.contains(&id);
                    let enabled = !d.locked && !d.follow && (on || !(missing || inactive || unprobed));
                    let r = ui.add_enabled(enabled, egui::Checkbox::new(&mut on, RichText::new(&name).strong()));
                    let label = format!("选择 {name}");
                    r.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, on, &label));
                    if r.changed() {
                        if on {
                            list.push(id.clone());
                        } else {
                            list.retain(|x| *x != id);
                        }
                    }
                    let tag = if missing {
                        Some("配置已移除".to_string())
                    } else if inactive {
                        Some("服务未启用".into())
                    } else if unprobed {
                        Some("待探测工具".into())
                    } else if servers {
                        Some(format!("{} 个工具", row["tools"].as_array().map_or(0, Vec::len)))
                    } else {
                        None
                    };
                    if let Some(t) = tag {
                        widgets::chip(ui, &t, pal.neutral, theme::TINY);
                    }
                });
                if servers {
                    let line = if s(&row, "transport") == "http" {
                        format!("HTTP · {}", s(&row, "url"))
                    } else {
                        format!("stdio · {}", s(&row, "command"))
                    };
                    ui.label(RichText::new(line).monospace().size(11.5).color(pal.mute));
                    if d.scope == ExtScope::Defaults && !missing {
                        ui.horizontal(|ui| {
                            if widgets::quiet_button(ui, icon::PENCIL_SIMPLE, &format!("编辑 {name}"), pal, false).clicked() {
                                d.view = View::Edit(ServerForm {
                                    id: Some(id.clone()),
                                    name: name.clone(),
                                    http: s(&row, "transport") == "http",
                                    command: s(&row, "command").to_string(),
                                    args: serde_json::to_string_pretty(&row["args"]).unwrap_or_else(|_| "[]".into()),
                                    url: s(&row, "url").to_string(),
                                    token_env: s(&row, "tokenEnv").to_string(),
                                    enabled: row["enabled"] == json!(true),
                                    trusted: false,
                                });
                                d.notice = None;
                            }
                            if widgets::quiet_button(ui, icon::PULSE, &format!("探测 {name}"), pal, false).clicked() {
                                d.view = View::Probe { row: row.clone(), result: None };
                                d.notice = None;
                            }
                        });
                    }
                } else if !missing {
                    ui.add(
                        Label::new(
                            RichText::new(labels::compact(s(&row, "description"), 220))
                                .font(theme::body(theme::SMALL))
                                .color(pal.mute),
                        )
                        .wrap(),
                    );
                    ui.label(RichText::new(s(&row, "source")).monospace().size(11.0).color(pal.faint));
                    ui.horizontal(|ui| {
                        if widgets::quiet_button(ui, icon::FILE_TEXT, &format!("预览 {name}"), pal, false).clicked() {
                            let (sid, cwd) = (id.clone(), d.cwd.clone());
                            d.view = View::Preview { name: name.clone(), body: None };
                            *next = Some((Task::Preview, Box::new(move |ext, _| ext.preview(&sid, cwd.as_deref()))));
                        }
                        if row["imported"] == json!(true)
                            && d.scope == ExtScope::Defaults
                            && widgets::quiet_button(ui, "", &format!("取消登记 {name}"), pal, false).clicked()
                        {
                            d.view = View::Forget(row.clone());
                        }
                    });
                }
            });
        ui.add_space(6.0);
    }
}

fn edit_view(ui: &mut Ui, d: &mut ExtDialog, pal: &theme::Palette, busy: bool, next: &mut Option<(Task, Job)>) {
    let View::Edit(f) = &mut d.view else { return };
    ui.label(
        RichText::new(if f.id.is_some() { "编辑 MCP 服务" } else { "添加 MCP 服务" }).font(theme::strong(14.0)),
    );
    ui.add(Label::new(RichText::new(TRUST).font(theme::body(theme::SMALL)).color(pal.warn.fg)).wrap());
    ui.add_space(6.0);
    let row = |ui: &mut Ui, label: &str, value: &mut String, hint: &str, multi: bool| {
        ui.horizontal(|ui| {
            ui.add_sized([120.0, 24.0], Label::new(RichText::new(label).font(theme::body(13.0))));
            let edit = if multi {
                egui::TextEdit::multiline(value).desired_rows(3)
            } else {
                egui::TextEdit::singleline(value)
            };
            let r = ui.add(edit.hint_text(hint).desired_width(ui.available_width()));
            r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, label));
        });
    };
    row(ui, "服务名称", &mut f.name, "", false);
    ui.horizontal(|ui| {
        ui.add_sized([120.0, 24.0], Label::new(RichText::new("传输方式").font(theme::body(13.0))));
        let before = f.http;
        egui::ComboBox::from_id_salt("mcp-transport")
            .selected_text(if f.http { "远程服务 · Streamable HTTP" } else { "本机程序 · stdio" })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut f.http, false, "本机程序 · stdio");
                ui.selectable_value(&mut f.http, true, "远程服务 · Streamable HTTP");
            });
        if before != f.http {
            f.trusted = false;
        }
    });
    if f.http {
        row(ui, "MCP 服务地址", &mut f.url, "https://mcp.example.org/mcp", false);
        row(ui, "令牌环境变量名", &mut f.token_env, "可选，例如 MCP_SERVICE_TOKEN", false);
        ui.label(
            RichText::new("仅允许 HTTPS 或回环 HTTP。需要令牌时，填入以 MCP_ 开头的环境变量名；不要在 URL 或表单内填写令牌值。")
                .font(theme::body(theme::SMALL))
                .color(pal.mute),
        );
    } else {
        row(ui, "可执行程序", &mut f.command, "单个程序名或路径", false);
        row(ui, "启动参数", &mut f.args, "JSON 字符串数组，例如 [\"--stdio\"]", true);
    }
    ui.checkbox(&mut f.enabled, "启用服务，允许将它授予会话");
    ui.checkbox(&mut f.trusted, "我信任上述程序或服务来源");
    ui.add_space(6.0);
    let mut remove = None;
    let mut error = None;
    ui.horizontal(|ui| {
        if let Some(id) = f.id.clone()
            && widgets::quiet_button(ui, icon::TRASH, "移除服务", pal, false).clicked()
        {
            remove = Some(json!({"id": id, "name": f.name}));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui
                .add_enabled_ui(!busy, |ui| widgets::quiet_button(ui, icon::CHECK, "保存服务", pal, true))
                .inner
                .clicked()
            {
                match server_record(f) {
                    Ok(record) => {
                        *next = Some((
                            Task::Write("已保存服务。授权前请先启用服务并探测工具。"),
                            Box::new(move |ext, _| ext.save_server(&record).map(Value::String)),
                        ));
                    }
                    Err(e) => error = Some(e),
                }
            }
        });
    });
    if let Some(e) = error {
        d.notice = Some((e, true));
    }
    if let Some(row) = remove {
        d.view = View::Remove(row);
    }
}

/// Client-side checks of `extensions.js` before `mcpSave`.
fn server_record(f: &ServerForm) -> Result<Value, String> {
    if f.name.trim().is_empty() {
        return Err("填写服务名称。".into());
    }
    let mut record = json!({"name": f.name.trim(), "enabled": f.enabled, "transport": if f.http { "http" } else { "stdio" }});
    if f.http {
        let url = url::Url::parse(f.url.trim()).map_err(|_| "请输入完整的服务地址。".to_string())?;
        let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !(url.scheme() == "https" || (loopback && url.scheme() == "http"))
        {
            return Err("地址须为 HTTPS 或回环 HTTP，且不能携带账号、密码、查询参数或片段。".into());
        }
        let token = f.token_env.trim();
        if !token.is_empty()
            && !(token.starts_with("MCP_")
                && token.len() <= 100
                && token.len() > 4
                && token[4..].chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
        {
            return Err("变量名须以 MCP_ 开头，后接大写字母、数字或下划线。".into());
        }
        record["url"] = json!(url.as_str());
        record["tokenEnv"] = json!(token);
    } else {
        if f.command.trim().is_empty() {
            return Err("填写可执行程序。".into());
        }
        let args: Value =
            serde_json::from_str(&f.args).map_err(|_| "启动参数必须是 JSON 数组，例如 [\"--stdio\"]。".to_string())?;
        if !args.as_array().is_some_and(|a| a.iter().all(Value::is_string)) {
            return Err("每个启动参数都必须是字符串。".into());
        }
        record["command"] = json!(f.command.trim());
        record["args"] = args;
    }
    if f.enabled && !f.trusted {
        return Err("启用前请确认你信任此来源。".into());
    }
    if let Some(id) = &f.id {
        record["id"] = json!(id);
    }
    Ok(record)
}

fn probe_view(ui: &mut Ui, d: &mut ExtDialog, pal: &theme::Palette, busy: bool, next: &mut Option<(Task, Job)>) {
    let View::Probe { row, result } = &mut d.view else { return };
    ui.label(RichText::new(format!("探测 {}", s(row, "name"))).font(theme::strong(14.0)));
    ui.add(Label::new(RichText::new(TRUST).font(theme::body(theme::SMALL)).color(pal.warn.fg)).wrap());
    let http = s(row, "transport") == "http";
    ui.label(
        RichText::new(if http {
            "点击按钮将实际连接此服务地址并读取工具目录。"
        } else {
            "点击按钮将实际启动已保存的命令并读取其工具目录。"
        })
        .font(theme::body(theme::SMALL))
        .color(pal.mute),
    );
    let launch = if http {
        [s(row, "url").to_string(), if s(row, "tokenEnv").is_empty() { String::new() } else { format!("令牌环境变量：{}", s(row, "tokenEnv")) }]
            .into_iter()
            .filter(|x| !x.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        let mut v = vec![row["command"].clone()];
        v.extend(row["args"].as_array().cloned().unwrap_or_default());
        serde_json::to_string_pretty(&v).unwrap_or_default()
    };
    ui.label(RichText::new(launch).monospace().size(12.0));
    ui.add_space(6.0);
    if ui
        .add_enabled_ui(!busy, |ui| widgets::quiet_button(ui, icon::PULSE, "连接并读取工具", pal, true))
        .inner
        .clicked()
    {
        let id = s(row, "id").to_string();
        *result = None;
        *next = Some((Task::Probe, Box::new(move |ext, rt| rt.block_on(ext.probe(&id)))));
    }
    match result {
        None if busy => {
            ui.label(RichText::new("正在连接服务并读取工具目录…").color(pal.mute));
        }
        None => {}
        Some(Err(e)) => {
            ui.label(RichText::new(e.as_str()).color(pal.fail.fg));
        }
        Some(Ok(tools)) => {
            let tools = tools.as_array().cloned().unwrap_or_default();
            ui.label(RichText::new(format!("发现 {} 个工具", tools.len())).strong());
            for t in tools {
                ui.label(RichText::new(s(&t, "name")).strong());
                ui.label(
                    RichText::new(labels::compact(s(&t, "description"), 200))
                        .font(theme::body(theme::SMALL))
                        .color(pal.mute),
                );
            }
        }
    }
}
