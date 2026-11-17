//! Global context-delivery and Claude cache preferences.

use crate::{app::App, host::Action, ops::Pending, theme, widgets};
use egui::{RichText, Ui};
use sa_store::efficiency::{self, Settings, Ttl};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct ObserverEfficiency {
    settings: Option<Settings>,
    loading: Option<Pending<Settings>>,
    checked: Option<Instant>,
}

fn read_settings(ctx: &egui::Context, data: std::path::PathBuf) -> Pending<Settings> {
    Pending::spawn(ctx, move || {
        let conn = rusqlite::Connection::open_with_flags(
            data.join(sa_store::DB_FILE),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        efficiency::load(&conn)
    })
}

pub struct EfficiencyDialog {
    pub draft: Settings,
    pub saving: bool,
    pub error: Option<String>,
    loading: Option<Pending<Settings>>,
    ready: bool,
}

fn description(ui: &mut Ui, text: &str, pal: &theme::Palette) {
    ui.add(
        egui::Label::new(
            RichText::new(text)
                .font(theme::body(theme::SMALL))
                .color(pal.mute),
        )
        .wrap(),
    );
}

impl App {
    /// Read saved settings without blocking a frame or exposing dialog drafts.
    pub(crate) fn saved_efficiency(&mut self) -> Option<Settings> {
        if let Some(host) = &self.host {
            return Some(host.efficiency_settings());
        }
        if self.opts.demo {
            return Some(Settings::default());
        }
        let cache = &mut self.observer_efficiency;
        if let Some(result) = cache.loading.as_ref().and_then(Pending::poll) {
            cache.loading = None;
            if let Ok(settings) = result {
                cache.settings = Some(settings);
            }
        }
        if cache.loading.is_none()
            && cache.checked.is_none_or(|at| at.elapsed() >= Duration::from_secs(5))
        {
            let data = self.opts.data.clone().unwrap_or_else(sa_store::default_data_dir);
            cache.loading = Some(read_settings(&self.ctx, data));
            cache.checked = Some(Instant::now());
        }
        self.ctx.request_repaint_after(Duration::from_secs(5));
        cache.settings.clone()
    }

    pub fn open_efficiency(&mut self) {
        if self.efficiency.is_some() {
            return;
        }
        let settings = self.host.as_ref().map(|host| host.efficiency_settings());
        let ready = settings.is_some() || self.opts.demo;
        let loading = if ready {
            None
        } else {
            let dir = self
                .opts
                .data
                .clone()
                .unwrap_or_else(sa_store::default_data_dir);
            Some(read_settings(&self.ctx, dir))
        };
        self.efficiency = Some(EfficiencyDialog {
            draft: settings.unwrap_or_default(),
            saving: false,
            error: None,
            loading,
            ready,
        });
    }

    pub fn efficiency_saved(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => {
                self.efficiency = None;
                self.composer.message =
                    Some(("Token 与缓存设置已保存，将用于后续轮次。".into(), false));
            }
            Err(error) => {
                if let Some(dialog) = self.efficiency.as_mut() {
                    dialog.saving = false;
                    dialog.error = Some(error);
                } else {
                    self.composer.message = Some((error, true));
                }
            }
        }
    }

    pub fn efficiency_dialog(&mut self, ctx: &egui::Context) {
        let writable = self.writable();
        let demo = self.opts.demo;
        let Some(dialog) = self.efficiency.as_mut() else {
            return;
        };
        if let Some(result) = dialog.loading.as_ref().and_then(Pending::poll) {
            dialog.loading = None;
            match result {
                Ok(settings) => {
                    dialog.draft = settings;
                    dialog.ready = true;
                }
                Err(error) => dialog.error = Some(format!("无法读取已保存设置。{error}")),
            }
        }
        let pal = self.pal.clone();
        let editable = (writable || demo) && dialog.ready && !dialog.saving;
        let mut close = false;
        let mut save = false;
        let modal = egui::Modal::new(egui::Id::new("sa-efficiency"))
            .frame(widgets::modal_frame(&pal))
            .show(ctx, |ui| {
                ui.set_width(540.0);
                close = widgets::modal_header(
                    ui,
                    "",
                    "Token 与缓存",
                    "保存后用于所有工单的后续轮次，当前执行继续完成。",
                    &pal,
                );
                if demo {
                    description(ui, "演示预览，可试选控件。设置仅用于本次预览。", &pal);
                    ui.add_space(10.0);
                } else if !writable {
                    description(ui, "只读观察窗口。请在正在调度的主窗口保存设置。", &pal);
                    ui.add_space(10.0);
                }
                if dialog.loading.is_some() {
                    ui.horizontal(|ui| {
                        widgets::spinner(ui);
                        ui.label("正在读取已保存设置…");
                    });
                }
                let max_height = (ctx.content_rect().height() - 240.0).max(180.0);
                egui::ScrollArea::vertical().max_height(max_height).show(ui, |ui| {
                    ui.add_enabled_ui(editable, |ui| {
                        ui.add(widgets::Check::new(&mut dialog.draft.incremental_context, "增量上下文"));
                        description(ui, "续轮仅补充变化的上下文，完整内容保留为恢复依据。适用于完全访问与自动模式。", &pal);
                        ui.add_space(16.0);
                        ui.add(widgets::Check::new(&mut dialog.draft.lazy_mcp_schemas, "按需读取 MCP 参数"));
                        description(ui, "大工具目录先发送名称和简述，调用前从完整快照读取该工具的参数。适用于完全访问与自动模式；关闭后恢复完整目录。", &pal);
                        ui.add_space(16.0);
                        ui.add(widgets::Check::new(&mut dialog.draft.concise_handoffs, "精简协作提示"));
                        description(ui, "要求简短交接、只传相关证据，将长日志留在文件中。", &pal);
                        ui.add_space(16.0);
                        widgets::rule(ui, pal.line_soft);
                        ui.add_space(16.0);
                        widgets::section(ui, "Claude 缓存时长", &pal);
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut dialog.draft.claude_cache_ttl, Ttl::Inherit, "沿用执行端");
                            ui.radio_value(&mut dialog.draft.claude_cache_ttl, Ttl::FiveMinutes, "5 分钟");
                            ui.radio_value(&mut dialog.draft.claude_cache_ttl, Ttl::OneHour, "1 小时");
                        });
                        description(ui, "作为 Claude 主会话与内部子请求的默认时长。CLI 已有的显式时长、强制时长或禁用缓存设置优先生效。", &pal);
                        ui.add_space(8.0);
                        description(ui, "1 小时缓存的写入成本较高，实际生效与命中取决于执行端、网关和服务端支持。", &pal);
                        ui.add_space(12.0);
                        ui.add(widgets::Check::new(&mut dialog.draft.claude_keepalive, "长等待缓存保温"));
                        description(ui, "Claude 主控等待协作者超过缓存时长时，在过期前用不保存的分叉会话发一条极短请求，按缓存读取计费以刷新缓存，避免整段上下文过期重写。每次等待最多 10 次，有真实请求后重新计数。", &pal);
                        ui.add_space(8.0);
                        description(ui, "Codex、DeepSeek 等执行端沿用各自的服务端缓存策略。", &pal);
                        ui.add_space(16.0);
                        widgets::rule(ui, pal.line_soft);
                        ui.add_space(16.0);
                        widgets::section(ui, "用量调度", &pal);
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            let label = ui.label("协作者并发上限");
                            ui.add(
                                egui::DragValue::new(&mut dialog.draft.max_collaborators)
                                    .range(0..=16)
                                    .clamp_existing_to_range(false)
                                    .speed(1.0)
                                    .custom_formatter(|value, _| {
                                        if value == 0.0 { "不限制".into() } else { format!("{value:.0}") }
                                    })
                                    .custom_parser(|text| {
                                        if text.trim() == "不限制" { Some(0.0) } else { text.trim().parse().ok() }
                                    }),
                            ).labelled_by(label.id);
                        });
                        description(ui, "同一工单同时运行的协作者数量，主控不计入；其余派工排队等待。", &pal);
                        ui.add_space(12.0);
                        ui.add(widgets::Check::new(&mut dialog.draft.quota_guard, "额度保护"));
                        description(ui, "订阅额度窗口用尽时等待重置后继续，而不是判任务失败；接近用尽时为主控保留余量。", &pal);
                    });
                });
                if let Some(error) = &dialog.error {
                    ui.add_space(12.0);
                    ui.add(egui::Label::new(RichText::new(error).color(pal.fail.fg)).wrap());
                }
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(editable, |ui| {
                        if widgets::quiet_button(ui, "", "恢复默认", &pal, false).clicked() {
                            dialog.draft = Settings::default();
                            dialog.error = None;
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        save = widgets::primary_button(
                            ui,
                            "",
                            if dialog.saving { "正在保存…" } else { "保存" },
                            &pal,
                            writable && editable,
                        ).clicked();
                        ui.add_enabled_ui(!dialog.saving, |ui| {
                            if widgets::quiet_button(ui, "", "取消", &pal, false).clicked() {
                                close = true;
                            }
                        });
                    });
                });
            });
        if !dialog.saving && (close || modal.should_close()) {
            self.efficiency = None;
            return;
        }
        if save {
            dialog.saving = true;
            dialog.error = None;
            let settings = dialog.draft.clone();
            self.submit(Action::SaveEfficiency { settings });
        }
    }
}
