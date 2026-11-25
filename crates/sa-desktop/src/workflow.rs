//! Operator-owned worktree policy and local integration actions.
use crate::{
    host::{Action, Host},
    theme::{self, Palette, Tone},
    widgets,
};
use egui::{Label, RichText, Ui};
use serde_json::Value;

#[derive(Default)]
pub struct WorkflowState {
    pub delivery: crate::delivery_ui::DeliveryState,
    checks_project: Option<String>,
    checks_rows: Vec<Vec<String>>,
    checks_input: String,
    checks_loading: Option<crate::ops::Pending<Vec<Vec<String>>>>,
    #[cfg(test)]
    next_checks_read: Option<crate::ops::Pending<Vec<Vec<String>>>>,
    checks_error: Option<String>,
    checks_saving: Option<u64>,
    checks_pending_save: Option<(String, Vec<Vec<String>>)>,
    checks_loaded: bool,
    suggestions: Option<sa_engine::check_suggestions::SuggestionReport>,
    suggestions_loading: Option<crate::ops::Pending<sa_engine::check_suggestions::SuggestionReport>>,
    suggestions_error: Option<String>,
    command: String,
    hydration_ticket: Option<i64>,
    hydration_input: String,
    confirm: Option<i64>,
    queue_confirm: Option<(i64, i64, String)>,
    error: Option<String>,
    ticket: Option<i64>,
    fork_goal: String,
    fork_seq: Option<i64>,
    fork_confirm: bool,
    pr_url: String,
    pr_job: String,
    ledger_loading: Option<(i64, crate::ops::Pending<sa_store::ledger::StatusReport>)>,
    ledger: Option<(i64, Result<sa_store::ledger::StatusReport, String>)>,
}

/// Secondary help text.
fn help(ui: &mut Ui, text: impl Into<String>, pal: &Palette) {
    ui.add(
        Label::new(RichText::new(text.into()).font(theme::body(theme::SMALL)).color(pal.mute)).wrap(),
    );
}

/// Section heading of a collapsible group.
fn heading(text: &str, pal: &Palette) -> RichText {
    RichText::new(text).font(theme::strong(theme::UI)).color(pal.ink)
}

/// Quiet field caption.
fn caption(ui: &mut Ui, text: &str, pal: &Palette) {
    ui.label(RichText::new(text).font(theme::body(theme::UI)).color(pal.ink));
}

fn problem(ui: &mut Ui, text: &str, pal: &Palette) {
    ui.add(
        Label::new(RichText::new(text).font(theme::body(theme::SMALL)).color(pal.warn.fg)).wrap(),
    );
}

fn item_name(ui: &mut Ui, text: &str, pal: &Palette) {
    ui.label(RichText::new(text).font(theme::strong(theme::UI)).color(pal.ink));
}

fn workspace_tone(state: &str, pal: &Palette) -> Tone {
    match state {
        "ready" => pal.wait,
        "validated" | "merged" => pal.ok,
        "conflict" => pal.warn,
        "failed" => pal.fail,
        _ => pal.neutral,
    }
}

pub fn queue_state_label(state: &str) -> &'static str {
    match state {
        "queued" => "等待自动验证",
        "validating" => "正在验证候选",
        "repairing" => "已退回原任务修复",
        "awaiting_confirmation" => "验证通过，待你确认",
        "confirmed" => "已确认，等待空闲后合入",
        "publishing" => "合入中或等待恢复确认",
        "merged" => "已本地合入",
        "blocked" => "需要处理",
        "stale" => "版本已变化，需重新验证",
        "cancelled" => "已停止",
        _ => "状态待确认",
    }
}

impl WorkflowState {
    /// Rows the completion-check editor currently shows.
    #[cfg(test)]
    pub(crate) fn checks_rows(&self) -> &[Vec<String>] {
        &self.checks_rows
    }

    #[cfg(test)]
    pub(crate) fn hold_next_checks_read(
        &mut self,
    ) -> std::sync::mpsc::Sender<Result<Vec<Vec<String>>, String>> {
        let (pending, tx) = crate::ops::Pending::controlled();
        self.next_checks_read = Some(pending);
        tx
    }

    fn save_checks(&mut self, cwd: &str) -> Action {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let request = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.checks_saving = Some(request);
        self.checks_pending_save = Some((cwd.to_owned(), self.checks_rows.clone()));
        self.checks_error = None;
        Action::SaveProjectChecks { cwd: cwd.to_owned(), checks: self.checks_rows.clone(), request }
    }

    pub fn checks_result(&mut self, request: u64, result: Result<(), String>) {
        if self.checks_saving != Some(request) {
            return;
        }
        self.checks_saving = None;
        if let Some((project, rows)) = self.checks_pending_save.take() {
            match result {
                Ok(()) if self.checks_project.as_deref() == Some(project.as_str()) => {
                    // A read launched after switching away and back may have
                    // preceded the save. Its late result must not win.
                    self.checks_loading = None;
                    self.checks_rows = rows;
                    self.checks_loaded = true;
                    self.checks_error = None;
                }
                Ok(()) => {}
                Err(error) => self.checks_error = Some(format!("保存 {project} 的完成检查失败：{error}")),
            }
        } else { self.checks_error = result.err(); }
    }

    /// Project checks are available in observer windows through a read-only connection.
    #[allow(clippy::too_many_arguments)]
    pub fn checks(
        &mut self,
        ui: &mut Ui,
        pal: &Palette,
        host: Option<&Host>,
        cwd: &str,
        data: Option<&std::path::Path>,
        demo: bool,
    ) -> Option<Action> {
        if self.checks_project.as_deref() != Some(cwd) {
            self.checks_project = Some(cwd.to_owned());
            self.checks_rows.clear();
            self.checks_input.clear();
            self.checks_error = None;
            self.checks_loaded = demo;
            self.suggestions = None;
            self.suggestions_error = None;
            self.suggestions_loading = if host.is_some() && !demo {
                let project = std::path::PathBuf::from(cwd);
                Some(crate::ops::Pending::spawn(ui.ctx(), move || sa_engine::check_suggestions::inspect(&project)))
            } else { None };
            self.checks_loading = if demo {
                self.checks_rows = vec![vec!["cargo".into(), "test".into(), "--workspace".into()]];
                None
            } else if let Some(host) = host {
                let read = || host.project_checks(ui.ctx(), cwd);
                #[cfg(test)]
                let pending = self.next_checks_read.take().unwrap_or_else(read);
                #[cfg(not(test))]
                let pending = read();
                Some(pending)
            } else {
                let data = data.map(std::path::Path::to_path_buf).unwrap_or_else(sa_store::default_data_dir);
                let project = sa_engine::services::realpath(std::path::Path::new(cwd))
                    .map(|path| path.to_string_lossy().into_owned()).unwrap_or_else(|_| cwd.to_owned());
                Some(crate::ops::Pending::spawn(ui.ctx(), move || {
                    use rusqlite::OptionalExtension;
                    let connection = rusqlite::Connection::open_with_flags(data.join(sa_store::DB_FILE), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                    connection.busy_timeout(std::time::Duration::from_secs(1))?;
                    let exists: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='project_checks')", [], |row| row.get(0))?;
                    if !exists { return Ok(vec![]); }
                    let text: Option<String> = connection.query_row("SELECT checks FROM project_checks WHERE project=?", [&project], |row| row.get(0)).optional()?;
                    Ok(text.map(|text| serde_json::from_str(&text)).transpose()?.unwrap_or_default())
                }))
            };
        }
        if let Some(result) = self.checks_loading.as_ref().and_then(crate::ops::Pending::poll) {
            self.checks_loading = None;
            match result {
                Ok(rows) => { self.checks_rows = rows; self.checks_loaded = true; }
                Err(error) => self.checks_error = Some(error),
            }
        }
        let writable = host.is_some() && !demo;
        if let Some(result) = self.suggestions_loading.as_ref().and_then(crate::ops::Pending::poll) {
            self.suggestions_loading = None;
            match result {
                Ok(report) => self.suggestions = Some(report),
                Err(error) => self.suggestions_error = Some(error),
            }
        }
        let mut action = None;
        egui::CollapsingHeader::new(heading("完成检查", pal))
            .id_salt(("completion-checks", cwd))
            .show(ui, |ui| {
                help(ui, "写入任务或主控报告完成时由平台运行；未通过会退回原任务修复，最多两次。命令由你指定，agent 不能修改。", pal);
                if writable {
                    let mut adopt = false;
                    if let Some(report) = &self.suggestions {
                        egui::CollapsingHeader::new(heading("推荐检查", pal))
                            .id_salt(("suggested-checks", cwd))
                            .default_open(self.checks_rows.is_empty())
                            .show(ui, |ui| {
                                help(ui, "根据项目文件识别；点击采用后才会保存并启用。项目脚本会在完成检查时运行，请先核对命令。", pal);
                                for suggestion in &report.suggestions {
                                    item_name(ui, &suggestion.label, pal);
                                    help(ui, &suggestion.reason, pal);
                                    ui.add(Label::new(RichText::new(serde_json::to_string(&suggestion.command).unwrap_or_default()).font(theme::mono(12.0)).color(pal.ink)).wrap());
                                }
                                for issue in &report.issues { help(ui, issue, pal); }
                                let additions = report.suggestions.iter().filter(|s| !self.checks_rows.contains(&s.command)).count();
                                let room = self.checks_rows.len() + additions <= 4;
                                let ready = self.checks_loaded && self.checks_saving.is_none() && additions > 0 && room;
                                if !room { help(ui, "最多保留 4 条检查；请先移除不需要的命令，再采用推荐。", pal); }
                                adopt = ui.add_enabled_ui(ready, |ui| widgets::outline_button(ui, "", "采用推荐检查", pal)).inner.clicked();
                            });
                    }
                    if adopt {
                        if let Some(report) = &self.suggestions {
                            for suggestion in &report.suggestions {
                                if !self.checks_rows.contains(&suggestion.command) { self.checks_rows.push(suggestion.command.clone()); }
                            }
                        }
                        action = Some(self.save_checks(cwd));
                    }
                    if let Some(error) = &self.suggestions_error { problem(ui, &format!("无法读取推荐检查：{error}"), pal); }
                }
                let mut remove = None;
                for (index, argv) in self.checks_rows.iter().enumerate() {
                    ui.horizontal_top(|ui| {
                        if writable && self.checks_saving.is_none() && widgets::quiet_button(ui, "", &format!("移除检查 {}", index + 1), pal, false).clicked() {
                            remove = Some(index);
                        }
                        ui.add(Label::new(RichText::new(serde_json::to_string(argv).unwrap_or_default()).font(theme::mono(12.0)).color(pal.ink)).wrap().selectable(true));
                    });
                }
                if let Some(index) = remove { self.checks_rows.remove(index); }
                if writable {
                    ui.add_enabled_ui(self.checks_loaded && self.checks_saving.is_none(), |ui| {
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut self.checks_input)
                                .font(theme::mono(12.0))
                                .margin(egui::Margin::symmetric(8, 5))
                                .desired_width(f32::INFINITY)
                                .hint_text(widgets::hint(ui, r#"["git","diff","--check"]"#)),
                        );
                        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, ui.is_enabled(), "完成检查命令"));
                        ui.horizontal_wrapped(|ui| {
                            if ui.add_enabled_ui(self.checks_rows.len() < 4, |ui| widgets::outline_button(ui, "", "添加检查", pal)).inner.clicked() {
                                match serde_json::from_str::<Vec<String>>(&self.checks_input) {
                                    Ok(argv) if !argv.is_empty() => {
                                        self.checks_rows.push(argv);
                                        self.checks_input.clear();
                                        self.checks_error = None;
                                    }
                                    _ => self.checks_error = Some("请输入命令参数数组，例如 [\"git\",\"diff\",\"--check\"]".into()),
                                }
                            }
                            let can_save = ui.is_enabled();
                            if widgets::primary_button(ui, "", "保存完成检查", pal, can_save).clicked() {
                                action = Some(self.save_checks(cwd));
                            }
                        });
                    });
                }
                if let Some(error) = &self.checks_error { problem(ui, error, pal); }
            });
        ui.add_space(8.0);
        action
    }
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        pal: &Palette,
        host: &Host,
        ticket: i64,
    ) -> Option<Action> {
        if self.ticket != Some(ticket) {
            self.ticket = Some(ticket);
            self.confirm = None;
            self.queue_confirm = None;
            self.error = None;
            self.fork_seq = None;
            self.fork_confirm = false;
        }
        let mut action = None;
        ui.scope(|ui| {
            // Whitespace, not a rule, groups what a header opens.
            ui.visuals_mut().indent_has_left_vline = false;
            ui.spacing_mut().item_spacing.y = 8.0;
            self.groups(ui, pal, host, ticket, &mut action);
        });
        action
    }

    /// Platform operations whose effect needs the operator's check (P4).
    fn operations(&mut self, ui: &mut Ui, pal: &Palette, host: &Host, ticket: i64, action: &mut Option<Action>) {
        let Ok(rows) = host.operations(ticket) else { return };
        let open: Vec<_> = rows.iter()
            .filter(|op| matches!(op.state.as_str(), "unknown_effect" | "started" | "cancel_requested"))
            .collect();
        egui::CollapsingHeader::new(heading(&if open.is_empty() { "平台操作".to_string() } else { format!("平台操作 · {} 项待核对", open.len()) }, pal))
            .id_salt(("operations", ticket))
            .show(ui, |ui| {
                help(ui, format!("平台自己执行的 Git、合入、检查和工具调用共 {} 条记录。重启时只核对实际结果，不会重做；结果未知的操作需要你确认现场。", rows.len()), pal);
                for op in &open {
                    let job = op.job.map(|j| format!(" · 任务 #{j}")).unwrap_or_default();
                    item_name(ui, &format!("{}{} · {}", crate::labels::operation_kind(&op.kind), job,
                        crate::labels::operation_state(&op.state)), pal);
                    let detail = op.detail.get("reason").and_then(Value::as_str)
                        .or_else(|| op.detail.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| op.detail.to_string());
                    if !detail.is_empty() && detail != "null" { help(ui, detail, pal); }
                    if op.state == "unknown_effect"
                        && widgets::outline_button(ui, "", &format!("已核对现场 #{}", op.id), pal).clicked()
                    {
                        *action = Some(Action::ResolveOperation { id: op.id, note: "操作者已在桌面端核对现场".into() });
                    }
                }
            });
    }

    /// Spending ledger of this ticket (P0A): who spent what, and the gaps.
    fn ledger(&mut self, ui: &mut Ui, pal: &Palette, host: &Host, ticket: i64) {
        if let Some((id, pending)) = &self.ledger_loading
            && let Some(result) = pending.poll()
        {
            self.ledger = Some((*id, result));
            self.ledger_loading = None;
        }
        egui::CollapsingHeader::new(heading("用量账目", pal))
            .id_salt(("ledger", ticket))
            .show(ui, |ui| {
                help(ui, "按主控、协作者、顾问和执行原因列出已报告的用量；没有报告的部分单列为缺口，不按零计。", pal);
                let loading = self.ledger_loading.as_ref().is_some_and(|(id, _)| *id == ticket);
                if widgets::outline_button(ui, "", if loading { "正在汇总…" } else { "生成用量账目" }, pal).clicked() && !loading {
                    self.ledger_loading = Some((ticket, host.ledger(ui.ctx(), ticket)));
                }
                match self.ledger.as_ref().filter(|(id, _)| *id == ticket).map(|(_, r)| r) {
                    None => {}
                    Some(Err(error)) => problem(ui, error, pal),
                    Some(Ok(report)) => {
                        let c = &report.coverage;
                        help(ui, format!("共 {} 次执行，{} 次报告了用量。", c.attempts, c.reported_attempts), pal);
                        crate::usage::ledger_cost(ui, &report.api_cost, pal);
                        for bucket in &report.buckets {
                            let tokens = bucket.tokens.total_tokens.map(|t| format!("{t} Token")).unwrap_or_else(|| "用量未知".into());
                            help(ui, format!("{} · {} · {}：{} 次（{} 次有用量）· {}{}",
                                crate::labels::ledger_role(&bucket.role), crate::labels::ledger_kind(&bucket.kind),
                                crate::labels::ledger_outcome(&bucket.outcome), bucket.attempts, bucket.reported_attempts,
                                tokens, if bucket.partial { "（不完整）" } else { "" }), pal);
                        }
                        for (reason, count) in &report.gaps {
                            help(ui, format!("缺口 · {}：{count}", crate::labels::ledger_gap(reason)), pal);
                        }
                    }
                }
            });
    }

    /// Rule-based execution policy of the project (P2): record or apply.
    fn policy(&mut self, ui: &mut Ui, pal: &Palette, host: &Host, ticket: i64, action: &mut Option<Action>) {
        egui::CollapsingHeader::new(heading("执行策略", pal))
            .id_salt(("policy", ticket))
            .show(ui, |ui| {
                let Ok((mode, decisions)) = host.execution_policy(ticket) else {
                    help(ui, "工单状态正在更新，稍后重试。", pal);
                    return;
                };
                let mut apply = mode == "apply";
                if ui
                    .add(widgets::Check::new(
                        &mut apply,
                        RichText::new("按规则自动调整（本项目）").font(theme::body(theme::UI)),
                    ))
                    .changed()
                {
                    *action = Some(Action::SetExecutionPolicy { ticket, apply });
                }
                help(ui, "默认只记录建议，不改变调度。开启后仅做三件事：写入范围重叠的隔离写任务排队执行；自动重试与修复达到工单上限或同类失败连续 3 次时停止并交给你；不改变模型、权限或已停止的工单。", pal);
                if decisions.is_empty() {
                    help(ui, "本工单还没有策略记录。", pal);
                    return;
                }
                for decision in &decisions {
                    let applied = if decision["applied"] == true { "已执行" } else { "仅记录" };
                    let job = decision["job"].as_i64().map(|j| format!(" · 任务 #{j}")).unwrap_or_default();
                    help(ui, format!("{} · {}{} · {} · {}",
                        crate::labels::policy_point(decision["point"].as_str().unwrap_or_default()),
                        crate::labels::policy_choice(decision["selected"].as_str().unwrap_or_default()),
                        job, applied, decision["reason"].as_str().unwrap_or_default()), pal);
                }
            });
    }

    /// Ignored generated resources the project needs inside isolated worktrees.
    fn hydration(&mut self, ui: &mut Ui, pal: &Palette, host: &Host, ticket: i64, action: &mut Option<Action>) {
        if self.hydration_ticket != Some(ticket) {
            let Ok(paths) = host.workspace_hydration(ticket) else { return };
            self.hydration_ticket = Some(ticket);
            self.hydration_input = paths.join("
");
        }
        ui.add_space(4.0);
        caption(ui, "补齐的忽略资源（每行一个项目相对路径）", pal);
        ui.add(
            egui::TextEdit::multiline(&mut self.hydration_input)
                .desired_rows(2)
                .margin(egui::Margin::symmetric(8, 5))
                .font(theme::mono(12.0))
                .hint_text(widgets::hint(ui, "assets/generated"))
                .desired_width(f32::INFINITY),
        );
        help(ui, "隔离工作区和合并候选只含 Git 跟踪的文件。这里列出的路径下被 Git 忽略的文件会从原项目复制进去（最多 20 万个文件或 8 GiB）；任务对这些文件的修改不会合入。", pal);
        if widgets::outline_button(ui, "", "保存补齐路径", pal).clicked() {
            let paths = self.hydration_input.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
            *action = Some(Action::SetWorkspaceHydration { ticket, paths });
        }
    }

    fn groups(
        &mut self,
        ui: &mut Ui,
        pal: &Palette,
        host: &Host,
        ticket: i64,
        action: &mut Option<Action>,
    ) {
        egui::CollapsingHeader::new(heading("隔离工作区与合入", pal))
            .id_salt(("workspace", ticket))
            .show(ui, |ui| {
                let Ok(mut enabled) = host.isolated_writers(ticket) else {
                    help(ui, "工单状态正在更新，稍后重试。", pal);
                    return;
                };
                if ui
                    .add(widgets::Check::new(
                        &mut enabled,
                        RichText::new("将后续写入子任务放入独立 Git 工作区").font(theme::body(theme::UI)),
                    ))
                    .changed()
                {
                    *action = Some(Action::SetIsolatedWriters { ticket, enabled });
                }
                help(ui, "从已提交的干净仓库创建。完成后先验证，再本地合入。已有任务不迁移。", pal);
                self.hydration(ui, pal, host, ticket, action);
                let queue_enabled = host.integration_queue_enabled(ticket);
                if let Ok(mut queue_enabled) = queue_enabled {
                    if ui.add(widgets::Check::new(
                        &mut queue_enabled,
                        RichText::new("自动验证合入队列，合入前由我确认").font(theme::body(theme::UI)),
                    )).changed() {
                        self.queue_confirm = None;
                        *action = Some(Action::SetIntegrationQueue { ticket, enabled: queue_enabled });
                    }
                    help(ui, "需先启用隔离工作区并保存完成检查。验证失败或合并冲突会退回原任务修复，最多两次；不会自动更新原项目。", pal);
                }
                let queue = match host.integration_queue(ticket) {
                    Ok(rows) => rows,
                    Err(error) => { problem(ui, &error.to_string(), pal); return; }
                };
                self.queue_rows(ui, pal, &queue, action);
                let rows = match host.workspaces(ticket) {
                    Ok(r) => r,
                    Err(e) => {
                        problem(ui, &e.to_string(), pal);
                        return;
                    }
                };
                if rows.iter().all(|workspace| queue.iter().any(|item| item.job == workspace.job)) {
                    return;
                }
                ui.add_space(4.0);
                caption(ui, "验证命令（JSON 参数数组）", pal);
                ui.add(
                    egui::TextEdit::singleline(&mut self.command)
                        .margin(egui::Margin::symmetric(8, 5))
                        .font(theme::mono(12.0))
                        .hint_text(widgets::hint(ui, r#"["git","diff","--check"]"#))
                        .desired_width(f32::INFINITY),
                );
                help(
                    ui,
                    "使用项目实际测试命令；验证会在任务分支保存提交，合入前再次测试合并候选。",
                    pal,
                );
                if let Some(error) = &self.error {
                    problem(ui, error, pal);
                }
                for w in rows {
                    // Queue-owned workspaces use exact-candidate confirmation,
                    // never the older validate-and-integrate shortcut.
                    if queue.iter().any(|item| item.job == w.job) { continue; }
                    ui.add_space(4.0);
                    widgets::rule(ui, pal.line_soft);
                    ui.add_space(4.0);
                    let state = match w.state.as_str() {
                        "planned" => "等待创建",
                        "ready" => "待验证",
                        "validated" => "验证通过，待合入",
                        "merged" => "已合入",
                        "conflict" => "合并冲突",
                        "failed" => "验证未通过",
                        _ => "状态待确认",
                    };
                    ui.horizontal(|ui| {
                        item_name(ui, &format!("任务 #{}", w.job), pal);
                        widgets::status(ui, state, workspace_tone(&w.state, pal).fg, false);
                    });
                    ui.add(
                        Label::new(
                            RichText::new(&w.path).font(theme::mono(12.0)).color(pal.mute),
                        )
                        .truncate(),
                    );
                    if !w.detail.is_empty() {
                        help(ui, &w.detail, pal);
                    }
                    if let Some(candidate) = w.validation["candidate"].as_str() {
                        help(ui, format!("合并候选：{candidate}"), pal);
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                        let can_validate = !matches!(w.state.as_str(), "planned" | "merged");
                        if ui
                            .add_enabled_ui(can_validate, |ui| {
                                widgets::outline_button(ui, "", &format!("验证 #{}", w.job), pal)
                            })
                            .inner
                            .clicked()
                        {
                            match serde_json::from_str::<Vec<String>>(&self.command) {
                                Ok(command) if !command.is_empty() => {
                                    self.error = None;
                                    *action = Some(Action::ValidateWorkspace {
                                        job: w.job,
                                        command,
                                    });
                                }
                                _ => {
                                    self.error = Some(
                                        "请输入命令参数数组，例如 [\"git\",\"diff\",\"--check\"]"
                                            .into(),
                                    )
                                }
                            }
                        }
                        if ui
                            .add_enabled_ui(w.state == "validated", |ui| {
                                widgets::outline_button(ui, "", &format!("合入 #{}…", w.job), pal)
                            })
                            .inner
                            .clicked()
                        {
                            self.confirm = Some(w.job);
                        }
                    });
                    if self.confirm == Some(w.job) {
                        help(ui, "将验证合并后的代码，并更新原项目的本地分支。不会推送到远端。", pal);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            if widgets::primary_button(ui, "", "确认验证并合入", pal, true).clicked() {
                                *action = Some(Action::IntegrateWorkspace { job: w.job });
                                self.confirm = None;
                            }
                            if widgets::quiet_button(ui, "", "暂不合入", pal, false).clicked() {
                                self.confirm = None;
                            }
                        });
                    }
                }
            });
        ui.add_space(8.0);
        self.policy(ui, pal, host, ticket, action);
        ui.add_space(8.0);
        self.ledger(ui, pal, host, ticket);
        ui.add_space(8.0);
        self.operations(ui, pal, host, ticket, action);
        ui.add_space(8.0);
        egui::CollapsingHeader::new(heading("从历史检查点分支", pal))
            .id_salt(("fork", ticket))
            .show(ui, |ui| {
                let rows = host.checkpoints(ticket).unwrap_or_default();
                if rows.is_empty() {
                    help(ui, "主控返回终稿后，可选择历史轮次建立分支。", pal);
                    return;
                }
                widgets::combo(("fork-seq", ticket))
                    .selected_text(
                        self.fork_seq
                            .map(|s| format!("检查点 #{s}"))
                            .unwrap_or_else(|| "选择历史轮次".into()),
                    )
                    .show_ui(ui, |ui| {
                        for r in rows {
                            if let Some(seq) = r["seq"].as_i64() {
                                ui.selectable_value(
                                    &mut self.fork_seq,
                                    Some(seq),
                                    format!("第 {} 轮 · 检查点 #{seq}", r["turn"]),
                                );
                            }
                        }
                    });
                ui.add(
                    egui::TextEdit::multiline(&mut self.fork_goal)
                        .margin(egui::Margin::symmetric(8, 5))
                        .font(theme::body(theme::UI))
                        .desired_rows(2)
                        .desired_width(f32::INFINITY)
                        .hint_text(widgets::hint(ui, "新分支要尝试的目标")),
                );
                help(
                    ui,
                    "保留原会话，新分支沿用原权限和模型并重新请求模型；不会回滚文件或重放旧工具，MCP 与技能需重新选择。",
                    pal,
                );
                let can_fork = self.fork_seq.is_some() && !self.fork_goal.trim().is_empty();
                if ui
                    .add_enabled_ui(can_fork, |ui| widgets::outline_button(ui, "", "建立分支…", pal))
                    .inner
                    .clicked()
                {
                    self.fork_confirm = true;
                }
                if self.fork_confirm {
                    if let Ok(source) = host.branch_source(ticket) {
                        ui.label(
                            RichText::new(format!(
                                "主控：{} · 权限：{}",
                                crate::labels::agent_name(source["lead"].as_str().unwrap_or_default()),
                                crate::labels::profile(
                                    source["permission_profile"].as_str().unwrap_or_default()
                                )
                            ))
                            .font(theme::body(theme::UI))
                            .color(pal.ink),
                        );
                    }
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        if widgets::primary_button(ui, "", "确认沿用权限并启动", pal, true).clicked() {
                            if let Some(seq) = self.fork_seq {
                                *action = Some(Action::ForkCheckpoint {
                                    ticket,
                                    seq,
                                    goal: self.fork_goal.clone(),
                                });
                            }
                            self.fork_confirm = false;
                        }
                        if widgets::quiet_button(ui, "", "取消分支", pal, false).clicked() {
                            self.fork_confirm = false;
                        }
                    });
                }
            });
        ui.add_space(8.0);
        egui::CollapsingHeader::new(heading("PR 与 CI 反馈", pal))
            .id_salt(("scm", ticket))
            .show(ui, |ui| {
                help(
                    ui,
                    "绑定任务后按需读取 GitHub 检查与评审摘要。新反馈在任务下一次已授权执行时送达；不会自动恢复已停止的任务。",
                    pal,
                );
                ui.horizontal(|ui| {
                    caption(ui, "任务编号", pal);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.pr_job)
                            .margin(egui::Margin::symmetric(8, 5))
                            .desired_width(70.0),
                    );
                });
                ui.add(
                    egui::TextEdit::singleline(&mut self.pr_url)
                        .margin(egui::Margin::symmetric(8, 5))
                        .font(theme::mono(12.0))
                        .desired_width(f32::INFINITY)
                        .hint_text(widgets::hint(ui, "https://github.com/owner/repo/pull/123")),
                );
                if widgets::outline_button(ui, "", "绑定 PR", pal).clicked() {
                    match self.pr_job.trim().parse::<i64>() {
                        Ok(job) => {
                            *action = Some(Action::BindScm {
                                ticket,
                                job,
                                url: self.pr_url.trim().to_owned(),
                            });
                            self.error = None;
                        }
                        Err(_) => self.error = Some("请输入本工单的任务编号".into()),
                    }
                }
                if let Some(error) = &self.error {
                    problem(ui, error, pal);
                }
                match host.scm_bindings(ticket) {
                    Ok(rows) => {
                        for row in rows {
                            let job = row["job"].as_i64().unwrap_or_default();
                            ui.add_space(4.0);
                            widgets::rule(ui, pal.line_soft);
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                item_name(ui, &format!("任务 #{job}"), pal);
                                ui.label(
                                    RichText::new(format!("{} 条待送达", row["pending"]))
                                        .font(theme::body(theme::TINY))
                                        .color(pal.faint),
                                );
                            });
                            if let Some(url) = row["url"].as_str() {
                                ui.hyperlink(url);
                            }
                            if let Some(summary) = row["latest"].get("summary") {
                                ui.label(
                                    RichText::new(format!(
                                        "CI：{} · 评审：{}",
                                        summary["ci"].as_str().unwrap_or("unknown"),
                                        summary["review_decision"].as_str().unwrap_or("unknown")
                                    ))
                                    .font(theme::body(theme::UI))
                                    .color(pal.ink),
                                );
                                if let Some(head) = summary["head"].as_str() {
                                    help(ui, format!("提交：{head}"), pal);
                                }
                            }
                            if widgets::quiet_button(ui, "", &format!("读取反馈 #{job}"), pal, false)
                                .clicked()
                            {
                                *action = Some(Action::RefreshScm { job });
                            }
                        }
                    }
                    Err(e) => problem(ui, &e.to_string(), pal),
                }
            });
    }

    fn queue_rows(&mut self, ui: &mut Ui, pal: &Palette, rows: &[sa_engine::integration_queue::IntegrationItem], action: &mut Option<Action>) {
        if self.queue_confirm.as_ref().is_some_and(|(job, turn, head)| !rows.iter().any(|item|
            item.job == *job && item.turn == *turn && matches!(item.state.as_str(), "awaiting_confirmation" | "publishing")
                && item.validation["confirmation_token"].as_str() == Some(head.as_str()))) {
            self.queue_confirm = None;
        }
        for item in rows {
            ui.add_space(4.0);
            widgets::rule(ui, pal.line_soft);
            let state = queue_state_label(&item.state);
            ui.horizontal_wrapped(|ui| {
                item_name(ui, &format!("队列任务 #{}", item.job), pal);
                let color = if item.state == "awaiting_confirmation" || item.state == "merged" { pal.ok.fg } else { pal.wait.fg };
                widgets::status(ui, state, color, false);
            });
            help(ui, format!("{} · 已自动退回 {} / 2 次", item.detail, item.attempts), pal);
            if let Some(candidate) = item.validation["candidate"].as_str() { help(ui, format!("候选工作区：{candidate}"), pal); }
            if let Some(head) = item.validation["candidate_head"].as_str() { help(ui, format!("已验证提交：{head}"), pal); }
            if item.validation["workspace_checks"]["reused_from"] == "candidate_checks" {
                help(ui, "候选与任务内容相同，项目检查只在干净的候选目录执行了一次。", pal);
            }
            egui::CollapsingHeader::new(heading(&format!("验证详情 #{}", item.job), pal)).id_salt(("queue-details",item.job)).show(ui, |ui| {
                for key in ["workspace_checks", "candidate_checks"] {
                    if let Some(checks) = item.validation[key].as_array() {
                        for check in checks {
                            help(ui, format!("{} · {}", check["argv"], if check["ok"] == true {"通过"} else {"未通过"}), pal);
                            if let Some(output) = check["output_tail"].as_str().filter(|s| !s.is_empty()) { help(ui, output, pal); }
                        }
                    }
                }
                if let Some(conflicts) = item.validation["conflicts"].as_str().filter(|s| !s.is_empty()) { problem(ui, &format!("冲突文件：{conflicts}"), pal); }
            });
            if matches!(item.state.as_str(), "blocked" | "stale" | "cancelled")
                && widgets::outline_button(ui, "", &format!("重新排队验证 #{}", item.job), pal).clicked() {
                *action = Some(Action::RevalidateIntegration { job: item.job });
            }
            if matches!(item.state.as_str(), "awaiting_confirmation" | "publishing")
                && widgets::outline_button(ui, "", &format!("确认候选 #{}…", item.job), pal).clicked() {
                self.queue_confirm = Some((item.job, item.turn, item.validation["confirmation_token"].as_str().unwrap_or_default().to_owned()));
            }
            if self.queue_confirm.as_ref().is_some_and(|(job,_,_)| *job == item.job) {
                help(ui, "将把已验证的候选合入原项目本地分支，不会推送。若代码或检查已改变，平台会拒绝并要求重新验证。", pal);
                ui.horizontal_wrapped(|ui| {
                    if widgets::primary_button(ui, "", &format!("确认合入候选 #{}", item.job), pal, true).clicked() {
                        *action = Some(Action::ConfirmIntegration { job: item.job, confirmation_token: item.validation["confirmation_token"].as_str().unwrap_or_default().to_owned() });
                        self.queue_confirm = None;
                    }
                    if widgets::quiet_button(ui, "", &format!("暂不合入候选 #{}", item.job), pal, false).clicked() { self.queue_confirm = None; }
                });
            }
        }
    }
}
