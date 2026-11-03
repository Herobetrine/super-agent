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

/// The Super Agent mark (`desktop/ui/mark.svg`) painted at `rect`.
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
