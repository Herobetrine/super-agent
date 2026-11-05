//! Token usage reported by the harnesses, including coverage limitations.

use crate::{
    labels,
    theme::{self, Palette},
};
use egui::{RichText, Ui};
use sa_store::{TicketUsage, TokenCounts};

fn number(value: Option<u64>) -> String {
    value
        .map(|number| {
            let digits = number.to_string();
            let mut out = String::new();
            for (index, character) in digits.chars().enumerate() {
                if index > 0 && (digits.len() - index).is_multiple_of(3) {
                    out.push(',');
                }
                out.push(character);
            }
            out
        })
        .unwrap_or_else(|| "未报告".into())
}

fn counts(ui: &mut Ui, tokens: &TokenCounts) {
    egui::Grid::new(ui.id().with("counts"))
        .num_columns(2)
        .spacing([20.0, 5.0])
        .show(ui, |ui| {
            for (name, value) in [
                ("总令牌", tokens.total_tokens),
                ("输入（含缓存）", tokens.input_tokens),
                ("输出", tokens.output_tokens),
                ("缓存读取", tokens.cache_read_input_tokens),
                ("缓存写入", tokens.cache_creation_input_tokens),
                ("推理输出", tokens.reasoning_output_tokens),
            ] {
                ui.label(name);
                ui.monospace(number(value));
                ui.end_row();
            }
        });
}

pub fn button(ui: &mut Ui, usage: &TicketUsage, pal: &Palette) {
    let response = ui
        .button(
            RichText::new(format!("{} 用量", egui_phosphor::regular::GAUGE))
                .font(theme::body(theme::SMALL))
                .color(pal.mute),
        )
        .on_hover_text("查看执行端报告的令牌用量");
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "令牌用量"));
    egui::Popup::menu(&response).show(|ui| {
        ui.set_width(330.0);
        ui.label(RichText::new("执行端已报告用量").strong());
        if usage.pending {
            ui.label("正在汇总较早的记录…");
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(250));
        }
        if usage.partial || usage.unreported_jobs > 0 {
            ui.label(
                RichText::new("当前显示已报告的用量，部分调用或历史记录缺少计数。")
                    .small()
                    .color(pal.warn.fg),
            );
        }
        counts(ui, &usage.totals);
        if usage.unreported_jobs > 0 {
            ui.label(format!("{} 个任务尚未报告用量", usage.unreported_jobs));
        }
        ui.separator();
        egui::ScrollArea::vertical()
            .max_height(310.0)
            .show(ui, |ui| {
                for job in &usage.jobs {
                    let title = format!(
                        "{} #{} · {}",
                        labels::agent_name(&job.agent),
                        job.job,
                        number(job.tokens.total_tokens)
                    );
                    ui.push_id(job.job, |ui| {
                        ui.collapsing(title, |ui| {
                            counts(ui, &job.tokens);
                            if job.agent == "claude" {
                                ui.label("主循环用量，内部子任务未计入。");
                            }
                            ui.label(format!(
                                "已报告 {} 次执行 · 未报告 {} 次",
                                job.reported_attempts, job.unreported_attempts
                            ));
                            if job.partial {
                                ui.label(
                                    RichText::new("此任务的计数存在缺口")
                                        .small()
                                        .color(pal.warn.fg),
                                );
                            }
                        });
                    });
                }
            });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_and_zero_usage_remain_distinct() {
        assert_eq!(number(None), "未报告");
        assert_eq!(number(Some(0)), "0");
        assert_eq!(number(Some(1234567)), "1,234,567");
    }
}
