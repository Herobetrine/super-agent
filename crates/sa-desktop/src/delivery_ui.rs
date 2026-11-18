//! Operator-owned requirements and immutable delivery evidence. No scheduler
//! is needed to inspect a summary; mutations use the existing host queue.
use crate::{
    host::{Action, Host},
    ops::Pending,
    theme::{self, Palette},
    widgets,
};
use egui::{RichText, Ui};
use rusqlite::OptionalExtension;
use sa_store::delivery::{BehaviorCheck, BehaviorReceipt, DeliverySummary};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, atomic::{AtomicBool, AtomicU64, Ordering}},
};

pub struct DeliveryContext<'a> {
    pub data: PathBuf,
    pub ticket: i64,
    pub project: &'a str,
    pub execution: &'a str,
    pub host: Option<&'a Host>,
    pub demo: bool,
    pub expanded: bool,
}

#[derive(Default)]
pub struct DeliveryState {
    key: Option<(PathBuf, i64)>,
    loading: Option<Pending<DeliveryReading>>,
    reading: Option<DeliveryReading>,
    read_failed: bool,
    error: Option<String>,
    saving: Option<(u64, SaveEffect)>,
    editing: Option<RequirementEditor>,
    disposition: Option<DispositionEditor>,
    adoption: Option<CheckEditor>,
    acceptance_confirmation: Option<AcceptanceSelection>,
    text: String,
    path: String,
    expected: String,
    job: String,
    paths: String,
}

enum SaveEffect {
    Requirement,
    Disposition,
    AdoptCheck,
    RunCheck(Arc<AtomicBool>),
    Refresh,
}

struct CheckEditor {
    requirement: i64,
    contract_revision: i64,
    choices: Option<Result<Vec<Vec<String>>, String>>,
    loading: Option<Pending<Vec<Vec<String>>>>,
    selected: Option<Vec<String>>,
    verifier_paths: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Disposition {
    Required,
    Optional,
    Withdrawn,
}

struct DispositionEditor {
    requirement: i64,
    contract_revision: i64,
    disposition: Disposition,
    reason: String,
}

struct RequirementEditor {
    requirement: i64,
    contract_revision: i64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct AcceptanceSelection {
    assessment_id: i64,
    snapshot_id: i64,
    status: &'static str,
}

fn acceptance_kind(status: &str) -> &'static str {
    match status {
        "ready" => "快照交付",
        "partial" => "部分交付",
        _ => "类型未知",
    }
}

#[derive(Clone)]
struct DeliveryReading {
    summary: DeliverySummary,
    integration: Result<Vec<LinkedIntegration>, String>,
}

#[derive(Clone)]
struct LinkedIntegration {
    job: i64,
    turn: i64,
    state: &'static str,
}

fn read_delivery(data: &std::path::Path, ticket: i64) -> anyhow::Result<DeliveryReading> {
    let summary = sa_store::delivery::read_summary(data, ticket)?;
    let jobs: BTreeSet<_> = summary.requirements.iter().filter_map(|requirement| requirement.job).collect();
    let integration = if jobs.is_empty() {
        Ok(Vec::new())
    } else {
        (|| -> anyhow::Result<_> {
            let connection = rusqlite::Connection::open_with_flags(
                data.join(sa_store::DB_FILE), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            connection.busy_timeout(std::time::Duration::from_secs(1))?;
            let exists = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='integration_queue')",
                [], |row| row.get::<_, bool>(0),
            )?;
            if !exists { return Ok(Vec::new()); }
            let mut statement = connection.prepare("SELECT turn,state FROM integration_queue WHERE job=? AND ticket=?")?;
            let mut linked = Vec::new();
            for job in jobs {
                if let Some(item) = statement.query_row([job, ticket], |row| {
                    let state: String = row.get(1)?;
                    Ok(LinkedIntegration {
                        job,
                        turn: row.get(0)?,
                        state: crate::workflow::queue_state_label(&state),
                    })
                }).optional()? {
                    linked.push(item);
                }
            }
            Ok(linked)
        })().map_err(|error| format!("{error:#}"))
    };
    Ok(DeliveryReading { summary, integration })
}

fn note(ui: &mut Ui, text: impl Into<String>, pal: &Palette) {
    ui.label(
        RichText::new(text.into())
            .font(theme::body(theme::SMALL))
            .color(pal.mute),
    );
}

fn input(ui: &mut Ui, value: &mut String, label: &str, multiline: bool) {
    let label = ui.label(label);
    let edit = if multiline {
        egui::TextEdit::multiline(value).desired_rows(2)
    } else {
        egui::TextEdit::singleline(value)
    };
    ui.add(edit.desired_width(f32::INFINITY))
        .labelled_by(label.id);
}

fn status(value: &str) -> &'static str {
    match value {
        "pass" => "符合标准",
        "fail" => "未通过",
        "stale" => "证据已过期",
        _ => "尚未验证",
    }
}

fn requirement_status(value: &str, behavior: bool) -> &'static str {
    match (value, behavior) {
        ("pass", false) => "文件指纹符合标准",
        ("fail", false) => "文件指纹不符",
        _ => status(value),
    }
}

fn gate_status(value: &str) -> &'static str {
    match value {
        "disabled" => "快照交付核验已关闭",
        "ready" => "已满足快照必需标准",
        "partial" => "快照必需标准部分满足",
        "stale" => "快照核验已过期，请重新核验",
        _ => "快照交付核验受阻",
    }
}

fn command(ui: &mut Ui, argv: &[String]) {
    let json = serde_json::to_string(argv).expect("string arrays are JSON serializable");
    ui.add(egui::Label::new(RichText::new(json).font(theme::mono(12.0))).wrap().selectable(true));
}

fn paths(value: &str) -> Vec<String> {
    value.lines().map(str::trim).filter(|path| !path.is_empty()).map(str::to_owned).collect()
}

fn show_receipt(ui: &mut Ui, receipt: &BehaviorReceipt, pal: &Palette) {
    let result = match receipt.status.as_str() {
        "running" => "尚无最终结果",
        "pass" => "通过",
        "fail" => "未通过",
        "invalid" => "无效",
        _ => "结果未知",
    };
    let title = format!("检查回执 #{} · 快照 #{} · {result}", receipt.id, receipt.snapshot_id);
    egui::CollapsingHeader::new(title).id_salt(("behavior-receipt", receipt.id)).show(ui, |ui| {
        note(ui, format!("运行时合同版本 {} · 需求修订版本 {}", receipt.contract_revision, receipt.requirement_revision), pal);
        if let Some(code) = receipt.exit_code { note(ui, format!("退出码 {code}"), pal); }
        note(ui, format!("输入清单 SHA-256 · {}", receipt.manifest_sha256), pal);
        note(ui, format!("运行环境 · {}", receipt.environment), pal);
        ui.add(egui::Label::new(&receipt.detail).wrap().selectable(true));
    });
}

fn show_behavior(ui: &mut Ui, check: &BehaviorCheck, pal: &Palette) {
    note(ui, format!("操作员采用的行为检查 #{} · 采用时合同版本 {} · 需求修订版本 {}", check.id, check.contract_revision, check.requirement_revision), pal);
    command(ui, &check.argv);
    if !check.verifier_files.is_empty() {
        egui::CollapsingHeader::new("已绑定的验证器文件").id_salt(("verifier-files", check.id)).show(ui, |ui| {
            for file in &check.verifier_files {
                note(ui, format!("{} · SHA-256 {}", file.path, file.sha256), pal);
            }
        });
    }
    if let Some(receipt) = &check.receipt {
        show_receipt(ui, receipt, pal);
    } else {
        note(ui, "尚无此检查的快照运行回执。", pal);
    }
}

impl CheckEditor {
    fn new(host: &Host, ui: &Ui, project: &str, requirement: i64, contract_revision: i64, previous: Option<&BehaviorCheck>) -> Self {
        Self {
            requirement,
            contract_revision,
            choices: None,
            loading: Some(host.project_checks(ui.ctx(), project)),
            selected: None,
            verifier_paths: previous.map_or_else(String::new, |check| {
                check.verifier_files.iter().map(|file| file.path.as_str()).collect::<Vec<_>>().join("\n")
            }),
        }
    }

    fn refresh(&mut self, host: &Host, ui: &Ui, project: &str) {
        self.choices = None;
        self.loading = Some(host.project_checks(ui.ctx(), project));
    }

    fn show(&mut self, ui: &mut Ui, pal: &Palette, revision: i64, editable: bool) -> (bool, bool) {
        if let Some(result) = self.loading.as_ref().and_then(Pending::poll) {
            self.loading = None;
            self.choices = Some(result);
        }
        ui.group(|ui| {
            ui.label(format!("为需求 #{} 采用项目完成检查", self.requirement));
            if self.loading.is_some() { ui.spinner(); }
            match &self.choices {
                Some(Ok(choices)) if choices.is_empty() => note(ui, "先在协作页的「完成检查」中保存一条项目命令，再刷新交付证据。", pal),
                Some(Ok(choices)) => {
                    for (index, argv) in choices.iter().enumerate() {
                        if ui.radio(self.selected.as_ref() == Some(argv), format!("选用完成检查 {}", index + 1)).clicked() {
                            self.selected = Some(argv.clone());
                        }
                        command(ui, argv);
                    }
                }
                Some(Err(error)) => { ui.colored_label(pal.fail.fg, error); }
                None => {}
            }
            note(ui, "命令采用后随需求版本保存。请列出命令使用的脚本和测试定义文件，平台将绑定其内容指纹。命令自带完整断言时可留空。", pal);
            note(ui, "命令以工作台的宿主权限运行。项目输入来自捕获清单，请单独确认外部工具和依赖。", pal);
            input(ui, &mut self.verifier_paths, "验证器文件（每行一个项目相对路径）", true);
            let conflict = self.contract_revision != revision;
            if conflict {
                ui.colored_label(pal.warn.fg, format!("检查采用草稿基于需求版本 {}，当前为版本 {revision}。请核对当前需求并重新打开采用检查。", self.contract_revision));
            }
            let selected = self.selected.as_ref().is_some_and(|argv| {
                matches!(&self.choices, Some(Ok(choices)) if choices.contains(argv))
            });
            ui.horizontal_wrapped(|ui| {
                let submit = ui.add_enabled(editable && !conflict && selected, egui::Button::new(format!("保存采用的检查 #{}", self.requirement))).clicked();
                let cancel = ui.button("取消采用检查").clicked();
                (submit, cancel)
            }).inner
        }).inner
    }
}

impl DeliveryState {
    fn refresh_summary(&mut self) {
        self.loading = None;
        self.reading = None;
        self.read_failed = false;
    }

    pub fn result(&mut self, ticket: i64, request: u64, result: Result<(), String>) {
        if self.key.as_ref().is_none_or(|key| key.1 != ticket) {
            return;
        }
        let Some((expected, _)) = &self.saving else {
            // The operator returned to this ticket while its earlier action
            // was still running. Reread committed evidence without reviving
            // that view's editor or applying its error to the new view.
            self.refresh_summary();
            return;
        };
        if request != *expected {
            return;
        }
        let (_, effect) = self.saving.take().expect("matching action was pending");
        // Discard any read that began before this mutation completed.
        self.refresh_summary();
        match result {
            Ok(()) => {
                self.error = None;
                match effect {
                    SaveEffect::Requirement => {
                        self.editing = None;
                        self.text.clear();
                        self.path.clear();
                        self.expected.clear();
                        self.job.clear();
                    }
                    SaveEffect::Disposition => self.disposition = None,
                    SaveEffect::AdoptCheck => self.adoption = None,
                    SaveEffect::RunCheck(_) | SaveEffect::Refresh => {}
                }
            }
            Err(error) => self.error = Some(error),
        }
    }

    fn request(&mut self, effect: SaveEffect) -> u64 {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let request = NEXT.fetch_add(1, Ordering::Relaxed);
        self.saving = Some((request, effect));
        self.loading = None;
        self.error = None;
        request
    }

    fn acceptance(
        &mut self,
        ui: &mut Ui,
        pal: &Palette,
        summary: &DeliverySummary,
        writable: bool,
    ) -> Option<Action> {
        let gate = &summary.gate;
        let candidate = gate.acceptable_assessment().and_then(|assessment| {
            let status = match assessment.status.as_str() {
                "ready" => "ready",
                "partial" => "partial",
                _ => return None,
            };
            Some((AcceptanceSelection { assessment_id: assessment.id, snapshot_id: assessment.snapshot_id, status }, assessment))
        });

        if let Some(record) = &gate.acceptance {
            note(ui, format!("已接受{} · 快照 #{} · 核验回执 #{} · 接受记录 #{}",
                acceptance_kind(&record.status), record.snapshot_id, record.assessment_id, record.id), pal);
        } else if let Some((selection, _)) = candidate {
            note(ui, format!("待人工接受 · {} · 快照 #{} · 核验回执 #{}",
                acceptance_kind(selection.status), selection.snapshot_id, selection.assessment_id), pal);
        } else if summary.latest_snapshot.is_some() || gate.latest_assessment.is_some() {
            note(ui, "当前核验尚无人工接受记录。", pal);
        }
        if let Some(record) = &gate.latest_acceptance
            && gate.acceptance.as_ref().is_none_or(|current| current.id != record.id)
        {
            note(ui, format!("历史接受记录 #{} · {} · 快照 #{} · 核验回执 #{}",
                record.id, acceptance_kind(&record.status), record.snapshot_id, record.assessment_id), pal);
        }

        let candidate = candidate.filter(|_| writable && gate.acceptance.is_none());
        if self.acceptance_confirmation.is_some_and(|selected| candidate.is_none_or(|(active, _)| active != selected)) {
            self.acceptance_confirmation = None;
            note(ui, "核验回执已更新，请核对当前回执后重新确认。", pal);
        }
        let (selection, assessment) = candidate?;
        let partial = selection.status == "partial";
        if self.acceptance_confirmation.is_none() {
            let label = if partial { "接受此部分交付" } else { "接受此快照交付" };
            if ui.add_enabled(self.saving.is_none(), egui::Button::new(label)).clicked() {
                self.acceptance_confirmation = Some(selection);
            }
        }
        if self.acceptance_confirmation != Some(selection) {
            return None;
        }
        let (confirm, dismiss) = ui.group(|ui| {
            ui.label(format!("确认接受快照 #{} · 核验回执 #{} · {}",
                selection.snapshot_id, selection.assessment_id, acceptance_kind(selection.status)));
            if partial {
                for unmet in &assessment.unmet {
                    note(ui, format!("此次仍未满足的必需项 #{} · {}", unmet.requirement_id, unmet.reason), pal);
                }
            }
            ui.horizontal_wrapped(|ui| {
                let label = if partial { "确认接受部分交付" } else { "确认接受快照交付" };
                let confirm = ui.add_enabled(self.saving.is_none(), egui::Button::new(label)).clicked();
                let dismiss = ui.button("取消接受确认").clicked();
                (confirm, dismiss)
            }).inner
        }).inner;
        if confirm || dismiss {
            self.acceptance_confirmation = None;
        }
        confirm.then(|| Action::AcceptDelivery {
            ticket: summary.ticket,
            expected_assessment_id: selection.assessment_id,
            expected_status: selection.status,
            request: self.request(SaveEffect::Refresh),
        })
    }

    pub fn show(&mut self, ui: &mut Ui, pal: &Palette, context: DeliveryContext<'_>) -> Option<Action> {
        let DeliveryContext {
            data,
            ticket,
            project,
            execution,
            host,
            demo,
            expanded,
        } = context;
        let writable = host.is_some() && !demo;
        if self.key.as_ref() != Some(&(data.clone(), ticket)) {
            *self = Self {
                key: Some((data.clone(), ticket)),
                ..Self::default()
            };
        }
        let mut action = None;
        egui::CollapsingHeader::new(RichText::new("需求与交付证据").font(theme::strong(theme::UI)))
            .id_salt(("delivery", ticket)).default_open(expanded).show(ui, |ui| {
                if demo { note(ui, "演示模式不读取或保存真实交付记录。", pal); return; }
                if let Some(result) = self.loading.as_ref().and_then(Pending::poll) {
                    self.loading = None;
                    match result {
                        Ok(reading) => self.reading = Some(reading),
                        Err(error) => { self.error = Some(error); self.read_failed = true; }
                    }
                }
                if self.reading.is_none() && self.loading.is_none() && self.saving.is_none() && !self.read_failed {
                    self.loading = Some(Pending::spawn(ui.ctx(), move || read_delivery(&data, ticket)));
                }
                ui.horizontal_wrapped(|ui| {
                    if ui.add_enabled(self.saving.is_none(), egui::Button::new("刷新交付证据")).clicked() {
                        self.refresh_summary();
                        self.error = None;
                        if let (Some(editor), Some(host)) = (&mut self.adoption, host) {
                            editor.refresh(host, ui, project);
                        }
                    }
                    if self.loading.is_some() { ui.spinner(); }
                    if let Some((_, effect)) = &self.saving {
                        match effect {
                            SaveEffect::RunCheck(cancel) => {
                                let stopping = cancel.load(Ordering::Relaxed);
                                note(ui, if stopping { "正在取消快照检查…" } else { "正在后台运行快照检查…" }, pal);
                                if ui.add_enabled(!stopping, egui::Button::new("取消本次检查")).clicked() {
                                    cancel.store(true, Ordering::Relaxed);
                                }
                            }
                            _ => note(ui, "正在保存…", pal),
                        }
                    }
                });
                if let Some(error) = &self.error { ui.colored_label(pal.fail.fg, error); }
                if matches!(self.saving, Some((_, SaveEffect::RunCheck(_)))) { return; }
                let Some(DeliveryReading { summary, integration }) = self.reading.clone() else { return; };
                if let Some(warning) = &summary.warning { ui.colored_label(pal.warn.fg, warning); }
                if writable {
                    let mut enabled = summary.gate.enabled;
                    if ui.add_enabled(self.saving.is_none(), egui::Checkbox::new(&mut enabled, "启用快照交付核验（由我控制）")).changed() {
                        let request = self.request(SaveEffect::Refresh);
                        action = Some(Action::SetDeliveryGate {
                            ticket, expected_policy_revision: summary.gate.policy_revision, enabled, request,
                        });
                    }
                }
                note(ui, format!("任务执行 · {}", crate::labels::job_state(execution)), pal);
                note(ui, gate_status(&summary.gate.status), pal);
                note(ui, &summary.gate.reason, pal);
                for unmet in &summary.gate.unmet {
                    note(ui, format!("待满足必需项 #{} · {} · {}", unmet.requirement_id, status(&unmet.status), unmet.reason), pal);
                }
                if writable { note(ui, "代码合入 · 见「隔离工作区与合入」。", pal); }
                if let Err(error) = &integration { note(ui, format!("合入队列暂不可读取 · {error}"), pal); }
                note(ui, format!("当前需求证据 · {}", status(&summary.status)), pal);
                if let Some(assessment) = &summary.gate.latest_assessment {
                    note(ui, format!("最近快照核验 #{} · 快照 #{} · 需求版本 {} · 策略版本 {}", assessment.id, assessment.snapshot_id, assessment.contract_revision, assessment.policy_revision), pal);
                }
                let current_snapshot = summary.latest_snapshot.as_ref().filter(|snapshot| snapshot.contract_revision == summary.contract_revision);
                if summary.gate.enabled && current_snapshot.is_none() {
                    note(ui, "待办 · 捕获当前需求版本的快照。", pal);
                }
                if writable && ui.add_enabled(self.saving.is_none() && summary.gate.enabled && current_snapshot.is_some(), egui::Button::new("核验快照交付")).clicked()
                    && let Some(snapshot) = current_snapshot {
                    let request = self.request(SaveEffect::Refresh);
                    action = Some(Action::PrepareDelivery {
                        ticket, expected_contract_revision: summary.contract_revision,
                        expected_snapshot_id: snapshot.id, expected_policy_revision: summary.gate.policy_revision, request,
                    });
                }
                if let Some(accept) = self.acceptance(ui, pal, &summary, writable) {
                    action = Some(accept);
                }
                note(ui, format!("需求版本 {} · {} 项需求", summary.contract_revision, summary.requirements.len()), pal);
                if summary.requirements.is_empty() { note(ui, "尚未记录需求。", pal); }
                for requirement in &summary.requirements {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(format!("#{} · {} · {}", requirement.id, requirement_status(&requirement.status, requirement.behavior.is_some()), requirement.text));
                        if writable && ui.add_enabled(self.saving.is_none(), egui::Button::new(format!("修订需求 #{}", requirement.id))).clicked() {
                            self.editing = Some(RequirementEditor { requirement: requirement.id, contract_revision: summary.contract_revision });
                            self.text = requirement.text.clone();
                            self.path = requirement.path.clone().unwrap_or_default();
                            self.expected = requirement.expected_sha256.clone().unwrap_or_default();
                            self.job = requirement.job.map(|id| id.to_string()).unwrap_or_default();
                        }
                        if writable && ui.add_enabled(self.saving.is_none(), egui::Button::new(format!("调整必需状态 #{}", requirement.id))).clicked() {
                            self.disposition = Some(DispositionEditor {
                                requirement: requirement.id,
                                contract_revision: summary.contract_revision,
                                disposition: if requirement.withdrawn { Disposition::Withdrawn } else if requirement.required { Disposition::Required } else { Disposition::Optional },
                                reason: String::new(),
                            });
                        }
                    });
                    let disposition = if requirement.withdrawn { "已撤销" } else if requirement.required { "必需" } else { "可选" };
                    note(ui, format!("需求 #{} · {disposition}", requirement.id), pal);
                    if let Some(reason) = &requirement.disposition_reason { note(ui, format!("操作员调整理由 · {reason}"), pal); }
                    if let Some(job) = requirement.job {
                        let text = match &integration {
                            Ok(rows) => rows.iter().find(|item| item.job == job).map_or_else(
                                || format!("关联任务 #{job} · 暂无合入队列记录"),
                                |item| format!("关联任务 #{job} · 第 {} 轮合入队列 · {}", item.turn, item.state),
                            ),
                            Err(_) => format!("关联任务 #{job}"),
                        };
                        note(ui, text, pal);
                    }
                    if let Some(path) = &requirement.path { note(ui, format!("验收文件：{path}"), pal); }
                    if let Some(evidence) = &requirement.evidence { note(ui, &evidence.reason, pal); }
                    let frozen_check = summary.latest_snapshot.as_ref().and_then(|snapshot| {
                        snapshot.requirements.iter().find(|item| item.id == requirement.id)?.behavior.as_ref()
                    });
                    if let Some(check) = &requirement.behavior {
                        show_behavior(ui, check, pal);
                        let current_check = check.requirement_revision == requirement.revision;
                        let frozen_check_matches = frozen_check.is_some_and(|frozen| frozen.same_definition(check));
                        if !current_check {
                            note(ui, "需求说明已修订，请重新采用检查。", pal);
                        } else if current_snapshot.is_none() || !frozen_check_matches {
                            note(ui, "请为当前需求版本捕获快照，再运行检查。", pal);
                        } else if writable && !requirement.withdrawn && let Some(snapshot) = current_snapshot {
                            let verb = if check.receipt.is_some() { "重新运行" } else { "运行" };
                            if ui.add_enabled(self.saving.is_none(), egui::Button::new(format!("{verb}需求 #{} 的快照检查", requirement.id)))
                                .on_hover_text(format!("在快照 #{} 的临时目录中，以工作台的宿主权限运行采用的命令。", snapshot.id)).clicked()
                            {
                                let cancel = Arc::new(AtomicBool::new(false));
                                let request = self.request(SaveEffect::RunCheck(cancel.clone()));
                                action = Some(Action::RunDeliveryCheck {
                                    ticket, expected_contract_revision: summary.contract_revision,
                                    snapshot_id: snapshot.id, requirement_id: requirement.id, cancel, request,
                                });
                            }
                        }
                    }
                    if let Some(frozen) = frozen_check
                        && requirement.behavior.as_ref().is_none_or(|check| check.id != frozen.id)
                        && let Some(receipt) = &frozen.receipt
                    {
                        note(ui, "最近快照上的既往检查", pal);
                        show_receipt(ui, receipt, pal);
                    }
                    if let Some(host) = host && !requirement.withdrawn
                        && ui.add_enabled(self.saving.is_none(), egui::Button::new(format!("为需求 #{} 采用检查", requirement.id))).clicked()
                    {
                        self.adoption = Some(CheckEditor::new(host, ui, project, requirement.id, summary.contract_revision, requirement.behavior.as_ref()));
                    }
                    if writable && self.adoption.as_ref().is_some_and(|editor| editor.requirement == requirement.id)
                        && let Some(editor) = &mut self.adoption
                    {
                        let (save, cancel) = ui.add_enabled_ui(self.saving.is_none(), |ui| {
                            editor.show(ui, pal, summary.contract_revision, !requirement.withdrawn)
                        }).inner;
                        if save && let Some(argv) = &editor.selected {
                            let expected_contract_revision = editor.contract_revision;
                            let requirement_id = editor.requirement;
                            let argv = argv.clone();
                            let verifier_paths = paths(&editor.verifier_paths);
                            let request = self.request(SaveEffect::AdoptCheck);
                            action = Some(Action::AdoptDeliveryCheck {
                                ticket, expected_contract_revision, requirement_id, argv, verifier_paths, request,
                            });
                        }
                        if cancel { self.adoption = None; }
                    }
                }
                if let Some(snapshot) = &summary.latest_snapshot {
                    note(ui, format!("最近交付快照 #{} · 需求版本 {} · {} 个文件", snapshot.id, snapshot.contract_revision, snapshot.files.len()), pal);
                }
                if !writable { note(ui, "只读观察窗口，可查看需求、快照和核验记录。", pal); return; }
                ui.add_enabled_ui(self.saving.is_none(), |ui| {
                    let mut save_disposition = false;
                    let mut cancel_disposition = false;
                    if let Some(editor) = &mut self.disposition {
                        ui.group(|ui| {
                            ui.label(format!("调整需求 #{} 的必需状态", editor.requirement));
                            ui.horizontal(|ui| {
                                ui.radio_value(&mut editor.disposition, Disposition::Required, "必需");
                                ui.radio_value(&mut editor.disposition, Disposition::Optional, "可选");
                                ui.radio_value(&mut editor.disposition, Disposition::Withdrawn, "撤销");
                            });
                            input(ui, &mut editor.reason, &format!("调整理由 #{}", editor.requirement), true);
                            let conflict = editor.contract_revision != summary.contract_revision;
                            if conflict {
                                ui.colored_label(pal.warn.fg, format!("状态调整草稿基于需求版本 {}，当前为版本 {}。请比较当前需求并重新打开状态编辑器。", editor.contract_revision, summary.contract_revision));
                            }
                            ui.horizontal(|ui| {
                                save_disposition = ui.add_enabled(!conflict && !editor.reason.trim().is_empty(), egui::Button::new(format!("保存必需状态 #{}", editor.requirement))).clicked();
                                cancel_disposition = ui.button("取消状态调整").clicked();
                            });
                        });
                    }
                    if save_disposition && let Some(editor) = &self.disposition {
                        let requirement_id = editor.requirement;
                        let required = editor.disposition != Disposition::Optional;
                        let withdrawn = editor.disposition == Disposition::Withdrawn;
                        let reason = editor.reason.trim().to_owned();
                        let expected_contract_revision = editor.contract_revision;
                        let request = self.request(SaveEffect::Disposition);
                        action = Some(Action::SetRequirementDisposition {
                            ticket, expected_contract_revision,
                            requirement_id, required, withdrawn, reason, request,
                        });
                    }
                    if cancel_disposition { self.disposition = None; }
                    input(ui, &mut self.text, "需求说明", true);
                    input(ui, &mut self.job, "关联任务编号（可选）", false);
                    egui::CollapsingHeader::new("文件验收标准（可选）").id_salt(("criterion", ticket)).show(ui, |ui| {
                        note(ui, "预期文件内容已知时，使用 SHA-256 核验。行为要求可以单独采用检查。", pal);
                        input(ui, &mut self.path, "验收文件（项目相对路径）", false);
                        input(ui, &mut self.expected, "预期内容指纹（SHA-256）", false);
                    });
                    let conflict = self.editing.as_ref().is_some_and(|editor| editor.contract_revision != summary.contract_revision);
                    if conflict && let Some(editor) = &self.editing {
                        ui.colored_label(pal.warn.fg, format!("需求修订草稿基于需求版本 {}，当前为版本 {}。请比较当前需求并重新打开需求编辑器。", editor.contract_revision, summary.contract_revision));
                    }
                    ui.horizontal(|ui| {
                        let label = if self.editing.is_some() { "保存需求修订" } else { "记录需求" };
                        if widgets::primary_button(ui, "", label, pal, !conflict && !self.text.trim().is_empty()).clicked() {
                            let job = if self.job.trim().is_empty() { Ok(None) } else { self.job.trim().parse::<i64>().map(Some) };
                            match job {
                                Ok(job) if job.is_none_or(|id| id > 0) => {
                                    let expected_revision = self.editing.as_ref().map_or(summary.contract_revision, |editor| editor.contract_revision);
                                    let requirement_id = self.editing.as_ref().map(|editor| editor.requirement);
                                    let request = self.request(SaveEffect::Requirement);
                                    action = Some(Action::SaveRequirement { ticket, expected_revision,
                                        requirement_id, text: self.text.clone(),
                                        path: (!self.path.trim().is_empty()).then(|| self.path.trim().to_owned()),
                                        expected_sha256: (!self.expected.trim().is_empty()).then(|| self.expected.trim().to_owned()), job, request });
                                }
                                _ => self.error = Some("任务编号必须是正整数。".into()),
                            }
                        }
                        if self.editing.is_some() && ui.button("取消修订").clicked() {
                            self.editing = None; self.text.clear(); self.path.clear(); self.expected.clear(); self.job.clear();
                        }
                    });
                    ui.add_space(6.0);
                    input(ui, &mut self.paths, "交付文件（每行一个项目相对路径）", true);
                    note(ui, "列出工单项目目录中的交付文件，以及运行检查所需的验证器和其他输入。", pal);
                    if widgets::primary_button(ui, "", "捕获交付快照", pal, !self.paths.trim().is_empty()).clicked() {
                        let paths = paths(&self.paths);
                        let request = self.request(SaveEffect::Refresh);
                        action = Some(Action::CaptureDelivery { ticket, expected_revision: summary.contract_revision, paths, request });
                    }
                });
            });
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    fn show_collapsed(state: &mut DeliveryState, ctx: &egui::Context, ticket: i64) {
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                state.show(ui, &Palette::light(), DeliveryContext {
                    data: PathBuf::from("delivery-request-test"),
                    ticket,
                    project: ".",
                    execution: "done",
                    host: None,
                    demo: false,
                    expanded: false,
                });
            });
        });
    }

    #[test]
    fn delivery_late_mutation_reply_cannot_replace_new_ticket_request() {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        let mut state = DeliveryState::default();
        show_collapsed(&mut state, &ctx, 1);
        let old_request = state.request(SaveEffect::RunCheck(Arc::default()));
        show_collapsed(&mut state, &ctx, 2);
        state.text = "第二张工单的需求".into();
        let new_request = state.request(SaveEffect::Requirement);
        assert_ne!(old_request, new_request);
        state.result(1, old_request, Err("迟到的旧工单失败回执".into()));
        assert_eq!(state.saving.as_ref().map(|pending| pending.0), Some(new_request));
        assert_eq!(state.text, "第二张工单的需求");
        assert!(state.error.is_none());
        state.result(2, new_request, Ok(()));
        assert!(state.saving.is_none());
        assert!(state.text.is_empty());
    }

    #[test]
    fn delivery_late_read_is_discarded_after_ticket_switch() {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        let mut state = DeliveryState::default();
        show_collapsed(&mut state, &ctx, 1);
        let (release, wait) = mpsc::channel();
        let (completed, completion) = mpsc::channel();
        state.loading = Some(Pending::spawn(&ctx, move || {
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
            let summary = DeliverySummary {
                ticket: 1,
                contract_revision: 99,
                requirements: Vec::new(),
                latest_snapshot: None,
                status: "unknown".into(),
                warning: None,
                gate: Default::default(),
            };
            completed.send(()).unwrap();
            Ok(DeliveryReading { summary, integration: Ok(Vec::new()) })
        }));
        show_collapsed(&mut state, &ctx, 2);
        assert!(state.loading.is_none());
        release.send(()).unwrap();
        completion.recv_timeout(Duration::from_secs(2)).unwrap();
        show_collapsed(&mut state, &ctx, 2);
        assert_eq!(state.key.as_ref().map(|key| key.1), Some(2));
        assert!(state.reading.is_none());
        assert!(state.error.is_none());
    }
}
