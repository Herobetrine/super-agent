//! The "手机推送与隐私" dialog: ntfy settings, a test push and a plain statement
//! of what Super Agent sends where. Settings live in the journal database and
//! apply as soon as they are saved.

use crate::{
    app::App,
    host::Action,
    ops::Pending,
    push::{self, Event, SendError},
    theme, widgets,
};
use egui::{Align, Color32, Layout, RichText, TextBuffer, Ui};
use sa_store::notify::{self, ServerProblem, Settings, TopicProblem};
use std::time::{Duration, Instant};

const TEST_TIMEOUT: Duration = Duration::from_secs(10);
const OBSERVER_REFRESH: Duration = Duration::from_secs(2);
const COPIED_FOR: f64 = 1.5;

type Saved = Result<Settings, String>;

pub struct NotifyDialog {
    pub draft: Settings,
    pub saving: bool,
    pub error: Option<String>,
    /// Saved settings read by this dialog (windows without a scheduler, retries).
    loaded: Option<Saved>,
    loading: Option<Pending<Settings>>,
    last_read: Instant,
    load_error: Option<String>,
    server_credentials: bool,
    ready: bool,
    testing: Option<Pending<Result<(), SendError>>>,
    /// Outcome of the last test push; cleared when the server or topic changes.
    pub test: Option<Result<(), SendError>>,
    test_key: (String, String),
}

impl NotifyDialog {
    fn show_saved(&mut self, settings: Settings) {
        self.draft = settings.clone();
        self.loaded = Some(Ok(settings));
        self.redact_draft_server();
        self.load_error = None;
        self.ready = true;
    }

    fn redact_draft_server(&mut self) {
        if let Some(redacted) = notify::redact_server_credentials(&self.draft.server) {
            self.draft.server = redacted;
            self.server_credentials = true;
        }
    }
}

/// Reject credential-bearing edits before egui can render or expose their text.
struct ServerText<'a> {
    text: &'a mut String,
    rejected: &'a mut bool,
}

impl TextBuffer for ServerText<'_> {
    fn type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<ServerText<'static>>()
    }

    fn is_mutable(&self) -> bool {
        true
    }

    fn as_str(&self) -> &str {
        self.text
    }

    fn insert_text(&mut self, text: &str, at: usize) -> usize {
        let mut candidate = self.text.clone();
        let inserted = candidate.insert_text(text, at);
        if notify::redact_server_credentials(&candidate).is_some() {
            // The old buffer can contain the userinfo prefix if the last
            // character typed was the authority's @ separator.
            self.text.clear();
            *self.rejected = true;
            return 0;
        }
        *self.text = candidate;
        if inserted != 0 {
            *self.rejected = false;
        }
        inserted
    }

    fn delete_char_range(&mut self, range: std::ops::Range<usize>) {
        let mut candidate = self.text.clone();
        candidate.delete_char_range(range);
        if notify::redact_server_credentials(&candidate).is_some() {
            self.text.clear();
            *self.rejected = true;
            return;
        }
        if candidate != *self.text {
            *self.text = candidate;
            *self.rejected = false;
        }
    }
}

#[cfg(test)]
#[test]
fn server_editor_clears_userinfo_typed_before_the_separator() {
    let mut text = "https://fixture-user:fixture-password".to_string();
    let mut rejected = false;
    let at = text.len();
    let mut field = ServerText { text: &mut text, rejected: &mut rejected };
    assert_eq!(field.insert_text("@ntfy.sh", at), 0);
    assert!(field.as_str().is_empty());
    assert!(rejected);
}

fn read_settings(ctx: &egui::Context, data: std::path::PathBuf) -> Pending<Settings> {
    Pending::spawn(ctx, move || {
        let conn = rusqlite::Connection::open_with_flags(
            data.join(sa_store::DB_FILE),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        notify::load(&conn)
    })
}

/// Why a push could not be delivered, as shown to the operator.
fn reason(error: &SendError) -> String {
    match error {
        SendError::Connect => "无法连接服务器".into(),
        SendError::Timeout => "连接超时（10 秒）".into(),
        SendError::Tls => "无法建立安全连接（证书校验失败）".into(),
        SendError::Status(code) => format!("服务器返回 {code}"),
        SendError::Other(text) => text.clone(),
    }
}

fn at_text(at: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(at)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

fn note(ui: &mut Ui, text: &str, color: Color32) {
    ui.add(
        egui::Label::new(RichText::new(text).font(theme::body(theme::SMALL)).color(color)).wrap(),
    );
}

fn field_label(ui: &mut Ui, text: &str, pal: &theme::Palette) {
    ui.label(RichText::new(text).font(theme::strong(theme::UI)).color(pal.ink));
}

/// Copy button whose caption turns into "已复制" for a moment.
fn copy_button(ui: &mut Ui, key: &'static str, idle: &str, text: &str, pal: &theme::Palette) -> bool {
    let id = egui::Id::new(key);
    let now = ui.input(|i| i.time);
    let recent = ui
        .data(|d| d.get_temp::<f64>(id))
        .is_some_and(|at| now - at < COPIED_FOR);
    let caption = if recent { "已复制" } else { idle };
    let clicked = widgets::outline_button(ui, "", caption, pal).clicked();
    if clicked {
        ui.ctx().copy_text(text.to_string());
        ui.data_mut(|d| d.insert_temp(id, now));
    }
    if recent {
        ui.ctx().request_repaint_after(Duration::from_millis(300));
    }
    clicked
}

impl App {
    pub fn open_notify(&mut self) {
        if self.notify.is_some() {
            return;
        }
        let mut dialog = NotifyDialog {
            draft: Settings::default(),
            saving: false,
            error: None,
            loaded: None,
            loading: None,
            last_read: Instant::now(),
            load_error: None,
            server_credentials: false,
            ready: true,
            testing: None,
            test: None,
            test_key: Default::default(),
        };
        if let Some(host) = &self.host {
            match host.notify_settings() {
                Ok(settings) => {
                    dialog.show_saved(settings);
                }
                Err(error) => {
                    dialog.ready = false;
                    dialog.load_error = Some(error.clone());
                    dialog.loaded = Some(Err(error));
                }
            }
        } else if self.opts.demo {
            dialog.loaded = Some(Ok(Settings::default()));
        } else {
            dialog.ready = false;
            dialog.loading = Some(read_settings(&self.ctx, self.data_dir()));
        }
        self.notify = Some(dialog);
    }

    fn data_dir(&self) -> std::path::PathBuf {
        self.opts.data.clone().unwrap_or_else(sa_store::default_data_dir)
    }

    pub fn notify_saved(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => {
                self.notify = None;
                self.composer.message =
                    Some(("手机推送与隐私设置已保存，立即生效。".into(), false));
            }
            Err(error) => {
                if let Some(dialog) = self.notify.as_mut() {
                    dialog.saving = false;
                    dialog.error = Some(format!("无法保存设置。{error}"));
                } else {
                    self.composer.message = Some((error, true));
                }
            }
        }
    }

    pub fn notify_dialog(&mut self, ctx: &egui::Context) {
        let writable = self.writable();
        let demo = self.opts.demo;
        let data = self.data_dir();
        let last_push = self.pusher.last();
        let pal = self.pal.clone();
        let Some(dialog) = self.notify.as_mut() else {
            return;
        };
        if let Some(result) = dialog.loading.as_ref().and_then(Pending::poll) {
            dialog.loading = None;
            match result {
                Ok(settings) => {
                    let settings = if let Some(host) = &self.host {
                        host.notify_settings_cache().recover(settings.clone());
                        host.notify_settings().unwrap_or(settings)
                    } else {
                        settings
                    };
                    dialog.server_credentials = false;
                    dialog.show_saved(settings);
                }
                Err(error) => {
                    dialog.loaded = Some(Err(error.clone()));
                    dialog.load_error = Some(error);
                    dialog.ready = false;
                }
            }
        }
        dialog.redact_draft_server();
        if let Some(result) = dialog.testing.as_ref().and_then(Pending::poll) {
            dialog.testing = None;
            dialog.test = Some(result.unwrap_or_else(|error| Err(SendError::Other(error))));
        }
        let key = (dialog.draft.server.clone(), dialog.draft.topic.clone());
        if dialog.test.is_some() && key != dialog.test_key {
            dialog.test = None;
        }
        let saved = self
            .host
            .as_ref()
            .map(|host| host.notify_settings())
            .or_else(|| dialog.loaded.clone());
        let busy = dialog.saving || dialog.testing.is_some();
        let editable = (writable || demo) && dialog.ready && !busy;
        let on = editable && dialog.draft.enabled;
        let check = notify::check(&dialog.draft);
        let problem = check.server.is_some()
            || check.topic.is_some()
            || (dialog.draft.enabled && dialog.server_credentials);
        let can_test = writable && on && !problem;
        let failed_load = dialog.load_error.is_some() && dialog.loading.is_none();
        let mut close = false;
        let mut save = false;
        let mut retry = false;
        let mut test = false;
        let modal = egui::Modal::new(egui::Id::new("sa-notify"))
            .frame(widgets::modal_frame(&pal))
            .show(ctx, |ui| {
                ui.set_width(560.0);
                close = widgets::modal_header(
                    ui,
                    "",
                    "手机推送与隐私",
                    "保存后立即生效。推送由本机直接发往你填写的 ntfy 服务器，不经过 Super Agent 的任何服务器。",
                    &pal,
                );
                if demo {
                    note(ui, "演示预览，可试选控件。设置仅用于本次预览。", pal.mute);
                    ui.add_space(10.0);
                } else if !writable {
                    note(ui, "只读观察窗口。请在正在调度的主窗口保存设置。", pal.mute);
                    ui.add_space(10.0);
                }
                if !dialog.ready && dialog.loading.is_some() {
                    ui.horizontal(|ui| {
                        widgets::spinner(ui);
                        ui.label("正在读取已保存设置…");
                    });
                    ui.add_space(8.0);
                }
                if failed_load {
                    let text = format!(
                        "无法读取已保存设置。{}",
                        dialog.load_error.as_deref().unwrap_or_default()
                    );
                    ui.add(egui::Label::new(RichText::new(text).color(pal.fail.fg)).wrap());
                    ui.add_space(8.0);
                }
                let max_height = (ctx.content_rect().height() - 240.0).max(180.0);
                egui::ScrollArea::vertical().max_height(max_height).show(ui, |ui| {
                    widgets::section(ui, "手机推送", &pal);
                    ui.add_space(8.0);
                    ui.add_enabled(
                        editable,
                        widgets::Check::new(&mut dialog.draft.enabled, "向手机推送提醒"),
                    );
                    note(
                        ui,
                        "通过 ntfy 推送。在手机上安装 ntfy 应用并订阅同一个主题，即可收到提醒。推送只含工单编号和状态，不含对话、项目名称或文件内容。",
                        pal.mute,
                    );
                    ui.add_space(12.0);

                    field_label(ui, "服务器", &pal);
                    ui.add_space(4.0);
                    let mut server = ServerText {
                        text: &mut dialog.draft.server,
                        rejected: &mut dialog.server_credentials,
                    };
                    let r = ui.add_enabled(
                        on,
                        egui::TextEdit::singleline(&mut server)
                            .margin(egui::Margin::symmetric(8, 5))
                            .hint_text(widgets::hint(ui, "https://ntfy.sh"))
                            .desired_width(ui.available_width()),
                    );
                    r.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, on, "服务器")
                    });
                    note(
                        ui,
                        "可以改成自建的 ntfy 服务器。使用 ntfy.sh 时，推送会经过 ntfy.sh 的服务器。",
                        pal.mute,
                    );
                    let server_problem = if dialog.draft.enabled && dialog.server_credentials {
                        Some(ServerProblem::HasCredentials)
                    } else {
                        check.server
                    };
                    if let Some(problem) = server_problem {
                        ui.add_space(2.0);
                        note(
                            ui,
                            match problem {
                                ServerProblem::Empty => "请填写服务器地址",
                                ServerProblem::NeedsHttps => {
                                    "请使用 https 地址；只有本机或局域网地址可以用 http"
                                }
                                ServerProblem::Invalid => "服务器地址格式不对",
                                ServerProblem::HasPath => push::ROOT_SERVER_ONLY,
                                ServerProblem::HasCredentials => notify::SERVER_CREDENTIALS_ERROR,
                            },
                            pal.fail.fg,
                        );
                    }
                    ui.add_space(12.0);

                    field_label(ui, "主题", &pal);
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.add_enabled_ui(
                                !dialog.draft.topic.is_empty()
                                    && !busy
                                    && dialog.ready
                                    && (dialog.draft.enabled || !(writable || demo)),
                                |ui| {
                                    copy_button(ui, "sa-notify-copy-topic", "复制", &dialog.draft.topic, &pal);
                                },
                            );
                            ui.add_enabled_ui(on, |ui| {
                                if widgets::outline_button(ui, "", "生成随机主题", &pal).clicked() {
                                    dialog.draft.topic = notify::random_topic();
                                }
                            });
                            let r = ui.add_enabled(
                                on,
                                egui::TextEdit::singleline(&mut dialog.draft.topic)
                                    .margin(egui::Margin::symmetric(8, 5))
                                    .hint_text(widgets::hint(ui, "例如 sa-…"))
                                    .desired_width(ui.available_width()),
                            );
                            r.widget_info(|| {
                                egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, on, "主题")
                            });
                        });
                    });
                    note(
                        ui,
                        "主题相当于收件地址，知道主题的人都能收到这些推送。请使用难以猜到的随机主题。",
                        pal.mute,
                    );
                    if let Some(problem) = check.topic {
                        ui.add_space(2.0);
                        note(
                            ui,
                            match problem {
                                TopicProblem::Empty => "请填写主题",
                                TopicProblem::Invalid => {
                                    "主题只能包含字母、数字、- 和 _，最多 64 个字符"
                                }
                            },
                            pal.fail.fg,
                        );
                    } else if dialog.draft.enabled && check.short_topic {
                        ui.add_space(2.0);
                        note(
                            ui,
                            "主题较短，容易被猜到。建议点击“生成随机主题”。",
                            pal.warn.fg,
                        );
                    }
                    ui.add_space(16.0);

                    widgets::section(ui, "推送哪些提醒", &pal);
                    ui.add_space(8.0);
                    ui.add_enabled_ui(on, |ui| {
                        ui.add(widgets::Check::new(
                            &mut dialog.draft.needs_you,
                            "需要你处理：有问题待回答，或有操作待审批",
                        ));
                        ui.add_space(4.0);
                        ui.add(widgets::Check::new(&mut dialog.draft.done, "已完成"));
                        ui.add_space(4.0);
                        ui.add(widgets::Check::new(
                            &mut dialog.draft.unfinished,
                            "异常结束：失败、部分完成或已中断（手动停止的工单不推送）",
                        ));
                    });
                    if dialog.draft.enabled && check.nothing_selected {
                        ui.add_space(4.0);
                        note(ui, "未选择任何提醒，将不会收到推送。", pal.warn.fg);
                    }
                    ui.add_space(8.0);
                    note(
                        ui,
                        "仅在窗口不在前台时推送，与系统通知一致。同一个待办或同一次工单结束只推送一次；启动前已经存在的待办不会补发。推送只作提醒，不能在手机上直接操作，请回到电脑上的 Super Agent 处理。",
                        pal.mute,
                    );
                    ui.add_space(12.0);

                    ui.horizontal(|ui| {
                        if dialog.testing.is_some() {
                            widgets::spinner(ui);
                        }
                        ui.add_enabled_ui(can_test, |ui| {
                            let caption = if dialog.testing.is_some() {
                                "正在发送…"
                            } else {
                                "发送测试推送"
                            };
                            test = widgets::outline_button(ui, "", caption, &pal).clicked();
                        });
                        if demo {
                            note(ui, "演示预览不会发送真实推送。", pal.mute);
                        }
                    });
                    if let Some(result) = &dialog.test {
                        ui.add_space(4.0);
                        match result {
                            Ok(()) => {
                                let unsaved = saved.as_ref().is_none_or(|s| s.as_ref().ok() != Some(&dialog.draft));
                                let text = if unsaved {
                                    "测试推送已发送。当前设置还没有保存，确认无误后请点击“保存”。如果手机没有收到，请检查订阅的主题是否完全一致，以及 ntfy 是否允许通知。"
                                } else {
                                    "测试推送已发送。如果手机没有收到，请检查订阅的主题是否完全一致，以及 ntfy 是否允许通知。"
                                };
                                note(ui, text, pal.ok.fg);
                            }
                            Err(error) => {
                                note(ui, &format!("发送失败：{}", reason(error)), pal.fail.fg);
                            }
                        }
                    }
                    if let Some(last) = &last_push {
                        ui.add_space(8.0);
                        let text = match &last.result {
                            Ok(()) => format!("最近一次自动推送：{}，已送达服务器", at_text(last.at)),
                            Err(error) => format!(
                                "最近一次自动推送失败：{}，{}",
                                at_text(last.at),
                                reason(error)
                            ),
                        };
                        note(ui, &text, pal.mute);
                    }

                    ui.add_space(16.0);
                    widgets::rule(ui, pal.line_soft);
                    ui.add_space(16.0);
                    widgets::section(ui, "数据与隐私", &pal);
                    ui.add_space(8.0);
                    note(
                        ui,
                        "Super Agent 不收集遥测或使用数据，也不会把任何数据发给 Super Agent 的开发者。",
                        pal.ink,
                    );
                    ui.add_space(8.0);
                    note(ui, "工单、对话、用量记录和设置都保存在本机：", pal.ink);
                    let path = data.display().to_string();
                    ui.horizontal(|ui| {
                        let width = (ui.available_width() - 100.0).max(120.0);
                        ui.allocate_ui_with_layout(
                            egui::vec2(width, 0.0),
                            Layout::top_down(Align::Min),
                            |ui| {
                                ui.set_width(width);
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&path)
                                            .font(theme::mono(theme::SMALL))
                                            .color(pal.mute),
                                    )
                                    .wrap(),
                                );
                            },
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            copy_button(ui, "sa-notify-copy-path", "复制路径", &path, &pal);
                        });
                    });
                    ui.add_space(8.0);
                    note(
                        ui,
                        "Super Agent 本身只在以下情况联网（不含执行端执行任务时运行的命令和工具）：",
                        pal.ink,
                    );
                    for line in [
                        "执行端（Codex、Claude、Gemini、DeepSeek 等）与各自的服务通信；",
                        "你配置的自定义供应商和 MCP 服务器；",
                        "工作台刷新模型目录时（向对应供应商查询可用模型）；",
                        "你在工作台里刷新已绑定的 PR 时（通过本机已登录的 gh 向 GitHub 查询该 PR）；",
                        "你在工作台里打开网页链接时（由默认浏览器访问）；",
                        "工作台运行你配置的项目检查命令时（是否联网取决于命令本身）；",
                        "你开启的手机推送。",
                    ] {
                        ui.horizontal_top(|ui| {
                            ui.add_space(8.0);
                            note(ui, &format!("· {line}"), pal.ink);
                        });
                    }
                    ui.add_space(8.0);
                    match &saved {
                        Some(Ok(s)) if s.enabled => {
                            let redacted = notify::redact_server_credentials(&s.server);
                            let server = redacted.as_deref().unwrap_or_else(|| s.server.trim());
                            if server.is_empty() && redacted.is_some() {
                                note(ui, "手机推送：无法读取设置", pal.ink);
                            } else {
                                note(ui, &format!("手机推送：已开启，发往 {server}"), pal.ink);
                            }
                        }
                        Some(Ok(_)) => note(ui, "手机推送：未开启", pal.ink),
                        Some(Err(_)) => note(ui, "手机推送：无法读取设置", pal.ink),
                        None => {}
                    }
                    ui.add_space(8.0);
                    note(ui, "开发时的自动测试会阻止加入遥测或数据分析组件。", pal.mute);
                });
                if let Some(error) = &dialog.error {
                    ui.add_space(12.0);
                    ui.add(egui::Label::new(RichText::new(error).color(pal.fail.fg)).wrap());
                }
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    if failed_load {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if widgets::quiet_button(ui, "", "关闭", &pal, false).clicked() {
                                close = true;
                            }
                            retry = widgets::primary_button(ui, "", "重试", &pal, true).clicked();
                        });
                        return;
                    }
                    ui.add_enabled_ui(editable, |ui| {
                        if widgets::quiet_button(ui, "", "恢复默认", &pal, false).clicked() {
                            dialog.draft = Settings::default();
                            dialog.server_credentials = false;
                            dialog.error = None;
                        }
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        save = widgets::primary_button(
                            ui,
                            "",
                            if dialog.saving { "正在保存…" } else { "保存" },
                            &pal,
                            writable && editable && !problem,
                        )
                        .clicked();
                        ui.add_enabled_ui(!busy, |ui| {
                            if widgets::quiet_button(ui, "", "取消", &pal, false).clicked() {
                                close = true;
                            }
                        });
                    });
                });
            });
        if !busy && (close || modal.should_close()) {
            self.notify = None;
            return;
        }
        let observer_refresh = if !writable && !demo && dialog.loading.is_none() {
            let remaining = OBSERVER_REFRESH.saturating_sub(dialog.last_read.elapsed());
            ctx.request_repaint_after(remaining);
            remaining.is_zero()
        } else {
            false
        };
        if retry || observer_refresh {
            dialog.load_error = None;
            dialog.last_read = Instant::now();
            dialog.loading = Some(read_settings(ctx, data));
            return;
        }
        let credential_free = !dialog.draft.enabled || !dialog.server_credentials;
        if test && credential_free {
            let (server, topic) = (dialog.draft.server.clone(), dialog.draft.topic.clone());
            dialog.test = None;
            dialog.test_key = (server.clone(), topic.clone());
            dialog.testing = Some(Pending::spawn(ctx, move || {
                Ok(push::send(&server, &topic, &push::test_push(), TEST_TIMEOUT))
            }));
        }
        if save && credential_free {
            dialog.saving = true;
            dialog.error = None;
            let mut settings = dialog.draft.clone();
            settings.server = settings.server.trim().to_string();
            self.submit(Action::SaveNotify(settings));
        }
    }

    /// Whether the window currently has the keyboard focus (toast gate).
    pub(crate) fn window_focused(&self) -> bool {
        #[cfg(test)]
        if let Some(focused) = self.test_focus {
            return focused;
        }
        self.ctx.input(|input| input.viewport().focused).unwrap_or(true)
    }

    /// Queue a phone push for `event` while the window is in the background.
    /// Never blocks: the network call runs on the pusher's worker thread.
    pub(crate) fn push_event(&mut self, event: Event) {
        let Some(host) = &self.host else {
            return;
        };
        if self.opts.demo || self.shot_active() || self.window_focused() {
            return;
        }
        #[cfg(test)]
        if let Ok(settings) = host.notify_settings()
            && let Some(push) = push::push_for(&event, &settings)
        {
            self.pushed.push(push);
        }
        #[cfg(not(test))]
        self.pusher.notify(&event, host.notify_settings_cache());
    }
}
