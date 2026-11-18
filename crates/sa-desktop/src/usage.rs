//! Token usage reported by the harnesses, including coverage limitations.

use crate::{
    labels,
    theme::{self, Palette},
    widgets,
};
use egui::{RichText, Ui};
use sa_store::{TicketUsage, TokenCounts, quota::Observed};

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

fn counts(ui: &mut Ui, tokens: &TokenCounts, pal: &Palette) {
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
                ui.label(dim(name));
                ui.label(mono_value(number(value), pal));
                ui.end_row();
            }
        });
}

fn dim(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).font(theme::body(theme::SMALL))
}

fn help(ui: &mut Ui, text: impl Into<String>, pal: &Palette) {
    ui.add(egui::Label::new(dim(text).color(pal.mute)).wrap());
}

fn mono_value(text: String, pal: &Palette) -> RichText {
    RichText::new(text).font(theme::mono(12.0)).color(pal.ink)
}

fn heading(ui: &mut Ui, text: &str, pal: &Palette) {
    ui.label(
        RichText::new(text)
            .font(theme::strong(theme::UI))
            .color(pal.ink),
    );
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
        help(ui, "正在汇总较早的记录…", pal);
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(250));
    }
    if usage.partial || usage.unreported_jobs > 0 {
        ui.add(
            egui::Label::new(
                dim("当前显示已报告的用量，部分调用或历史记录缺少计数。").color(pal.warn.fg),
            )
            .wrap(),
        );
    }
    if usage.jobs.iter().any(|job| job.agent == "claude") || usage.advisor_calls.iter().any(|call| call.agent == "claude") {
        help(ui, "Claude 仅统计主循环，内部子任务未计入。", pal);
    }
    if !usage.advisor_calls.is_empty() {
        help(ui, format!("顾问调用已计入总量：已报告 {} 次，未报告 {} 次。", usage.reported_advisor_calls, usage.unreported_advisor_calls), pal);
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

fn quota_has_data(observed: &Observed) -> bool {
    let window = &observed.window;
    window.exhausted
        || window.used.is_some_and(f64::is_finite)
        || window
            .resets_at
            .is_some_and(|reset| !labels::stamp(reset).is_empty())
}

fn quota_summary(quota: &[Observed]) -> Option<String> {
    let window = quota
        .iter()
        .map(|observed| &observed.window)
        .filter(|window| {
            window.exhausted
                || window
                    .used
                    .is_some_and(|used| used.is_finite() && used >= 0.5)
        })
        .max_by(|a, b| {
            a.exhausted.cmp(&b.exhausted).then_with(|| {
                a.used
                    .filter(|used| used.is_finite())
                    .unwrap_or(-1.0)
                    .total_cmp(&b.used.filter(|used| used.is_finite()).unwrap_or(-1.0))
            })
        })?;
    Some(if window.exhausted {
        "额度已用尽".into()
    } else {
        format!("额度 {:.0}%", window.used.unwrap_or(0.0) * 100.0)
    })
}

fn quota_line(observed: &Observed, now: f64) -> String {
    let window = &observed.window;
    let length = match window.minutes {
        Some(300) => "5 小时".into(),
        Some(10080) => "每周".into(),
        Some(minutes) => format!("{minutes} 分钟"),
        // Kinds named after an error code say nothing to the operator.
        None if matches!(window.kind.as_str(), "unknown" | "text" | "window" | "codex") => "额度窗口".into(),
        None => window.kind.clone(),
    };
    let used = if window.exhausted {
        "已用尽".into()
    } else {
        window
            .used
            .filter(|used| used.is_finite())
            .map(|used| format!("{:.0}%", used * 100.0))
            .unwrap_or_else(|| "未报告".into())
    };
    let reset = window
        .resets_at
        .map(|reset| {
            let time = labels::stamp(reset);
            if time.is_empty() {
                String::new()
            } else {
                format!(" · {time} 重置（{}后）", labels::duration(reset - now))
            }
        })
        .unwrap_or_default();
    format!(
        "{} · {length} · {used}{reset}",
        labels::agent_name(&observed.agent)
    )
}

fn quota_details(ui: &mut Ui, quota: &[Observed], pal: &Palette) {
    if !quota.iter().any(quota_has_data) {
        return;
    }
    heading(ui, "订阅额度窗口", pal);
    let now = labels::now_secs();
    for observed in quota.iter().filter(|observed| quota_has_data(observed)) {
        help(ui, quota_line(observed, now), pal);
    }
}

/// Quiet composer footer: exact token values only exist inside hover details.
pub fn footer(ui: &mut Ui, usage: &TicketUsage, quota: &[Observed], pal: &Palette) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        hover_item(ui, &format!("{} 用量", egui_phosphor::regular::GAUGE), "令牌用量", pal, |ui| {
        ui.set_width(330.0);
        heading(ui, "执行端已报告用量", pal);
        coverage(ui, usage, pal);
        counts(ui, &usage.totals, pal);
        if usage.unreported_jobs > 0 {
            help(ui, format!("{} 个任务尚未报告用量", usage.unreported_jobs), pal);
        }
        if quota.iter().any(quota_has_data) {
            widgets::rule(ui, pal.line_soft);
            quota_details(ui, quota, pal);
        }
        widgets::rule(ui, pal.line_soft);
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
                        ui.collapsing(RichText::new(title).font(theme::body(theme::UI)), |ui| {
                            counts(ui, &job.tokens, pal);
                            if job.agent == "claude" {
                                help(ui, "主循环用量，内部子任务未计入。", pal);
                            }
                            help(
                                ui,
                                format!(
                                    "已报告 {} 次执行 · 未报告 {} 次",
                                    job.reported_attempts, job.unreported_attempts
                                ),
                                pal,
                            );
                            if job.partial {
                                ui.label(dim("此任务的计数存在缺口").color(pal.warn.fg));
                            }
                        });
                    });
                }
                for call in &usage.advisor_calls {
                    let title = format!("顾问 {} · 任务 #{} · {}", labels::agent_name(&call.agent), call.job, number(call.tokens.total_tokens));
                    ui.push_id(&call.attempt_id, |ui| {
                        ui.collapsing(RichText::new(title).font(theme::body(theme::UI)), |ui| {
                            counts(ui, &call.tokens, pal);
                            help(ui, format!("第 {} 轮 · {}", call.turn, if call.reported { "有用量记录" } else { "未报告，不能视为零" }), pal);
                            if call.partial { help(ui, "此顾问调用的计数存在缺口。", pal); }
                        });
                    });
                }
            });
        });
        hover_item(ui, "Token 速率", "Token 速率", pal, |ui| {
            ui.set_max_width(350.0);
            heading(ui, "平均输出速率", pal);
            ui.label(mono_value(rate(usage.performance.output_tokens_per_second()), pal));
            help(ui, "可计时执行的输出 Token ÷ 执行耗时，包含工具调用和等待；排除轮次间空闲。并行任务按各自耗时加权，非模型纯生成速度。", pal);
            help(ui, "执行中统计截至最近一次用量报告，未上报的生成进度不估算。", pal);
            if usage.performance.samples > 0 {
                help(ui, format!("计入输出 {} Token · 耗时 {:.3} 秒 · {} 个样本",
                    number(Some(usage.performance.output_tokens)), usage.performance.elapsed_ms as f64 / 1000.0,
                    usage.performance.samples), pal);
            }
            if usage.performance.partial {
                help(ui, "部分执行缺少完整计时或输出，仅展示可用样本。", pal);
            }
            coverage(ui, usage, pal);
            egui::ScrollArea::vertical().id_salt("rate-jobs").max_height(240.0).show(ui, |ui| {
                for job in &usage.jobs {
                    ui.label(dim(format!("{} #{} · {}", labels::agent_name(&job.agent), job.job,
                        rate(job.performance.output_tokens_per_second()))));
                }
            });
        });
        hover_item(ui, "缓存命中率", "缓存命中率", pal, |ui| {
            ui.set_max_width(350.0);
            heading(ui, "输入缓存命中率", pal);
            ui.label(mono_value(hit_rate(&usage.totals), pal));
            help(ui, "缓存读取 Token ÷ 全部输入 Token（含缓存读取与写入）。缓存写入不算命中；缺失字段或零输入不估算。", pal);
            help(ui, format!("缓存读取 {} · 输入 {}", number(usage.totals.cache_read_input_tokens), number(usage.totals.input_tokens)), pal);
            coverage(ui, usage, pal);
            egui::ScrollArea::vertical().id_salt("cache-jobs").max_height(240.0).show(ui, |ui| {
                for job in &usage.jobs {
                    ui.label(dim(format!("{} #{} · {}", labels::agent_name(&job.agent), job.job, hit_rate(&job.tokens))));
                }
            });
        });
        if let Some(summary) = quota_summary(quota) {
            hover_item(ui, &summary, &summary, pal, |ui| {
                ui.set_max_width(350.0);
                quota_details(ui, quota, pal);
            });
        }
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

    fn quota_window(agent: &str, used: Option<f64>, minutes: Option<i64>) -> Observed {
        Observed {
            agent: agent.into(),
            provider: "subscription".into(),
            window: sa_store::quota::Window {
                kind: "primary".into(),
                used,
                resets_at: None,
                exhausted: false,
                minutes,
            },
            observed_at: labels::now_secs(),
        }
    }

    #[test]
    fn low_quota_and_old_snapshot_add_no_footer_segment() {
        for quota in [
            vec![],
            vec![quota_window("codex", None, None)],
            vec![quota_window("codex", Some(0.49), Some(300))],
        ] {
            let has_window = quota.iter().any(quota_has_data);
            assert_eq!(quota_summary(&quota), None);
            let mut h = Harness::builder()
                .with_size(egui::vec2(500.0, 560.0))
                .build_ui(move |ui| {
                    crate::theme::install_fonts(ui.ctx());
                    ui.style_mut().interaction.tooltip_delay = 0.0;
                    footer(ui, &TicketUsage::default(), &quota, &Palette::light());
                });
            h.run_steps(3);
            assert!(h.query_by_label("额度 49%").is_none());
            assert!(h.query_by_label("额度已用尽").is_none());
            assert!(h.query_by_label("订阅额度窗口").is_none());
            h.get_by_label("令牌用量").hover();
            h.run_steps(40);
            assert_eq!(h.query_by_label("订阅额度窗口").is_some(), has_window);
            if has_window {
                assert!(h.query_by_label("Codex · 5 小时 · 49%").is_some());
            }
        }
        assert_eq!(
            quota_summary(&[quota_window("codex", Some(0.5), Some(300))]),
            Some("额度 50%".into())
        );
    }

    #[test]
    fn quota_segment_shows_windows_only_on_hover() {
        let quota = vec![quota_window("codex", Some(0.85), Some(300))];
        let mut h = Harness::builder()
            .with_size(egui::vec2(500.0, 560.0))
            .build_ui(move |ui| {
                crate::theme::install_fonts(ui.ctx());
                ui.style_mut().interaction.tooltip_delay = 0.0;
                ui.label("输入框上方");
                footer(ui, &TicketUsage::default(), &quota, &Palette::light());
            });
        h.run_steps(3);
        assert!(h.query_by_label("额度 85%").is_some());
        assert!(h.query_by_label("Codex · 5 小时 · 85%").is_none());
        for label in ["令牌用量", "额度 85%"] {
            h.get_by_label(label).hover();
            h.run_steps(40);
            assert!(h.query_by_label("订阅额度窗口").is_some());
            assert!(h.query_by_label("Codex · 5 小时 · 85%").is_some());
            h.get_by_label(label).click();
            h.run_steps(3);
            h.get_by_label("输入框上方").hover();
            h.run_steps(40);
            assert!(h.query_by_label("订阅额度窗口").is_none());
        }
    }

    #[test]
    fn exhausted_quota_takes_priority_over_used_percent() {
        let mut exhausted = quota_window("claude", None, Some(10080));
        exhausted.window.exhausted = true;
        let quota = vec![quota_window("codex", Some(0.99), Some(300)), exhausted];
        assert_eq!(quota_summary(&quota), Some("额度已用尽".into()));
        let mut h = Harness::builder()
            .with_size(egui::vec2(500.0, 560.0))
            .build_ui(move |ui| {
                crate::theme::install_fonts(ui.ctx());
                ui.style_mut().interaction.tooltip_delay = 0.0;
                footer(ui, &TicketUsage::default(), &quota, &Palette::light());
            });
        h.run_steps(3);
        h.get_by_label("额度已用尽").hover();
        h.run_steps(40);
        assert!(h.query_by_label("Claude Code · 每周 · 已用尽").is_some());
        assert!(h.query_by_label("Codex · 5 小时 · 99%").is_some());
    }

    #[test]
    fn most_constrained_segment_keeps_all_current_windows_in_hover() {
        let quota = vec![
            quota_window("codex", Some(0.65), Some(300)),
            quota_window("claude", Some(0.85), Some(10080)),
            quota_window("codex", Some(0.2), Some(60)),
        ];
        assert_eq!(quota_summary(&quota), Some("额度 85%".into()));
        let mut h = Harness::builder()
            .with_size(egui::vec2(500.0, 560.0))
            .build_ui(move |ui| {
                crate::theme::install_fonts(ui.ctx());
                ui.style_mut().interaction.tooltip_delay = 0.0;
                footer(ui, &TicketUsage::default(), &quota, &Palette::light());
            });
        h.run_steps(3);
        h.get_by_label("额度 85%").hover();
        h.run_steps(40);
        for line in [
            "Codex · 5 小时 · 65%",
            "Claude Code · 每周 · 85%",
            "Codex · 60 分钟 · 20%",
        ] {
            assert!(
                h.query_by_label(line).is_some(),
                "missing quota window: {line}"
            );
        }
    }

    #[test]
    fn quota_reset_reuses_local_clock_and_duration_and_ignores_empty_windows() {
        let mut observed = quota_window("claude", None, None);
        assert!(!quota_has_data(&observed));
        assert_eq!(quota_summary(&[observed.clone()]), None);
        observed.window.kind = "seven_day_opus".into();
        observed.window.resets_at = Some(2_000_000_000.0);
        assert_eq!(
            quota_line(&observed, 2_000_000_000.0 - 5700.0),
            format!(
                "Claude Code · seven_day_opus · 未报告 · {} 重置（1 小时 35 分后）",
                labels::stamp(2_000_000_000.0)
            )
        );
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
                footer(ui, &usage, &[], &Palette::light());
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
