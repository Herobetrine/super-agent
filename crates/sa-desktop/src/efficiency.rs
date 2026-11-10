//! Global context-delivery and Claude cache preferences.

use crate::{app::App, host::Action, ops::Pending, theme, widgets};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use sa_store::efficiency::{self, Settings, Ttl};

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
            Some(Pending::spawn(&self.ctx, move || {
                let conn = rusqlite::Connection::open_with_flags(
                    dir.join(sa_store::DB_FILE),
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )?;
                efficiency::load(&conn)
            }))
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
                    icon::GEAR,
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
                        ui.spinner();
                        ui.label("正在读取已保存设置…");
                    });
                }
                let max_height = (ctx.content_rect().height() - 240.0).max(180.0);
                egui::ScrollArea::vertical().max_height(max_height).show(ui, |ui| {
                    ui.add_enabled_ui(editable, |ui| {
                        ui.checkbox(&mut dialog.draft.incremental_context, "增量上下文");
                        description(ui, "续轮仅补充变化的上下文，完整内容保留为恢复依据。适用于完全访问与自动模式。", &pal);
                        ui.add_space(14.0);
                        ui.checkbox(&mut dialog.draft.concise_handoffs, "精简协作提示");
                        description(ui, "要求简短交接、只传相关证据，将长日志留在文件中。", &pal);
                        ui.add_space(18.0);
                        ui.separator();
                        ui.add_space(12.0);
                        ui.label(RichText::new("Claude 缓存时长").font(theme::strong(14.0)).color(pal.ink));
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut dialog.draft.claude_cache_ttl, Ttl::Inherit, "沿用执行端");
                            ui.radio_value(&mut dialog.draft.claude_cache_ttl, Ttl::FiveMinutes, "5 分钟");
                            ui.radio_value(&mut dialog.draft.claude_cache_ttl, Ttl::OneHour, "1 小时");
                        });
                        description(ui, "作为 Claude 主会话与内部子请求的默认时长。CLI 已有的显式时长、强制时长或禁用缓存设置优先生效。", &pal);
                        ui.add_space(8.0);
                        description(ui, "1 小时缓存的写入成本较高，实际生效与命中取决于执行端、网关和服务端支持。", &pal);
                        ui.add_space(8.0);
                        description(ui, "Codex、DeepSeek 等执行端沿用各自的服务端缓存策略。", &pal);
                    });
                });
                if let Some(error) = &dialog.error {
                    ui.add_space(12.0);
                    ui.add(egui::Label::new(RichText::new(error).color(pal.fail.fg)).wrap());
                }
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(editable, egui::Button::new("恢复默认")).clicked() {
                        dialog.draft = Settings::default();
                        dialog.error = None;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        save = widgets::primary_button(
                            ui,
                            icon::CHECK,
                            if dialog.saving { "正在保存…" } else { "保存" },
                            &pal,
                            writable && editable,
                        ).clicked();
                        if ui.add_enabled(!dialog.saving, egui::Button::new("取消")).clicked() {
                            close = true;
                        }
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
