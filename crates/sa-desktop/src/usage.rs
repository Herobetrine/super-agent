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

fn hover_item(ui: &mut Ui, text: &str, label: &str, pal: &Palette, details: impl FnOnce(&mut Ui)) {
    let response = ui.add(
        egui::Label::new(
            RichText::new(text)
                .font(theme::body(theme::TINY))
                .color(pal.mute),
        )
        .selectable(false)
        .sense(egui::Sense::hover()),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, label));
    response.on_hover_ui(details);
}

fn coverage(ui: &mut Ui, usage: &TicketUsage, pal: &Palette) {
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
    if usage.jobs.iter().any(|job| job.agent == "claude") {
        ui.label("Claude 仅统计主循环，内部子任务未计入。");
    }
}

fn rate(value: Option<f64>) -> String {
    value
        .map(|v| format!("{v:.2} Token/s"))
        .unwrap_or_else(|| "暂无可计时样本".into())
}

fn hit_rate(tokens: &TokenCounts) -> String {
    tokens
        .cache_hit_rate()
        .map(|v| format!("{:.2}%", v * 100.0))
        .unwrap_or_else(|| "未报告或输入为零".into())
}

/// Quiet composer footer: exact values only exist inside hover details.
pub fn footer(ui: &mut Ui, usage: &TicketUsage, pal: &Palette) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        hover_item(ui, &format!("{} 用量", egui_phosphor::regular::GAUGE), "令牌用量", pal, |ui| {
        ui.set_width(330.0);
        ui.label(RichText::new("执行端已报告用量").strong());
        coverage(ui, usage, pal);
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
        hover_item(ui, "Token 速率", "Token 速率", pal, |ui| {
            ui.set_max_width(350.0);
            ui.label(RichText::new("平均输出速率").strong());
            ui.monospace(rate(usage.performance.output_tokens_per_second()));
            ui.label("可计时执行的输出 Token ÷ 执行耗时，包含工具调用和等待；排除轮次间空闲。并行任务按各自耗时加权，非模型纯生成速度。");
            ui.label("执行中统计截至最近一次用量报告，未上报的生成进度不估算。");
            if usage.performance.samples > 0 {
                ui.label(format!("计入输出 {} Token · 耗时 {:.3} 秒 · {} 个样本",
                    number(Some(usage.performance.output_tokens)), usage.performance.elapsed_ms as f64 / 1000.0,
                    usage.performance.samples));
            }
            if usage.performance.partial {
                ui.label("部分执行缺少完整计时或输出，仅展示可用样本。");
            }
            coverage(ui, usage, pal);
            egui::ScrollArea::vertical().id_salt("rate-jobs").max_height(240.0).show(ui, |ui| {
                for job in &usage.jobs {
                    ui.label(format!("{} #{} · {}", labels::agent_name(&job.agent), job.job,
                        rate(job.performance.output_tokens_per_second())));
                }
            });
        });
        hover_item(ui, "缓存命中率", "缓存命中率", pal, |ui| {
            ui.set_max_width(350.0);
            ui.label(RichText::new("输入缓存命中率").strong());
            ui.monospace(hit_rate(&usage.totals));
            ui.label("缓存读取 Token ÷ 全部输入 Token（含缓存读取与写入）。缓存写入不算命中；缺失字段或零输入不估算。");
            ui.label(format!("缓存读取 {} · 输入 {}", number(usage.totals.cache_read_input_tokens), number(usage.totals.input_tokens)));
            coverage(ui, usage, pal);
            egui::ScrollArea::vertical().id_salt("cache-jobs").max_height(240.0).show(ui, |ui| {
                for job in &usage.jobs {
                    ui.label(format!("{} #{} · {}", labels::agent_name(&job.agent), job.job, hit_rate(&job.tokens)));
                }
            });
        });
        let state = if usage.pending { "汇总中" }
            else if usage.totals.total_tokens.is_none() { "尚未完整报告" }
            else if usage.partial { "部分统计" } else { "已更新" };
        ui.label(RichText::new(state).font(theme::body(theme::TINY)).color(pal.faint));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};
    use sa_store::usage::UsagePerformance;

    #[test]
    fn unknown_and_zero_usage_remain_distinct() {
        assert_eq!(number(None), "未报告");
        assert_eq!(number(Some(0)), "0");
        assert_eq!(number(Some(1234567)), "1,234,567");
    }

    #[test]
    fn footer_hides_exact_metrics_until_hover_and_does_not_pin_on_click() {
        let usage = TicketUsage {
            totals: TokenCounts {
                total_tokens: Some(1250),
                input_tokens: Some(1000),
                output_tokens: Some(250),
                cache_read_input_tokens: Some(750),
                ..Default::default()
            },
            performance: UsagePerformance {
                output_tokens: 250,
                elapsed_ms: 2000,
                samples: 1,
                partial: false,
            },
            ..Default::default()
        };
        let mut h = Harness::builder()
            .with_size(egui::vec2(380.0, 560.0))
            .build_ui(move |ui| {
                crate::theme::install_fonts(ui.ctx());
                ui.style_mut().interaction.tooltip_delay = 0.0;
                ui.label("输入框上方");
                footer(ui, &usage, &Palette::light());
            });
        h.run_steps(3);
        assert!(h.query_by_label("1,250").is_none());
        assert!(h.query_by_label("125.00 Token/s").is_none());
        assert!(h.query_by_label("75.00%").is_none());
        for (label, detail) in [
            ("令牌用量", "1,250"),
            ("Token 速率", "125.00 Token/s"),
            ("缓存命中率", "75.00%"),
        ] {
            h.get_by_label(label).hover();
            h.run_steps(40);
            assert!(
                h.query_by_label(detail).is_some(),
                "missing hover value for {label}"
            );
            h.get_by_label(label).click();
            h.run_steps(3);
            h.get_by_label("输入框上方").hover();
            h.run_steps(40);
            assert!(
                h.query_by_label(detail).is_none(),
                "pinned hover value for {label}"
            );
        }
        for label in ["令牌用量", "Token 速率", "缓存命中率"] {
            let rect = h.get_by_label(label).rect();
            assert!(rect.left() >= 0.0 && rect.right() <= 380.0);
        }
    }
}
