//! Operator-managed providers and rotation (operator requirement
//! "供应商管理与轮换"). Providers point Claude Code, Codex or DeepSeek
//! Harness at an extra endpoint; keys stay in environment variables and are
//! never stored. Rotation lists, per harness, the routes to switch to when
//! the current one fails (`route.rotated`).

use crate::app::App;
use crate::host::Action;
use crate::labels::agent_name;
use crate::theme;
use crate::widgets;
use egui::{Align, CornerRadius, Frame, Label, Layout, Margin, RichText, Stroke, Ui};
use egui_phosphor::regular as icon;
use sa_store::providers::{CUSTOM_AGENTS, CustomProvider, ROTATION_AGENTS, Rotation, RouteRef};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Providers,
    Rotation,
}

#[derive(Clone, Default)]
pub struct Form {
    pub id: String,
    pub agent: String,
    pub name: String,
    pub base_url: String,
    pub key_env: String,
    /// One model id per line.
    pub models: String,
    pub enabled: bool,
}

impl Form {
    fn from(p: &CustomProvider) -> Self {
        Self {
            id: p.id.clone(),
            agent: p.agent.clone(),
            name: p.name.clone(),
            base_url: p.base_url.clone(),
            key_env: p.key_env.clone(),
            models: p.models.join("\n"),
            enabled: p.enabled,
        }
    }

    fn provider(&self) -> CustomProvider {
        CustomProvider {
            id: self.id.clone(),
            agent: self.agent.clone(),
            name: self.name.trim().to_string(),
            base_url: self.base_url.trim().to_string(),
            key_env: self.key_env.trim().to_string(),
            models: self
                .models
                .lines()
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(str::to_string)
                .collect(),
            enabled: self.enabled,
        }
    }
}

pub struct ProvidersDialog {
    pub page: Page,
    pub form: Option<Form>,
    pub rotation: Rotation,
    catalogs: HashMap<String, Option<Result<Value, String>>>,
    /// Pending rotation pick per agent: `(provider, model)`.
    picks: HashMap<String, (String, String)>,
    pub probing: bool,
    pub saving: bool,
    pub message: Option<(String, bool)>,
}

impl ProvidersDialog {
    pub fn needs(&self, agent: &str) -> bool {
        matches!(self.catalogs.get(agent), Some(None))
    }

    pub fn answer(&mut self, agent: &str, result: Result<Value, String>) {
        if self.needs(agent) {
            self.catalogs.insert(agent.to_string(), Some(result));
        }
    }

    pub fn probed(&mut self, result: Result<Vec<String>, String>) {
        self.probing = false;
        match (result, self.form.as_mut()) {
            (Ok(models), Some(f)) => {
                self.message = Some((format!("已获取 {} 个模型", models.len()), false));
                f.models = models.join("\n");
            }
            (Ok(_), None) => {}
            (Err(e), _) => self.message = Some((e, true)),
        }
    }
}

fn field(ui: &mut Ui, label: &str, value: &mut String, hint: &str) {
    ui.horizontal(|ui| {
        ui.add_sized([96.0, 24.0], Label::new(RichText::new(label).font(theme::body(13.0))));
        let r = ui.add(
            egui::TextEdit::singleline(value)
                .hint_text(hint)
                .desired_width(ui.available_width()),
        );
        r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, label));
    });
}

fn route_name(catalog: Option<&Value>, r: &RouteRef) -> String {
    let label = catalog
        .and_then(|c| c.get("providers"))
        .and_then(Value::as_array)
        .and_then(|ps| ps.iter().find(|p| p.get("id").and_then(Value::as_str) == Some(&r.provider)))
        .and_then(|p| p.get("label").and_then(Value::as_str))
        .unwrap_or(&r.provider)
        .to_string();
    format!("{label} / {}", r.model)
}

impl App {
    pub fn open_providers(&mut self) {
        let Some(host) = &self.host else { return };
        self.providers = Some(ProvidersDialog {
            page: Page::Providers,
            form: None,
            rotation: host.rotation(),
            catalogs: HashMap::new(),
            picks: HashMap::new(),
            probing: false,
            saving: false,
            message: None,
        });
    }

    fn rotation_catalog(&mut self, agent: &str) {
        let (Some(host), Some(d)) = (&self.host, self.providers.as_mut()) else { return };
        if d.catalogs.contains_key(agent) {
            return;
        }
        d.catalogs.insert(agent.to_string(), None);
        host.request_catalog(agent);
    }

    /// `ActionKind::Providers` outcome.
    pub fn providers_saved(&mut self, result: Result<(), String>) {
        let Some(d) = self.providers.as_mut() else {
            if let Err(e) = result {
                self.composer.message = Some((e, true));
            }
            return;
        };
        d.saving = false;
        match result {
            Ok(()) => {
                d.form = None;
                d.message = Some(("已保存。新的设置从下一轮开始生效。".into(), false));
                // Catalogs change with the registry.
                d.catalogs.clear();
                if let Some(h) = &self.host {
                    d.rotation = h.rotation();
                }
            }
            Err(e) => d.message = Some((e, true)),
        }
    }

    pub fn providers_dialog(&mut self, ctx: &egui::Context) {
        if self.providers.is_none() {
            return;
        }
        let pal = self.pal.clone();
        let list = self.host.as_ref().map(|h| h.providers()).unwrap_or_default();
        let mut submit: Option<Action> = None;
        let mut probe: Option<CustomProvider> = None;
        let mut load: Vec<String> = Vec::new();
        let mut close = false;
        let busy = self.composer.pending > 0;
        let d = self.providers.as_mut().expect("checked");
        let modal = egui::Modal::new(egui::Id::new("sa-providers"))
            .frame(widgets::modal_frame(&pal))
            .show(ctx, |ui| {
                ui.set_width(620.0);
                if widgets::modal_header(ui, icon::PLUGS, "供应商与轮换", "添加兼容端点；某个供应商不可用时按顺序自动改用下一个。", &pal) {
                    close = true;
                }
                let pages = [Page::Providers, Page::Rotation];
                let current = pages.iter().position(|p| *p == d.page).unwrap_or(0);
                if let Some(i) = widgets::segmented(ui, &["自定义供应商", "轮换"], current, &pal) {
                    d.page = pages[i];
                    d.message = None;
                }
                ui.add_space(10.0);
                let max = (ui.ctx().content_rect().height() - 220.0).max(240.0);
                egui::ScrollArea::vertical().max_height(max).min_scrolled_height(max).show(ui, |ui| match d.page {
                    Page::Providers => providers_page(ui, d, &list, &pal, &mut submit, &mut probe, busy),
                    Page::Rotation => rotation_page(ui, d, &pal, &mut submit, &mut load, busy),
                });
                if let Some((text, error)) = &d.message {
                    ui.add_space(8.0);
                    let color = if *error { pal.fail.fg } else { pal.ok.fg };
                    ui.add(Label::new(RichText::new(text).font(theme::body(theme::SMALL)).color(color)).wrap());
                }
            });
        if modal.should_close() {
            close = true;
        }
        if close {
            self.providers = None;
            return;
        }
        if let Some(p) = probe
            && let Some(h) = &self.host
        {
            if let Some(d) = self.providers.as_mut() {
                d.probing = true;
                d.message = None;
            }
            h.probe_models(p);
        }
        for a in load {
            self.rotation_catalog(&a);
        }
        if let Some(a) = submit {
            if let Some(d) = self.providers.as_mut() {
                d.saving = true;
                d.message = None;
            }
            self.submit(a);
        }
    }
}

fn providers_page(
    ui: &mut Ui,
    d: &mut ProvidersDialog,
    list: &[CustomProvider],
    pal: &theme::Palette,
    submit: &mut Option<Action>,
    probe: &mut Option<CustomProvider>,
    busy: bool,
) {
    ui.add(
        Label::new(
            RichText::new(
                "为 Claude Code、Codex 或 DeepSeek Harness 添加兼容端点。密钥只从环境变量读取，Super Agent 只保存变量名；保存后出现在“模型与执行端”的供应商列表中。",
            )
            .font(theme::body(theme::SMALL))
            .color(pal.mute),
        )
        .wrap(),
    );
    ui.add_space(8.0);
    let (probing, saving) = (d.probing, d.saving);
    let mut cancel = false;
    if let Some(f) = d.form.as_mut() {
        Frame::new()
            .fill(pal.canvas)
            .stroke(Stroke::new(1.0_f32, pal.line_soft))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.add_sized([96.0, 24.0], Label::new(RichText::new("执行端").font(theme::body(13.0))));
                    ui.add_enabled_ui(f.id.is_empty(), |ui| {
                        widgets::combo("provider-agent")
                            .selected_text(agent_name(&f.agent))
                            .show_ui(ui, |ui| {
                                for a in CUSTOM_AGENTS {
                                    ui.selectable_value(&mut f.agent, a.to_string(), agent_name(a));
                                }
                            });
                    });
                });
                field(ui, "名称", &mut f.name, "例如 公司网关");
                field(ui, "接口地址", &mut f.base_url, "https://api.example.com/v1");
                field(ui, "密钥环境变量", &mut f.key_env, "例如 EXAMPLE_API_KEY（本机端点可留空）");
                ui.horizontal(|ui| {
                    ui.add_sized([96.0, 24.0], Label::new(RichText::new("模型").font(theme::body(13.0))));
                    let r = ui.add(
                        egui::TextEdit::multiline(&mut f.models)
                            .hint_text("每行一个模型 ID")
                            .desired_rows(3)
                            .desired_width(ui.available_width()),
                    );
                    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "模型"));
                });
                widgets::toggle(ui, &mut f.enabled, "启用", pal);
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let can_probe = !probing && !f.base_url.trim().is_empty();
                    if ui
                        .add_enabled_ui(can_probe, |ui| {
                            widgets::quiet_button(ui, icon::ARROWS_CLOCKWISE, "获取模型", pal, false)
                        })
                        .inner
                        .on_hover_text("从接口的 /v1/models 读取模型列表")
                        .clicked()
                    {
                        *probe = Some(f.provider());
                    }
                    if probing {
                        ui.spinner();
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let ok = !busy && !saving && !f.name.trim().is_empty() && !f.base_url.trim().is_empty();
                        if ui
                            .add_enabled_ui(ok, |ui| widgets::primary_button(ui, icon::CHECK, "保存供应商", pal, ok))
                            .inner
                            .clicked()
                        {
                            *submit = Some(Action::ProviderSave { provider: f.provider() });
                        }
                        if widgets::quiet_button(ui, "", "取消", pal, false).clicked() {
                            cancel = true;
                        }
                    });
                });
            });
        ui.add_space(10.0);
        if cancel {
            d.form = None;
        }
    } else if widgets::quiet_button(ui, icon::PLUS, "添加供应商", pal, true).clicked() {
        d.form = Some(Form {
            agent: "claude".into(),
            enabled: true,
            ..Default::default()
        });
        d.message = None;
    }
    ui.add_space(6.0);
    if list.is_empty() {
        ui.label(RichText::new("还没有自定义供应商。").color(pal.mute));
    }
    for p in list {
        Frame::new()
            .fill(pal.surface)
            .stroke(Stroke::new(1.0_f32, pal.line_soft))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&p.name).font(theme::strong(13.5)).color(pal.ink));
                    widgets::chip(ui, &agent_name(&p.agent), pal.neutral, theme::TINY);
                    if !p.enabled {
                        widgets::chip(ui, "已停用", pal.warn, theme::TINY);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let del = widgets::icon_button(ui, icon::TRASH, 24.0, pal.mute, pal.hover, false)
                            .on_hover_text("删除");
                        let label = format!("删除供应商 {}", p.name);
                        del.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
                        if del.clicked() && !busy {
                            *submit = Some(Action::ProviderRemove { id: p.id.clone() });
                        }
                        let edit = widgets::icon_button(ui, icon::PENCIL_SIMPLE, 24.0, pal.mute, pal.hover, false)
                            .on_hover_text("编辑");
                        let label = format!("编辑供应商 {}", p.name);
                        edit.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
                        if edit.clicked() {
                            d.form = Some(Form::from(p));
                            d.message = None;
                        }
                    });
                });
                let key = if p.key_env.is_empty() { "无需密钥".to_string() } else { format!("密钥变量 {}", p.key_env) };
                ui.label(
                    RichText::new(format!("{} · {key} · {} 个模型", p.base_url, p.models.len()))
                        .font(theme::body(theme::TINY + 0.5))
                        .color(pal.mute),
                );
            });
        ui.add_space(6.0);
    }
}

fn rotation_page(
    ui: &mut Ui,
    d: &mut ProvidersDialog,
    pal: &theme::Palette,
    submit: &mut Option<Action>,
    load: &mut Vec<String>,
    busy: bool,
) {
    widgets::toggle(ui, &mut d.rotation.enabled, "启用供应商轮换", pal);
    ui.add(
        Label::new(
            RichText::new(
                "执行端连续失败（限流、过载、认证或额度问题、供应商不可用）时，按下列顺序改用下一条路由重试；不会续接原会话。未列出的执行端不轮换。",
            )
            .font(theme::body(theme::SMALL))
            .color(pal.mute),
        )
        .wrap(),
    );
    ui.add_space(8.0);
    for agent in ROTATION_AGENTS {
        let catalog = match d.catalogs.get(agent) {
            Some(Some(Ok(c))) => Some(c.clone()),
            _ => None,
        };
        if !d.catalogs.contains_key(agent) {
            load.push(agent.to_string());
        }
        ui.label(RichText::new(agent_name(agent)).font(theme::strong(13.5)).color(pal.ink));
        let order = d.rotation.order.entry(agent.to_string()).or_default();
        let mut swap: Option<(usize, usize)> = None;
        let mut remove: Option<usize> = None;
        let n = order.len();
        for (i, r) in order.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{}.", i + 1)).color(pal.faint));
                ui.label(RichText::new(route_name(catalog.as_ref(), r)).font(theme::body(13.0)));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::icon_button(ui, icon::X, 22.0, pal.mute, pal.hover, false).on_hover_text("移除").clicked() {
                        remove = Some(i);
                    }
                    if i + 1 < n
                        && widgets::icon_button(ui, icon::ARROW_DOWN, 22.0, pal.mute, pal.hover, false)
                            .on_hover_text("下移")
                            .clicked()
                    {
                        swap = Some((i, i + 1));
                    }
                    if i > 0
                        && widgets::icon_button(ui, icon::ARROW_UP, 22.0, pal.mute, pal.hover, false)
                            .on_hover_text("上移")
                            .clicked()
                    {
                        swap = Some((i - 1, i));
                    }
                });
            });
        }
        if let Some((a, b)) = swap {
            order.swap(a, b);
        }
        if let Some(i) = remove {
            order.remove(i);
        }
        match d.catalogs.get(agent) {
            Some(Some(Err(e))) => {
                ui.label(RichText::new(e).font(theme::body(theme::TINY + 0.5)).color(pal.fail.fg));
            }
            None | Some(None) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("正在读取可用路由…").color(pal.mute));
                });
            }
            Some(Some(Ok(c))) => {
                let providers: Vec<&Value> = c
                    .get("providers")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter(|p| p.get("available") != Some(&Value::Bool(false))).collect())
                    .unwrap_or_default();
                let pick = d.picks.entry(agent.to_string()).or_default();
                let id = |v: &Value| v.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                let label = |v: &Value| v.get("label").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| id(v));
                ui.horizontal(|ui| {
                    let current = providers.iter().find(|p| id(p) == pick.0);
                    widgets::combo(("rot-provider", agent))
                        .width(200.0)
                        .selected_text(current.map(|p| label(p)).unwrap_or_else(|| "选择供应商".into()))
                        .show_ui(ui, |ui| {
                            for p in &providers {
                                if ui.selectable_label(id(p) == pick.0, label(p)).clicked() {
                                    pick.0 = id(p);
                                    pick.1.clear();
                                }
                            }
                        });
                    let models: Vec<String> = current
                        .and_then(|p| p.get("models"))
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter(|m| m.get("available") != Some(&Value::Bool(false)))
                                .map(id)
                                .collect()
                        })
                        .unwrap_or_default();
                    widgets::combo(("rot-model", agent))
                        .width(170.0)
                        .selected_text(if pick.1.is_empty() { "选择模型".to_string() } else { pick.1.clone() })
                        .show_ui(ui, |ui| {
                            for m in &models {
                                ui.selectable_value(&mut pick.1, m.clone(), m);
                            }
                        });
                    let route = RouteRef {
                        provider: pick.0.clone(),
                        model: pick.1.clone(),
                        mode: "cli".into(),
                    };
                    let ok = !route.provider.is_empty() && !route.model.is_empty() && !order.contains(&route);
                    if ui
                        .add_enabled_ui(ok, |ui| widgets::quiet_button(ui, icon::PLUS, "加入", pal, false))
                        .inner
                        .clicked()
                    {
                        order.push(route);
                    }
                });
            }
        }
        ui.add_space(10.0);
    }
    d.rotation.order.retain(|_, v| !v.is_empty());
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if ui
            .add_enabled_ui(!busy && !d.saving, |ui| widgets::primary_button(ui, icon::CHECK, "保存轮换设置", pal, !busy && !d.saving))
            .inner
            .clicked()
        {
            *submit = Some(Action::RotationSave {
                rotation: d.rotation.clone(),
            });
        }
    });
}
