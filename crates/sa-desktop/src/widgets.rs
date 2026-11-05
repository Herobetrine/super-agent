//! Small painted widgets: chips, avatars, icon buttons, the brand mark.

use crate::labels::agent_monogram;
use crate::theme::{self, Palette, Tone};
use egui::{
    Align2, Color32, CornerRadius, FontId, Galley, Pos2, Rect, Response, Sense, Shape, Stroke, Ui,
    Vec2, pos2, text::LayoutJob, vec2,
};
use std::sync::Arc;

/// Single-line galley, elided with "…" when wider than `max_w`.
pub fn line_galley(ui: &Ui, text: &str, font: FontId, color: Color32, max_w: f32) -> Arc<Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap.max_width = max_w.max(8.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    ui.painter().layout_job(job)
}

/// Pill-shaped status chip.
pub fn chip(ui: &mut Ui, text: &str, tone: Tone, size: f32) -> Response {
    let galley = line_galley(ui, text, theme::body(size), tone.fg, 220.0);
    let pad = vec2(7.0, 2.5);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + pad * 2.0, Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(6), tone.bg);
        ui.painter().galley(rect.min + pad, galley, tone.fg);
    }
    resp
}

/// Chip with an icon prefix and optional outline.
pub fn badge(ui: &mut Ui, icon: &str, text: &str, tone: Tone) -> Response {
    let label = if icon.is_empty() {
        text.to_string()
    } else {
        format!("{icon}  {text}")
    };
    chip(ui, &label, tone, theme::SMALL)
}

/// Agent avatar circle with monogram.
pub fn avatar_at(ui: &Ui, center: Pos2, radius: f32, agent: &str, pal: &Palette) {
    let p = ui.painter();
    p.circle_filled(center, radius, pal.agent_color(agent));
    p.text(
        center + vec2(0.0, 0.5),
        Align2::CENTER_CENTER,
        agent_monogram(agent),
        theme::strong((radius * 0.82).max(7.5)),
        Color32::WHITE,
    );
}

pub fn avatar(ui: &mut Ui, agent: &str, diameter: f32, pal: &Palette) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(diameter), Sense::hover());
    if ui.is_rect_visible(rect) {
        avatar_at(ui, rect.center(), diameter / 2.0, agent, pal);
    }
    resp
}

/// Square icon button with a soft hover background.
pub fn icon_button(
    ui: &mut Ui,
    icon: &str,
    size: f32,
    fg: Color32,
    hover_bg: Color32,
    active: bool,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    if ui.is_rect_visible(rect) {
        let bg = if active || resp.hovered() {
            hover_bg
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, CornerRadius::same(8), bg);
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            icon,
            theme::body(size * 0.5),
            fg,
        );
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Text button with icon, quiet by default and outlined on hover.
pub fn quiet_button(ui: &mut Ui, icon: &str, text: &str, pal: &Palette, active: bool) -> Response {
    let label = if icon.is_empty() {
        text.to_string()
    } else {
        format!("{icon}  {text}")
    };
    let galley = line_galley(ui, &label, theme::body(13.0), pal.ink, 400.0);
    let pad = vec2(10.0, 6.0);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + pad * 2.0, Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), text));
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if active {
            p.rect_filled(rect, CornerRadius::same(8), pal.selected);
        } else if resp.hovered() {
            p.rect_filled(rect, CornerRadius::same(8), pal.hover);
        }
        p.galley(rect.min + pad, galley, pal.ink);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The Super Agent mark (`assets/mark.svg`) painted at `rect`.
pub fn brand_mark(ui: &Ui, rect: Rect, pal: &Palette) {
    let p = ui.painter();
    let s = rect.width() / 64.0;
    p.rect_filled(rect, CornerRadius::same((17.0 * s) as u8), pal.mark_bg);
    let pt = |x: f32, y: f32| pos2(rect.left() + x * s, rect.top() + y * s);
    let stroke = Stroke::new((5.0 * s).max(1.5), pal.mark_fg);
    for poly in [
        [
            pt(14.0, 35.0),
            pt(26.0, 16.0),
            pt(38.0, 16.0),
            pt(25.0, 37.0),
        ],
        [
            pt(26.0, 48.0),
            pt(38.0, 28.0),
            pt(50.0, 28.0),
            pt(38.0, 48.0),
        ],
    ] {
        p.add(Shape::closed_line(poly.to_vec(), stroke));
    }
}

/// A one-pixel horizontal rule across the available width.
pub fn rule(ui: &mut Ui, color: Color32) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(r, 0.0, color);
}

/// Card frame used across panels.
pub fn card(pal: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(pal.surface)
        .stroke(Stroke::new(1.0_f32, pal.line_soft))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(12, 10))
}

/// Tinted callout frame.
pub fn callout(tone: Tone) -> egui::Frame {
    egui::Frame::new()
        .fill(tone.bg)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(14, 10))
}

/// Paint a 3px accent bar on the left edge of `rect`.
pub fn left_bar(ui: &Ui, rect: Rect, color: Color32) {
    ui.painter().rect_filled(
        Rect::from_min_size(rect.min, vec2(3.0, rect.height())),
        CornerRadius {
            nw: 3,
            sw: 3,
            ne: 0,
            se: 0,
        },
        color,
    );
}

/// Combo box with a Phosphor caret instead of egui's triangle.
pub fn combo(id: impl std::hash::Hash) -> egui::ComboBox {
    egui::ComboBox::from_id_salt(id).icon(|ui, rect, visuals, open| {
        let caret = if open {
            egui_phosphor::regular::CARET_UP
        } else {
            egui_phosphor::regular::CARET_DOWN
        };
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            caret,
            theme::body(12.0),
            visuals.fg_stroke.color,
        );
    })
}

/// Filled accent button for the primary action of a dialog or screen.
pub fn primary_button(ui: &mut Ui, icon: &str, text: &str, pal: &Palette, enabled: bool) -> Response {
    let label = if icon.is_empty() {
        text.to_string()
    } else {
        format!("{icon}  {text}")
    };
    let fg = if pal.dark { pal.canvas } else { Color32::WHITE };
    let galley = line_galley(ui, &label, theme::strong(13.0), fg, 400.0);
    let pad = vec2(14.0, 7.0);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(galley.size() + pad * 2.0, sense);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, text));
    if ui.is_rect_visible(rect) {
        let hover = ui.ctx().animate_bool(resp.id.with("hover"), enabled && resp.hovered());
        let mut fill = pal.accent;
        if !enabled {
            fill = fill.gamma_multiply(0.4);
        } else if hover > 0.0 {
            fill = fill.lerp_to_gamma(if pal.dark { Color32::WHITE } else { Color32::BLACK }, 0.12 * hover);
        }
        ui.painter().rect_filled(rect, CornerRadius::same(9), fill);
        ui.painter().galley(rect.min + pad, galley, fg);
    }
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// iOS-style switch with a label; returns the response of the whole row.
pub fn toggle(ui: &mut Ui, on: &mut bool, text: &str, pal: &Palette) -> Response {
    let galley = line_galley(ui, text, theme::body(13.5), pal.ink, ui.available_width() - 48.0);
    let track = vec2(34.0, 20.0);
    let size = vec2(track.x + 10.0 + galley.size().x, track.y.max(galley.size().y));
    let enabled = ui.is_enabled();
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, *on, text));
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool(resp.id, *on);
        let tr = Rect::from_min_size(pos2(rect.left(), rect.center().y - track.y / 2.0), track);
        let off = if pal.dark { pal.line } else { hex_line(pal) };
        let fill = off.lerp_to_gamma(pal.accent, t);
        let p = ui.painter();
        p.rect_filled(tr, CornerRadius::same(10), if enabled { fill } else { fill.gamma_multiply(0.5) });
        let x = egui::lerp(tr.left() + 10.0..=tr.right() - 10.0, t);
        p.circle_filled(pos2(x, tr.center().y), 7.5, Color32::WHITE);
        p.galley(pos2(tr.right() + 10.0, rect.center().y - galley.size().y / 2.0), galley, pal.ink);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn hex_line(pal: &Palette) -> Color32 {
    pal.line.lerp_to_gamma(pal.faint, 0.4)
}

/// Frame shared by every modal dialog.
pub fn modal_frame(pal: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(pal.surface)
        .stroke(Stroke::new(1.0_f32, pal.line))
        .corner_radius(CornerRadius::same(16))
        .shadow(egui::Shadow {
            offset: [0, 18],
            blur: 48,
            spread: 0,
            color: Color32::from_black_alpha(if pal.dark { 120 } else { 40 }),
        })
        .inner_margin(egui::Margin::same(22))
}

/// Dialog header: icon tile, title, optional subtitle and a close button.
/// Returns true when the close button was clicked.
pub fn modal_header(ui: &mut Ui, icon: &str, title: &str, subtitle: &str, pal: &Palette) -> bool {
    let mut close = false;
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
        ui.painter().rect_filled(r, CornerRadius::same(10), pal.pale);
        ui.painter()
            .text(r.center(), Align2::CENTER_CENTER, icon, theme::body(18.0), pal.accent);
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.label(egui::RichText::new(title).font(theme::strong(16.0)).color(pal.ink));
            if !subtitle.is_empty() {
                ui.label(egui::RichText::new(subtitle).font(theme::body(theme::SMALL)).color(pal.mute));
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            let r = icon_button(ui, egui_phosphor::regular::X, 28.0, pal.mute, pal.hover, false).on_hover_text("关闭");
            r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "关闭对话框"));
            close = r.clicked();
        });
    });
    ui.add_space(12.0);
    close
}

/// Segmented control (tabs inside a dialog); returns the newly picked index.
pub fn segmented(ui: &mut Ui, items: &[&str], current: usize, pal: &Palette) -> Option<usize> {
    let mut picked = None;
    egui::Frame::new()
        .fill(if pal.dark { pal.canvas } else { pal.hover })
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(3))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.horizontal(|ui| {
                for (i, name) in items.iter().enumerate() {
                    let selected = i == current;
                    let font = if selected { theme::strong(13.0) } else { theme::body(13.0) };
                    let color = if selected { pal.ink } else { pal.mute };
                    let g = line_galley(ui, name, font, color, 240.0);
                    let (rect, resp) = ui.allocate_exact_size(g.size() + vec2(24.0, 12.0), Sense::click());
                    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, selected, *name));
                    if selected {
                        ui.painter().rect_filled(rect, CornerRadius::same(8), if pal.dark { pal.selected } else { pal.surface });
                    } else if resp.hovered() {
                        ui.painter().rect_filled(rect, CornerRadius::same(8), pal.line_soft.gamma_multiply(0.6));
                    }
                    ui.painter().galley(rect.min + vec2(12.0, 6.0), g, color);
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !selected {
                        picked = Some(i);
                    }
                }
            });
        });
    picked
}

/// Full-width outlined button with an icon and an optional shortcut hint.
pub fn wide_button(ui: &mut Ui, icon: &str, text: &str, hint: &str, pal: &Palette) -> Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 38.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, text));
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool(resp.id.with("hover"), resp.hovered());
        let p = ui.painter();
        p.rect(
            rect,
            CornerRadius::same(10),
            pal.surface.lerp_to_gamma(pal.hover, t),
            Stroke::new(1.0_f32, pal.line),
            egui::StrokeKind::Inside,
        );
        p.text(rect.left_center() + vec2(14.0, 0.0), Align2::LEFT_CENTER, icon, theme::body(16.0), pal.accent);
        p.text(rect.left_center() + vec2(38.0, 0.0), Align2::LEFT_CENTER, text, theme::strong(13.5), pal.ink);
        if !hint.is_empty() {
            p.text(rect.right_center() - vec2(12.0, 0.0), Align2::RIGHT_CENTER, hint, theme::body(theme::TINY), pal.faint);
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
