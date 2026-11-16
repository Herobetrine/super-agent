//! Operator-owned worktree policy and local integration actions.
use crate::{
    host::{Action, Host},
    theme::{self, Palette, Tone},
    widgets,
};
use egui::{Label, RichText, Ui};

#[derive(Default)]
pub struct WorkflowState {
    command: String,
    confirm: Option<i64>,
    error: Option<String>,
    ticket: Option<i64>,
    fork_goal: String,
    fork_seq: Option<i64>,
    fork_confirm: bool,
    pr_url: String,
    pr_job: String,
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

impl WorkflowState {
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
                let rows = match host.workspaces(ticket) {
                    Ok(r) => r,
                    Err(e) => {
                        problem(ui, &e.to_string(), pal);
                        return;
                    }
                };
                if rows.is_empty() {
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
}
