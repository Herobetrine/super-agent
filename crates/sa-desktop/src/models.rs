//! Model and harness selection (`desktop/ui/models.js`): per-agent
//! `{mode, provider, model}` routes for new-conversation defaults, the draft
//! of a new conversation, or a stopped ticket (`saveRoutes`).
//!
//! Catalogs are probed through [`Host::request_catalog`](crate::host::Host)
//! and arrive in [`App::poll_host`]; nothing is cached across dialogs.

use crate::app::App;
use crate::host::Action;
use crate::labels::{self, agent_name};
use crate::theme;
use crate::widgets;
use egui::{Align, CornerRadius, Frame, Label, Layout, Margin, RichText, Stroke, Ui};
use egui_phosphor::regular as icon;
use sa_engine::adapter::AGENTS;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// `route_defaults`: routes for every new conversation.
    Defaults,
    /// The composer's next new conversation.
    Draft,
    /// A ticket's `agent_routes` (only while every job is terminal).
    Ticket(i64),
}

enum Probe {
    Loading,
    Ready(Value),
    Failed(String),
}

pub struct ModelDialog {
    pub scope: Scope,
    chosen: Map<String, Value>,
    agent: String,
    catalogs: HashMap<String, Probe>,
    locked: bool,
    pub warning: Option<String>,
    pub saving: bool,
}

fn rows<'a>(v: &'a Value, key: &str) -> Vec<&'a Value> {
    v.get(key).and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default()
}

fn id_of(v: &Value) -> &str {
    v.get("id").and_then(Value::as_str).unwrap_or_default()
}

fn available(v: &Value) -> bool {
    v.get("available") != Some(&Value::Bool(false))
}

fn label_of(v: &Value) -> String {
    v.get("label")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| id_of(v))
        .to_string()
}

fn pick_str<'a>(pick: &'a Value, key: &str) -> &'a str {
    pick.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Providers allowed by the mode row (all when it lists none).
fn providers_for<'a>(c: &'a Value, mode: &str) -> Vec<&'a Value> {
    let allowed = rows(c, "modes")
        .into_iter()
        .find(|m| id_of(m) == mode)
        .and_then(|m| m.get("providers").and_then(Value::as_array));
    rows(c, "providers")
        .into_iter()
        .filter(|p| allowed.is_none_or(|a| a.iter().any(|x| x.as_str() == Some(id_of(p)))))
        .collect()
}

/// Whether `pick` is a usable combination of catalog `c` (`validate`, MS:201-218).
fn valid(c: &Value, pick: &Value) -> bool {
    let (mode, provider, model) = (pick_str(pick, "mode"), pick_str(pick, "provider"), pick_str(pick, "model"));
    let Some(m) = rows(c, "modes").into_iter().find(|m| id_of(m) == mode) else { return false };
    let Some(p) = rows(c, "providers").into_iter().find(|p| id_of(p) == provider) else { return false };
    available(m)
        && available(p)
        && providers_for(c, mode).iter().any(|x| id_of(x) == provider)
        && rows(p, "models").iter().any(|x| id_of(x) == model && available(x))
}

/// First available provider of `mode` and its default (or first available) model.
fn reset(c: &Value, mode: &str, provider: Option<&str>) -> Value {
    let providers = providers_for(c, mode);
    let p = match provider {
        Some(id) => providers.iter().find(|p| id_of(p) == id).copied(),
        None => providers.iter().find(|p| available(p)).copied(),
    };
    let model = p
        .map(|p| {
            let models = rows(p, "models");
            let default = p.get("defaultModel").and_then(Value::as_str);
            models
                .iter()
                .find(|m| Some(id_of(m)) == default && available(m))
                .or_else(|| models.iter().find(|m| available(m)))
                .map(|m| id_of(m).to_string())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    json!({"mode": mode, "provider": p.map(id_of).unwrap_or_default(), "model": model})
}

/// One labeled combo box; unavailable rows are disabled and an unknown
/// current id is shown as "当前不可用" (`fill`, MS:4-11).
fn combo(ui: &mut Ui, label: &str, id: &str, options: &[&Value], current: &str, locked: bool) -> Option<String> {
    let mut picked = None;
    ui.horizontal(|ui| {
        ui.add_sized(
            [64.0, 24.0],
            Label::new(RichText::new(label).font(theme::body(13.0))),
        );
        let text = options
            .iter()
            .find(|o| id_of(o) == current)
            .map(|o| label_of(o))
            .unwrap_or_else(|| {
                if current.is_empty() {
                    "没有可用选项".into()
                } else {
                    format!("{current} · 当前不可用")
                }
            });
        ui.add_enabled_ui(!locked, |ui| {
            let resp = egui::ComboBox::from_id_salt(("model-combo", id))
                .width(ui.available_width().min(380.0))
                .selected_text(text)
                .show_ui(ui, |ui| {
                    for o in options {
                        let oid = id_of(o);
                        let mut name = label_of(o);
                        if !available(o) {
                            name.push_str(" · 不可用");
                        }
                        let r = ui.add_enabled(available(o), egui::Button::selectable(oid == current, name));
                        if let Some(d) = o.get("detail").and_then(Value::as_str).filter(|d| !d.is_empty()) {
                            r.clone().on_hover_text(d);
                        }
                        if r.clicked() && oid != current {
                            picked = Some(oid.to_string());
                        }
                    }
                });
            resp.response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, !locked, label));
        });
    });
    picked
}

impl ModelDialog {
    pub fn needs(&self, agent: &str) -> bool {
        matches!(self.catalogs.get(agent), Some(Probe::Loading))
    }

    pub fn answer(&mut self, agent: &str, result: Result<Value, String>) {
        if !self.needs(agent) {
            return;
        }
        let probe = match result {
            Ok(v) => Probe::Ready(v),
            Err(e) => Probe::Failed(if e.is_empty() { "获取失败".into() } else { e }),
        };
        self.catalogs.insert(agent.to_string(), probe);
    }

    /// Save is allowed when every explicit route is valid in its loaded
    /// catalog; unloaded agents other than the visible one are not blocked.
    fn savable(&self) -> bool {
        !self.saving
            && self.chosen.iter().all(|(a, pick)| match self.catalogs.get(a) {
                Some(Probe::Ready(c)) => valid(c, pick),
                Some(_) => false,
                None => *a != self.agent,
            })
    }
}

impl App {
    /// Open the model dialog for `scope` (writable windows only).
    pub fn open_models(&mut self, scope: Scope) {
        let Some(host) = &self.host else { return };
        let snap = self.view.as_ref().and_then(|v| v.snap.as_ref());
        let (chosen, locked, agent) = match &scope {
            Scope::Defaults => (host.defaults(), false, self.composer.lead.clone()),
            Scope::Draft => (
                self.composer.routes.clone().unwrap_or_else(|| host.defaults()),
                false,
                self.composer.lead.clone(),
            ),
            Scope::Ticket(id) => {
                let Some(s) = snap.filter(|s| s.ticket.id == *id) else { return };
                let locked = s.jobs.iter().any(|j| labels::is_active(&j.state) || j.state == "interrupted");
                (s.ticket.agent_routes.clone(), locked, s.ticket.lead.clone())
            }
        };
        let chosen = match chosen {
            Value::Object(m) => m,
            Value::String(s) => serde_json::from_str(&s).unwrap_or_default(),
            _ => Map::new(),
        };
        self.models = Some(ModelDialog {
            scope,
            chosen,
            agent: agent.clone(),
            catalogs: HashMap::new(),
            locked,
            warning: None,
            saving: false,
        });
        self.load_catalog(&agent);
    }

    fn load_catalog(&mut self, agent: &str) {
        let (Some(host), Some(d)) = (&self.host, self.models.as_mut()) else { return };
        if d.needs(agent) {
            return;
        }
        d.catalogs.insert(agent.to_string(), Probe::Loading);
        host.request_catalog(agent);
    }

    /// Short summary of the explicit routes, for button tooltips.
    pub fn routes_summary(routes: Option<&Value>) -> String {
        let Some(m) = routes.and_then(Value::as_object).filter(|m| !m.is_empty()) else {
            return "跟随本机适配器默认配置".into();
        };
        m.iter()
            .map(|(a, r)| format!("{}：{} / {}", agent_name(a), pick_str(r, "provider"), pick_str(r, "model")))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn models_dialog(&mut self, ctx: &egui::Context) {
        let Some(d) = self.models.as_mut() else { return };
        let pal = self.pal.clone();
        let title = match d.scope {
            Scope::Defaults => "模型与执行端 · 新会话默认",
            Scope::Draft => "模型与执行端 · 本次新会话",
            Scope::Ticket(_) => "本次会话的模型与执行端",
        };
        let mut switch_to: Option<String> = None;
        let mut refresh = false;
        let mut close = false;
        let mut save = false;
        let modal = egui::Modal::new(egui::Id::new("sa-models"))
            .frame(
                Frame::new()
                    .fill(pal.surface)
                    .stroke(Stroke::new(1.0_f32, pal.line))
                    .corner_radius(CornerRadius::same(14))
                    .inner_margin(Margin::same(20)),
            )
            .show(ctx, |ui| {
                ui.set_width(520.0);
                ui.label(RichText::new(title).font(theme::strong(16.0)).color(pal.ink));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for a in AGENTS {
                        let explicit = d.chosen.contains_key(a);
                        let name = if explicit { format!("{} ·", agent_name(a)) } else { agent_name(a).to_string() };
                        if widgets::quiet_button(ui, "", &name, &pal, d.agent == a)
                            .on_hover_text(if explicit { "已指定供应商与模型" } else { "跟随本机默认配置" })
                            .clicked()
                            && d.agent != a
                        {
                            switch_to = Some(a.to_string());
                        }
                    }
                });
                ui.add_space(10.0);
                let agent = d.agent.clone();
                match d.catalogs.get(&agent) {
                    None | Some(Probe::Loading) => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(RichText::new("正在读取本机配置…").color(pal.mute));
                        });
                    }
                    Some(Probe::Failed(e)) => {
                        ui.add(Label::new(RichText::new(e).color(pal.fail.fg)).wrap());
                        if widgets::quiet_button(ui, icon::ARROWS_CLOCKWISE, "刷新目录", &pal, false).clicked() {
                            refresh = true;
                        }
                    }
                    Some(Probe::Ready(c)) => {
                        let c = c.clone();
                        let follow = !d.chosen.contains_key(&agent);
                        let default = c.get("default").cloned().unwrap_or_else(|| json!({}));
                        let pick = d.chosen.get(&agent).cloned().unwrap_or_else(|| default.clone());
                        let mut follow_now = follow;
                        ui.add_enabled_ui(!d.locked, |ui| {
                            ui.checkbox(&mut follow_now, "跟随本机适配器默认配置");
                        });
                        if let Some(detail) = c.get("detail").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                            ui.add(Label::new(RichText::new(detail).font(theme::body(theme::SMALL)).color(pal.mute)).wrap());
                        }
                        ui.add_space(6.0);
                        let mode = pick_str(&pick, "mode").to_string();
                        let provider = pick_str(&pick, "provider").to_string();
                        let modes = rows(&c, "modes");
                        let providers = providers_for(&c, &mode);
                        let models = providers
                            .iter()
                            .find(|p| id_of(p) == provider)
                            .map(|p| rows(p, "models"))
                            .unwrap_or_default();
                        let mut next: Option<Value> = None;
                        if let Some(m) = combo(ui, "执行端", "mode", &modes, &mode, d.locked) {
                            next = Some(reset(&c, &m, None));
                        }
                        if let Some(p) = combo(ui, "供应商", "provider", &providers, &provider, d.locked) {
                            next = Some(reset(&c, &mode, Some(&p)));
                        }
                        if let Some(m) = combo(ui, "模型", "model", &models, pick_str(&pick, "model"), d.locked) {
                            next = Some(json!({"mode": mode, "provider": provider, "model": m}));
                        }
                        if follow_now != follow {
                            if follow_now {
                                d.chosen.remove(&agent);
                            } else {
                                d.chosen.insert(agent.clone(), pick.clone());
                            }
                        }
                        if let Some(n) = next {
                            d.chosen.insert(agent.clone(), n);
                        }
                        let shown = d.chosen.get(&agent).cloned().unwrap_or(default);
                        let mut notes: Vec<String> = [
                            modes.iter().find(|m| id_of(m) == pick_str(&shown, "mode")),
                            rows(&c, "providers").iter().find(|p| id_of(p) == pick_str(&shown, "provider")),
                        ]
                        .into_iter()
                        .flatten()
                        .filter_map(|r| r.get("detail").and_then(Value::as_str).filter(|s| !s.is_empty()))
                        .map(str::to_string)
                        .collect();
                        if !d.chosen.contains_key(&agent) {
                            notes.push("当前仍跟随本机默认配置，选择供应商或模型后会自动切换为明确配置。".into());
                        } else if !valid(&c, &shown) {
                            notes.push("这个组合不可用，请更换选项或跟随默认配置。".into());
                        }
                        if !notes.is_empty() {
                            ui.add_space(4.0);
                            ui.add(
                                Label::new(RichText::new(notes.join(" · ")).font(theme::body(theme::SMALL)).color(pal.mute))
                                    .wrap(),
                            );
                        }
                        ui.horizontal_wrapped(|ui| {
                            for l in rows(&c, "downloads") {
                                let (Some(label), Some(url)) =
                                    (l.get("label").and_then(Value::as_str), l.get("url").and_then(Value::as_str))
                                else {
                                    continue;
                                };
                                if url.starts_with("https://") {
                                    ui.hyperlink_to(label, url);
                                }
                            }
                            if widgets::quiet_button(ui, icon::ARROWS_CLOCKWISE, "刷新", &pal, false).clicked() {
                                refresh = true;
                            }
                        });
                    }
                }
                ui.add_space(12.0);
                let status = if d.locked {
                    "会话中仍有活动任务，当前配置只读。".to_string()
                } else if let Some(w) = &d.warning {
                    w.clone()
                } else {
                    "保存后只使用选中的组合。".to_string()
                };
                let tone = if d.warning.is_some() { pal.fail.fg } else { pal.mute };
                ui.add(Label::new(RichText::new(status).font(theme::body(theme::SMALL)).color(tone)).wrap());
                ui.add_space(10.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if !d.locked {
                        let ok = d.savable();
                        if ui
                            .add_enabled_ui(ok, |ui| widgets::quiet_button(ui, icon::CHECK, "保存选择", &pal, true))
                            .inner
                            .clicked()
                        {
                            save = true;
                        }
                        if d.saving {
                            ui.spinner();
                        }
                    }
                    if widgets::quiet_button(ui, "", if d.locked { "关闭" } else { "取消" }, &pal, false).clicked() {
                        close = true;
                    }
                });
            });
        if modal.should_close() && !save {
            close = true;
        }
        if close {
            self.models = None;
            return;
        }
        if let Some(a) = switch_to {
            if let Some(d) = self.models.as_mut() {
                d.agent = a.clone();
                d.warning = None;
            }
            if !matches!(self.models.as_ref().and_then(|d| d.catalogs.get(&a)), Some(Probe::Ready(_))) {
                self.load_catalog(&a);
            }
        }
        if refresh {
            let a = self.models.as_ref().map(|d| d.agent.clone()).unwrap_or_default();
            if let Some(d) = self.models.as_mut() {
                d.catalogs.remove(&a);
            }
            self.load_catalog(&a);
        }
        if save {
            self.save_models();
        }
    }

    fn save_models(&mut self) {
        let Some(d) = self.models.as_mut() else { return };
        let selections = Value::Object(d.chosen.clone());
        match d.scope.clone() {
            Scope::Draft => {
                self.composer.routes = Some(selections);
                self.composer.message = Some(("本次新会话的模型与执行端已设置。".into(), false));
                self.models = None;
            }
            Scope::Defaults => {
                d.saving = true;
                self.submit(Action::SaveDefaults { selections });
            }
            Scope::Ticket(ticket) => {
                d.saving = true;
                self.submit(Action::SaveRoutes { ticket, selections });
            }
        }
    }

    /// `ActionKind::Routes` outcome.
    pub fn routes_saved(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => {
                self.models = None;
                self.composer.message = Some(("模型与执行端设置已保存。".into(), false));
            }
            Err(e) => match self.models.as_mut() {
                Some(d) => {
                    d.saving = false;
                    d.warning = Some(e);
                }
                None => self.composer.message = Some((e, true)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Value {
        json!({
            "default": {"mode": "cli", "provider": "a", "model": "a1"},
            "modes": [{"id": "cli", "available": true}, {"id": "desktop", "available": false}],
            "providers": [
                {"id": "a", "models": [{"id": "a1"}, {"id": "a2", "available": false}]},
                {"id": "b", "available": true, "defaultModel": "b2", "models": [{"id": "b1"}, {"id": "b2"}]},
                {"id": "c", "available": false, "models": [{"id": "c1"}]}
            ]
        })
    }

    #[test]
    fn validity_and_resets_follow_the_electron_picker() {
        let c = catalog();
        assert!(valid(&c, &json!({"mode": "cli", "provider": "a", "model": "a1"})));
        assert!(!valid(&c, &json!({"mode": "cli", "provider": "a", "model": "a2"})));
        assert!(!valid(&c, &json!({"mode": "desktop", "provider": "a", "model": "a1"})));
        assert!(!valid(&c, &json!({"mode": "cli", "provider": "c", "model": "c1"})));
        assert!(!valid(&c, &json!({"mode": "cli", "provider": "x", "model": "a1"})));
        assert_eq!(reset(&c, "cli", None), json!({"mode": "cli", "provider": "a", "model": "a1"}));
        assert_eq!(reset(&c, "cli", Some("b")), json!({"mode": "cli", "provider": "b", "model": "b2"}));
    }
}
