//! Operator-owned worktree policy and local integration actions.
use crate::{
    host::{Action, Host},
    theme::Palette,
};

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
        egui::CollapsingHeader::new("隔离工作区与合入")
            .id_salt(("workspace", ticket))
            .show(ui, |ui| {
                let mut enabled = host.isolated_writers(ticket);
                if ui
                    .checkbox(&mut enabled, "将后续写入子任务放入独立 Git 工作区")
                    .changed()
                {
                    action = Some(Action::SetIsolatedWriters { ticket, enabled });
                }
                ui.small("从已提交的干净仓库创建。完成后先验证，再本地合入。已有任务不迁移。");
                let rows = match host.workspaces(ticket) {
                    Ok(r) => r,
                    Err(e) => {
                        ui.colored_label(pal.warn.fg, e.to_string());
                        return;
                    }
                };
                if rows.is_empty() {
                    return;
                }
                ui.label("验证命令（JSON 参数数组）");
                ui.add(
                    egui::TextEdit::singleline(&mut self.command)
                        .hint_text(r#"["git","diff","--check"]"#)
                        .desired_width(f32::INFINITY),
                );
                ui.small(
                    "使用项目实际测试命令；验证会在任务分支保存提交，合入前再次测试合并候选。",
                );
                if let Some(error) = &self.error {
                    ui.colored_label(pal.warn.fg, error);
                }
                for w in rows {
                    ui.separator();
                    let state = match w.state.as_str() {
                        "planned" => "等待创建",
                        "ready" => "待验证",
                        "validated" => "验证通过，待合入",
                        "merged" => "已合入",
                        "conflict" => "合并冲突",
                        "failed" => "验证未通过",
                        _ => "状态待确认",
                    };
                    ui.label(format!("任务 #{} · {state}", w.job));
                    ui.label(egui::RichText::new(&w.path).small().color(pal.mute));
                    if !w.detail.is_empty() {
                        ui.small(&w.detail);
                    }
                    if let Some(candidate) = w.validation["candidate"].as_str() {
                        ui.small(format!("合并候选：{candidate}"));
                    }
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add_enabled(
                                !matches!(w.state.as_str(), "planned" | "merged"),
                                egui::Button::new(format!("验证 #{}", w.job)),
                            )
                            .clicked()
                        {
                            match serde_json::from_str::<Vec<String>>(&self.command) {
                                Ok(command) if !command.is_empty() => {
                                    self.error = None;
                                    action = Some(Action::ValidateWorkspace {
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
                            .add_enabled(
                                w.state == "validated",
                                egui::Button::new(format!("合入 #{}…", w.job)),
                            )
                            .clicked()
                        {
                            self.confirm = Some(w.job);
                        }
                    });
                    if self.confirm == Some(w.job) {
                        ui.label("将验证合并后的代码，并更新原项目的本地分支。不会推送到远端。");
                        ui.horizontal(|ui| {
                            if ui.button("确认验证并合入").clicked() {
                                action = Some(Action::IntegrateWorkspace { job: w.job });
                                self.confirm = None;
                            }
                            if ui.button("暂不合入").clicked() {
                                self.confirm = None;
                            }
                        });
                    }
                }
            });
        egui::CollapsingHeader::new("从历史检查点分支").id_salt(("fork",ticket)).show(ui,|ui| {
            let rows=host.checkpoints(ticket).unwrap_or_default();
            if rows.is_empty(){ui.small("主控返回终稿后，可选择历史轮次建立分支。");return;}
            egui::ComboBox::from_id_salt(("fork-seq",ticket)).selected_text(self.fork_seq.map(|s|format!("检查点 #{s}")).unwrap_or_else(||"选择历史轮次".into())).show_ui(ui,|ui|{
                for r in rows {if let Some(seq)=r["seq"].as_i64(){ui.selectable_value(&mut self.fork_seq,Some(seq),format!("第 {} 轮 · 检查点 #{seq}",r["turn"]));}}
            });
            ui.add(egui::TextEdit::multiline(&mut self.fork_goal).desired_rows(2).hint_text("新分支要尝试的目标"));
            ui.small("保留原会话，新分支沿用原权限和模型并重新请求模型；不会回滚文件或重放旧工具，MCP 与技能需重新选择。");
            if ui.add_enabled(self.fork_seq.is_some()&&!self.fork_goal.trim().is_empty(),egui::Button::new("建立分支…")).clicked(){self.fork_confirm=true;}
            if self.fork_confirm {
                if let Ok(source)=host.branch_source(ticket){
                    ui.label(format!("主控：{} · 权限：{}",crate::labels::agent_name(source["lead"].as_str().unwrap_or_default()),crate::labels::profile(source["permission_profile"].as_str().unwrap_or_default())));
                }
                ui.horizontal(|ui|{
                    if ui.button("确认沿用权限并启动").clicked(){if let Some(seq)=self.fork_seq{action=Some(Action::ForkCheckpoint{ticket,seq,goal:self.fork_goal.clone()});}self.fork_confirm=false;}
                    if ui.button("取消分支").clicked(){self.fork_confirm=false;}
                });
            }
        });
        egui::CollapsingHeader::new("PR 与 CI 反馈").id_salt(("scm",ticket)).show(ui,|ui|{
            ui.small("绑定任务后按需读取 GitHub 检查与评审摘要。新反馈在任务下一次已授权执行时送达；不会自动恢复已停止的任务。");
            ui.horizontal(|ui|{
                ui.label("任务编号");ui.add(egui::TextEdit::singleline(&mut self.pr_job).desired_width(70.0));
            });
            ui.add(egui::TextEdit::singleline(&mut self.pr_url).desired_width(f32::INFINITY).hint_text("https://github.com/owner/repo/pull/123"));
            if ui.button("绑定 PR").clicked(){
                match self.pr_job.trim().parse::<i64>() {
                    Ok(job)=>{action=Some(Action::BindScm{ticket,job,url:self.pr_url.trim().to_owned()});self.error=None;}
                    Err(_)=>self.error=Some("请输入本工单的任务编号".into()),
                }
            }
            if let Some(error)=&self.error{ui.colored_label(pal.warn.fg,error);}
            match host.scm_bindings(ticket){
                Ok(rows)=>for row in rows{
                    let job=row["job"].as_i64().unwrap_or_default();
                    ui.separator();
                    ui.label(format!("任务 #{job} · {} 条待送达",row["pending"]));
                    if let Some(url)=row["url"].as_str(){ui.hyperlink(url);}
                    if let Some(summary)=row["latest"].get("summary") {
                        ui.label(format!("CI：{} · 评审：{}",summary["ci"].as_str().unwrap_or("unknown"),summary["review_decision"].as_str().unwrap_or("unknown")));
                        if let Some(head)=summary["head"].as_str(){ui.small(format!("提交：{head}"));}
                    }
                    if ui.button(format!("读取反馈 #{job}")).clicked(){action=Some(Action::RefreshScm{job});}
                },
                Err(e)=>{ui.colored_label(pal.warn.fg,e.to_string());}
            }
        });
        action
    }
}
