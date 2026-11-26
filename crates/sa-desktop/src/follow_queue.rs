use crate::host::{Action, QueueAction};
use crate::theme::{self, Palette};
use crate::widgets;
use egui::{Align, Layout, RichText, Ui, WidgetInfo, WidgetType, text::LayoutJob, vec2};
use egui_phosphor::regular as icon;
use sa_store::{HoldReason, QueuedFollow, Snapshot};
use std::collections::HashSet;

const DELETE: &str = "删除这条待发指令";

pub fn strip(
    ui: &mut Ui,
    snap: &Snapshot,
    pal: &Palette,
    writable: bool,
    pending: &HashSet<QueueAction>,
) -> Option<Action> {
    let count = snap.queue.len();
    if count == 0 {
        return None;
    }
    let ticket = snap.ticket.id;
    let mut action = None;
    ui.horizontal(|ui| {
        let header = match snap.queue_hold {
            Some(reason) => {
                let reason = match reason {
                    HoldReason::Stopped => "工单已停止",
                    HoldReason::Failed => "上一轮失败",
                    HoldReason::Interrupted => "上一轮已中断",
                };
                format!("追加队列已暂停 · {count} 条（{reason}）")
            }
            None => format!("排队中 · {count} 条，本轮结束后逐条发送"),
        };
        ui.label(
            RichText::new(header)
                .font(theme::strong(theme::SMALL))
                .color(pal.mute),
        );
        if snap.queue_hold.is_some() {
            if writable {
                let busy = pending.contains(&QueueAction::Resume { ticket });
                let label = if busy {
                    "正在继续…"
                } else {
                    "继续发送"
                };
                if ui
                    .add_enabled_ui(!busy, |ui| widgets::quiet_button(ui, "", label, pal, false))
                    .inner
                    .clicked()
                {
                    action = Some(Action::ResumeQueue { ticket, confirmed_resume: false });
                }
            } else {
                ui.label(muted("队列已暂停 · 请在主窗口继续", pal));
            }
        }
    });
    let font = theme::body(theme::SMALL);
    let height = (2.0 * ui.fonts_mut(|fonts| fonts.row_height(&font)))
        .ceil()
        .max(22.0);
    let gap = ui.spacing().item_spacing.y;
    let visible_rows = count.min(3) as f32;
    let viewport = visible_rows * height + (visible_rows - 1.0) * gap;
    egui::ScrollArea::vertical()
        .id_salt(("follow-queue", ticket))
        .max_height(viewport)
        .min_scrolled_height(viewport)
        .auto_shrink([false, true])
        .show_rows(ui, height, count, |ui, rows| {
            for item in &snap.queue[rows] {
                ui.push_id(item.id, |ui| {
                    let busy = pending.contains(&QueueAction::Delete {
                        ticket,
                        id: item.id,
                    });
                    if row(ui, item, height, pal, writable, busy) {
                        action = Some(Action::DeleteQueued {
                            ticket,
                            id: item.id,
                        });
                    }
                });
            }
        });
    ui.add_space(4.0);
    action
}

fn muted(text: &str, pal: &Palette) -> RichText {
    RichText::new(text)
        .font(theme::body(theme::SMALL))
        .color(pal.mute)
}

// Reserve the trailing controls first so the body wraps within the remaining width.
fn row(
    ui: &mut Ui,
    item: &QueuedFollow,
    height: f32,
    pal: &Palette,
    writable: bool,
    busy: bool,
) -> bool {
    let size = vec2(ui.available_width(), height);
    ui.allocate_ui_with_layout(size, Layout::right_to_left(Align::Center), |ui| {
        if !item.files.is_empty() {
            let count = format!("含 {} 个附件", item.files.len());
            ui.label(muted(&count, pal)).on_hover_ui(|ui| {
                for name in &item.files {
                    ui.label(name);
                }
            });
        }
        let mut delete = false;
        if item.steering {
            ui.label(muted("正在插入当前轮…", pal))
                .on_hover_text("正在送入当前轮，无法删除");
        } else if writable {
            delete = ui
                .add_enabled_ui(!busy, |ui| {
                    let response =
                        widgets::icon_button(ui, icon::TRASH, 22.0, pal.mute, pal.hover, false);
                    response.widget_info(|| {
                        WidgetInfo::labeled(WidgetType::Button, ui.is_enabled(), DELETE)
                    });
                    response.on_hover_text(DELETE).clicked()
                })
                .inner;
        }

        let width = ui.available_width().max(1.0);
        let mut job =
            LayoutJob::simple(item.body.clone(), theme::body(theme::SMALL), pal.ink, width);
        job.wrap.max_rows = 2;
        job.wrap.break_anywhere = true;
        job.wrap.overflow_character = Some('…');
        let galley = ui.painter().layout_job(job);
        let elided = galley.elided;
        let (rect, response) = ui.allocate_exact_size(vec2(width, height), egui::Sense::hover());
        let origin = egui::pos2(rect.left(), rect.center().y - galley.size().y / 2.0);
        ui.painter().galley(origin, galley, pal.ink);
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &item.body));
        if elided {
            response.on_hover_text(&item.body);
        }
        delete
    })
    .inner
}
