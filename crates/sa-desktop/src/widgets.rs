//! Painted widgets shared by every screen: buttons, chips, status marks,
//! avatars, tabs, sheets and dialog chrome.
//!
//! Three button weights, in falling order of emphasis: [`primary_button`]
//! (one per screen or dialog), [`outline_button`] (secondary actions) and
//! [`quiet_button`] (everything else). States read as a coloured dot plus
//! text ([`status`]); tinted [`chip`]s are for counts and short tags.

use crate::labels::agent_monogram;
use crate::theme::{self, Palette, Tone};
use egui::{
    Align2, Color32, CornerRadius, FontId, Galley, Pos2, Rect, Response, Sense, Shape, Stroke, Ui,
    UiBuilder, Vec2, pos2, text::LayoutJob, vec2,
};
use std::sync::Arc;

const R: u8 = theme::CONTROL_RADIUS;

/// Single-line galley, elided with "…" when wider than `max_w`.
pub fn line_galley(ui: &Ui, text: &str, font: FontId, color: Color32, max_w: f32) -> Arc<Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap.max_width = max_w.max(8.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    ui.painter().layout_job(job)
}

fn with_icon(icon: &str, text: &str) -> String {
    if icon.is_empty() {
        text.to_string()
    } else if text.is_empty() {
        icon.to_string()
    } else {
        format!("{icon}  {text}")
    }
}

/// Tinted tag for counts and short labels.
pub fn chip(ui: &mut Ui, text: &str, tone: Tone, size: f32) -> Response {
    let galley = line_galley(ui, text, theme::body(size), tone.fg, 240.0);
    let pad = vec2(7.0, 2.5);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + pad * 2.0, Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(6), tone.bg);
        ui.painter().galley(rect.min + pad, galley, tone.fg);
    }
    resp
}

/// Chip with an icon prefix, announced to assistive tech by its text.
pub fn badge(ui: &mut Ui, icon: &str, text: &str, tone: Tone) -> Response {
    let response = chip(ui, &with_icon(icon, text), tone, theme::SMALL);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
    response
}

/// State mark: a filled dot, haloed while the work is live.
pub fn dot(ui: &Ui, center: Pos2, color: Color32, live: bool) {
    let p = ui.painter();
    if live {
        p.circle_filled(center, 6.0, color.gamma_multiply(0.22));
    }
    p.circle_filled(center, 3.25, color);
}

/// A state as a dot plus text, without a background.
pub fn status(ui: &mut Ui, text: &str, color: Color32, live: bool) -> Response {
    let galley = line_galley(ui, text, theme::body(theme::SMALL), color, 240.0);
    let lead = 16.0;
    let size = vec2(lead + galley.size().x, galley.size().y.max(18.0));
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
    if ui.is_rect_visible(rect) {
        dot(ui, pos2(rect.left() + 6.0, rect.center().y + 0.5), color, live);
        ui.painter().galley(
            pos2(rect.left() + lead, rect.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
    }
    resp
}

/// Agent avatar: a rounded tile in the agent's colour with its monogram.
/// Agents are tools, not people, so the tile is square rather than round.
pub fn avatar_at(ui: &Ui, center: Pos2, radius: f32, agent: &str, pal: &Palette) {
    let p = ui.painter();
    let rect = Rect::from_center_size(center, Vec2::splat(radius * 2.0));
    p.rect_filled(
        rect,
        CornerRadius::same((radius * 0.62).round() as u8),
        pal.agent_color(agent),
    );
    p.text(
        center + vec2(0.0, 0.5),
        Align2::CENTER_CENTER,
        agent_monogram(agent),
        theme::strong((radius * 0.8).max(7.5)),
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
        let t = ui
            .ctx()
            .animate_bool_with_time(resp.id.with("hover"), active || resp.hovered(), 0.1);
        if t > 0.0 {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(R), hover_bg.gamma_multiply(t));
        }
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            icon,
            theme::body((size * 0.54).clamp(12.0, 18.0)),
            fg,
        );
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Toggle for a side panel: a window outline with the panel's side marked,
/// filled in while the panel is open.
pub fn panel_button(ui: &mut Ui, right: bool, size: f32, pal: &Palette, open: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let label = if right { "项目面板" } else { "会话侧栏" };
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, open, label));
    if ui.is_rect_visible(rect) {
        let t = ui
            .ctx()
            .animate_bool_with_time(resp.id.with("hover"), resp.hovered(), 0.1);
        let p = ui.painter();
        if t > 0.0 {
            p.rect_filled(rect, CornerRadius::same(R), pal.hover.gamma_multiply(t));
        }
        let fg = if open { pal.ink } else { pal.mute };
        let frame = Rect::from_center_size(rect.center(), vec2(16.0, 13.0));
        let x = if right { frame.right() - 5.5 } else { frame.left() + 5.5 };
        if open {
            let pane = if right {
                Rect::from_min_max(pos2(x, frame.top()), frame.max)
            } else {
                Rect::from_min_max(frame.min, pos2(x, frame.bottom()))
            };
            p.rect_filled(pane.shrink(1.0), CornerRadius::same(2), fg.gamma_multiply(0.28));
        }
        let stroke = Stroke::new(1.3_f32, fg);
        p.rect_stroke(frame, CornerRadius::same(3), stroke, egui::StrokeKind::Inside);
        p.line_segment([pos2(x, frame.top() + 1.0), pos2(x, frame.bottom() - 1.0)], stroke);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Borderless text button, optionally with an icon; filled while `active`.
pub fn quiet_button(ui: &mut Ui, icon: &str, text: &str, pal: &Palette, active: bool) -> Response {
    let enabled = ui.is_enabled();
    let color = if enabled { pal.ink } else { pal.faint };
    let galley = line_galley(ui, &with_icon(icon, text), theme::body(theme::UI), color, 400.0);
    let pad = vec2(9.0, 5.5);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + pad * 2.0, Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, text));
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if active {
            p.rect_filled(rect, CornerRadius::same(R), pal.selected);
        } else {
            let t = ui
                .ctx()
                .animate_bool_with_time(resp.id.with("hover"), enabled && resp.hovered(), 0.1);
            if t > 0.0 {
                p.rect_filled(rect, CornerRadius::same(R), pal.hover.gamma_multiply(t));
            }
        }
        p.galley(rect.min + pad, galley, color);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Bordered button for secondary actions.
pub fn outline_button(ui: &mut Ui, icon: &str, text: &str, pal: &Palette) -> Response {
    let enabled = ui.is_enabled();
    let color = if enabled { pal.ink } else { pal.faint };
    let galley = line_galley(ui, &with_icon(icon, text), theme::body(theme::UI), color, 400.0);
    let pad = vec2(11.0, 5.5);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + pad * 2.0, Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, text));
    if ui.is_rect_visible(rect) {
        let t = ui
            .ctx()
            .animate_bool_with_time(resp.id.with("hover"), enabled && resp.hovered(), 0.1);
        ui.painter().rect(
            rect,
            CornerRadius::same(R),
            pal.surface.lerp_to_gamma(pal.hover, t * 0.7),
            Stroke::new(1.0_f32, pal.line),
            egui::StrokeKind::Inside,
        );
        ui.painter().galley(rect.min + pad, galley, color);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Bordered button in the failure colour for destructive actions.
pub fn danger_button(ui: &mut Ui, icon: &str, text: &str, pal: &Palette) -> Response {
    let enabled = ui.is_enabled();
    let color = if enabled { pal.fail.fg } else { pal.faint };
    let galley = line_galley(ui, &with_icon(icon, text), theme::body(theme::UI), color, 400.0);
    let pad = vec2(11.0, 5.5);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + pad * 2.0, Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, text));
    if ui.is_rect_visible(rect) {
        let t = ui
            .ctx()
            .animate_bool_with_time(resp.id.with("hover"), enabled && resp.hovered(), 0.1);
        ui.painter().rect(
            rect,
            CornerRadius::same(R),
            pal.surface.lerp_to_gamma(pal.fail.bg, t),
            Stroke::new(1.0_f32, pal.fail.fg.gamma_multiply(0.35)),
            egui::StrokeKind::Inside,
        );
        ui.painter().galley(rect.min + pad, galley, color);
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

/// Activity spinner sized to sit on a text line.
pub fn spinner(ui: &mut Ui) -> Response {
    ui.add(egui::Spinner::new().size(15.0))
}

/// A one-pixel horizontal rule across the available width.
pub fn rule(ui: &mut Ui, color: Color32) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(r, 0.0, color);
}

/// Small muted heading above a group of controls or rows.
pub fn section(ui: &mut Ui, text: &str, pal: &Palette) -> Response {
    ui.label(
        egui::RichText::new(text)
            .font(theme::strong(theme::SMALL))
            .color(pal.mute),
    )
}

/// Outlined block for an item that stands on its own inside a sheet.
/// Do not nest cards; separate rows with [`rule`] instead.
pub fn card(pal: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(pal.surface)
        .stroke(Stroke::new(1.0_f32, pal.line))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(12, 10))
}

/// Borderless quiet block (quoted results, summaries, code-like detail).
pub fn well(pal: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(pal.surface_alt)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(12, 9))
}

/// Tinted callout frame.
pub fn callout(tone: Tone) -> egui::Frame {
    egui::Frame::new()
        .fill(tone.bg)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(14, 10))
}

/// Paint a 2px lane on the left edge of `rect` (who is speaking).
pub fn left_bar(ui: &Ui, rect: Rect, color: Color32) {
    ui.painter().rect_filled(
        Rect::from_min_size(rect.min, vec2(2.0, rect.height())),
        CornerRadius::same(1),
        color,
    );
}

/// The window is a grey canvas carrying white sheets. Paints one sheet over
/// the whole of `ui` and runs `add` inside it, clipped to the sheet.
pub fn sheet<R>(ui: &mut Ui, pal: &Palette, add: impl FnOnce(&mut Ui) -> R) -> R {
    let rect = ui.max_rect();
    ui.painter().rect(
        rect,
        CornerRadius::same(theme::RADIUS),
        pal.surface,
        Stroke::new(1.0_f32, pal.line),
        egui::StrokeKind::Inside,
    );
    let inner = rect.shrink(1.0);
    let out = ui
        .scope_builder(UiBuilder::new().max_rect(inner), |ui| {
            ui.set_clip_rect(inner.intersect(ui.clip_rect()));
            add(ui)
        })
        .inner;
    ui.advance_cursor_after_rect(rect);
    out
}

/// Soft fades at the clipped edges of a scroll viewport so content dissolves
/// into `bg` instead of being cut mid-line. Only the edges that hide content fade.
pub fn scroll_fades<R>(ui: &Ui, out: &egui::scroll_area::ScrollAreaOutput<R>, bg: Color32) {
    let max = (out.content_size.y - out.inner_rect.height()).max(0.0);
    // Scroll content is clipped `clip_rect_margin` beyond the viewport; cover that too.
    let margin = ui.visuals().clip_rect_margin;
    let rect = out.inner_rect.expand2(vec2(0.0, margin));
    let y = out.state.offset.y;
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    let band = |top: f32, h: f32, from_bg_at_top: bool| {
        let r = Rect::from_min_max(pos2(rect.left(), top), pos2(rect.right(), top + h));
        let clear = Color32::TRANSPARENT;
        let (a, b) = if from_bg_at_top { (bg, clear) } else { (clear, bg) };
        let mut mesh = egui::Mesh::default();
        mesh.colored_vertex(r.left_top(), a);
        mesh.colored_vertex(r.right_top(), a);
        mesh.colored_vertex(r.left_bottom(), b);
        mesh.colored_vertex(r.right_bottom(), b);
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(2, 1, 3);
        painter.add(Shape::mesh(mesh));
    };
    // A short solid strip hides the sliver of a half-clipped line; the gradient
    // after it dissolves the rest.
    let solid = margin + 2.0;
    if y > 0.5 {
        painter.rect_filled(Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + solid)), 0.0, bg);
        band(rect.top() + solid, y.clamp(4.0, 12.0), true);
    }
    if y < max - 0.5 {
        let h = (max - y).clamp(4.0, 16.0);
        painter.rect_filled(Rect::from_min_max(pos2(rect.left(), rect.bottom() - solid), rect.max), 0.0, bg);
        band(rect.bottom() - solid - h, h, false);
    }
}

/// Checkbox with a small rounded-square box, filled with the accent when on.
/// Drop-in for `egui::Checkbox` (same accessibility role and label).
pub struct Check<'a> {
    on: &'a mut bool,
    text: egui::WidgetText,
}

impl<'a> Check<'a> {
    pub fn new(on: &'a mut bool, text: impl Into<egui::WidgetText>) -> Self {
        Self { on, text: text.into() }
    }
}

impl egui::Widget for Check<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        let Check { on, text } = self;
        let side = 16.0;
        let gap = 8.0;
        let wrap = (ui.available_width() - side - gap).max(24.0);
        let galley = text.into_galley(ui, Some(egui::TextWrapMode::Wrap), wrap, egui::TextStyle::Body);
        let h = galley.size().y.max(ui.spacing().interact_size.y);
        let (rect, mut resp) =
            ui.allocate_exact_size(vec2(side + gap + galley.size().x, h), Sense::click());
        if resp.clicked() {
            *on = !*on;
            resp.mark_changed();
        }
        let label = galley.text().to_owned();
        let enabled = ui.is_enabled();
        let checked = *on;
        resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, checked, &label));
        if ui.is_rect_visible(rect) {
            let v = ui.visuals();
            let accent = v.selection.stroke.color;
            let t = ui.ctx().animate_bool_with_time(resp.id.with("check"), checked, 0.12);
            // Centre the box on the first text row so wrapped labels stay aligned.
            let text_top = rect.top() + (h - galley.size().y) / 2.0;
            let row_h = galley.rows.first().map_or(galley.size().y, |r| r.rect().height());
            let b = Rect::from_min_size(pos2(rect.left(), text_top + (row_h - side) / 2.0), Vec2::splat(side));
            let rest = if resp.hovered() { v.widgets.hovered.bg_stroke.color } else { v.widgets.inactive.bg_stroke.color };
            let fill = v.widgets.inactive.bg_fill.lerp_to_gamma(accent, t);
            let edge = rest.lerp_to_gamma(accent, t);
            let p = ui.painter();
            p.rect(b, CornerRadius::same(4), fill, Stroke::new(1.2_f32, edge), egui::StrokeKind::Inside);
            if t > 0.0 {
                let mark = if v.dark_mode { v.panel_fill } else { Color32::WHITE };
                let c = b.center();
                p.add(Shape::line(
                    vec![c + vec2(-3.6, 0.2), c + vec2(-1.0, 2.8), c + vec2(3.8, -2.6)],
                    Stroke::new(1.8_f32, mark.gamma_multiply(t)),
                ));
            }
            let text_color = v.text_color();
            p.galley(pos2(b.right() + gap, text_top), galley, text_color);
        }
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }
}

/// Placeholder text for a `TextEdit`. Plain `&str` hints pick up the global
/// text override (full ink), so give them the weak text colour explicitly.
pub fn hint(ui: &Ui, text: &str) -> egui::RichText {
    egui::RichText::new(text).color(ui.visuals().weak_text_color())
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
            theme::body(11.0),
            visuals.fg_stroke.color.gamma_multiply(0.7),
        );
    })
}

/// Make the combo boxes and buttons added to `ui` borderless until hovered
/// (toolbars, where a row of outlined controls would read as clutter).
pub fn quiet_controls(ui: &mut Ui, pal: &Palette) {
    let w = &mut ui.visuals_mut().widgets;
    w.inactive.weak_bg_fill = Color32::TRANSPARENT;
    w.inactive.bg_stroke = Stroke::NONE;
    w.hovered.weak_bg_fill = pal.hover;
    w.hovered.bg_stroke = Stroke::NONE;
    w.active.weak_bg_fill = pal.selected;
    w.active.bg_stroke = Stroke::NONE;
    w.open.weak_bg_fill = pal.selected;
    w.open.bg_stroke = Stroke::NONE;
}

/// Filled accent button for the primary action of a dialog or screen.
pub fn primary_button(ui: &mut Ui, icon: &str, text: &str, pal: &Palette, enabled: bool) -> Response {
    let fg = pal.on_accent;
    let galley = line_galley(ui, &with_icon(icon, text), theme::strong(theme::UI), fg, 400.0);
    let pad = vec2(13.0, 6.0);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(galley.size() + pad * 2.0, sense);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, text));
    if ui.is_rect_visible(rect) {
        let hover = ui.ctx().animate_bool(resp.id.with("hover"), enabled && resp.hovered());
        let mut fill = pal.accent;
        if !enabled {
            fill = pal.surface.lerp_to_gamma(pal.accent, 0.3);
        } else if hover > 0.0 {
            fill = fill.lerp_to_gamma(pal.surface, 0.18 * hover);
        }
        ui.painter().rect_filled(rect, CornerRadius::same(R), fill);
        ui.painter().galley(rect.min + pad, galley, fg);
    }
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// Switch with a label; returns the response of the whole row.
pub fn toggle(ui: &mut Ui, on: &mut bool, text: &str, pal: &Palette) -> Response {
    let galley = line_galley(ui, text, theme::body(theme::UI), pal.ink, ui.available_width() - 44.0);
    let track = vec2(30.0, 18.0);
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
        let off = pal.line.lerp_to_gamma(pal.faint, 0.45);
        let fill = off.lerp_to_gamma(pal.accent, t);
        let knob = Color32::WHITE.lerp_to_gamma(pal.on_accent, t);
        let p = ui.painter();
        p.rect_filled(tr, CornerRadius::same(9), if enabled { fill } else { fill.gamma_multiply(0.5) });
        let x = egui::lerp(tr.left() + 9.0..=tr.right() - 9.0, t);
        p.circle_filled(pos2(x, tr.center().y), 6.5, knob);
        p.galley(pos2(tr.right() + 10.0, rect.center().y - galley.size().y / 2.0), galley, pal.ink);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Frame shared by every modal dialog.
pub fn modal_frame(pal: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(pal.surface)
        .stroke(Stroke::new(1.0_f32, pal.line))
        .corner_radius(CornerRadius::same(14))
        .shadow(egui::Shadow {
            offset: [0, 16],
            blur: 48,
            spread: 0,
            color: Color32::from_black_alpha(if pal.dark { 130 } else { 36 }),
        })
        .inner_margin(egui::Margin::same(20))
}

/// Dialog header: title, optional subtitle and a close button.
/// Returns true when the close button was clicked.
pub fn modal_header(ui: &mut Ui, _icon: &str, title: &str, subtitle: &str, pal: &Palette) -> bool {
    let mut close = false;
    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 3.0;
            ui.set_max_width((ui.available_width() - 36.0).max(80.0));
            ui.label(egui::RichText::new(title).font(theme::strong(16.0)).color(pal.ink));
            if !subtitle.is_empty() {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(subtitle).font(theme::body(theme::SMALL)).color(pal.mute),
                    )
                    .wrap(),
                );
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            let r = icon_button(ui, egui_phosphor::regular::X, 26.0, pal.mute, pal.hover, false).on_hover_text("关闭");
            r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "关闭对话框"));
            close = r.clicked();
        });
    });
    ui.add_space(14.0);
    close
}

/// Segmented control (a few exclusive views inside a dialog); returns the newly picked index.
pub fn segmented(ui: &mut Ui, items: &[&str], current: usize, pal: &Palette) -> Option<usize> {
    let mut picked = None;
    egui::Frame::new()
        .fill(pal.pale)
        .corner_radius(CornerRadius::same(9))
        .inner_margin(egui::Margin::same(3))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.horizontal(|ui| {
                for (i, name) in items.iter().enumerate() {
                    let selected = i == current;
                    let font = if selected { theme::strong(theme::UI) } else { theme::body(theme::UI) };
                    let color = if selected { pal.ink } else { pal.mute };
                    let g = line_galley(ui, name, font, color, 240.0);
                    let (rect, resp) = ui.allocate_exact_size(g.size() + vec2(22.0, 10.0), Sense::click());
                    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, selected, *name));
                    if selected {
                        let fill = if pal.dark { pal.selected } else { pal.surface };
                        ui.painter().rect_filled(rect.translate(vec2(0.0, 1.0)), CornerRadius::same(7), Color32::from_black_alpha(if pal.dark { 0 } else { 12 }));
                        ui.painter().rect_filled(rect, CornerRadius::same(7), fill);
                    } else if resp.hovered() {
                        ui.painter().rect_filled(rect, CornerRadius::same(7), pal.hover);
                    }
                    ui.painter().galley(rect.min + vec2(11.0, 5.0), g, color);
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !selected {
                        picked = Some(i);
                    }
                }
            });
        });
    picked
}

/// One entry of [`tabs`].
pub struct TabItem<'a> {
    pub name: &'a str,
    /// Small count after the name.
    pub count: Option<String>,
    /// Draw the count in the warning colour (something needs the operator).
    pub attention: bool,
}

/// Underlined tab row across the available width; returns the newly picked index.
/// Counts are dropped when the row is too narrow for them.
pub fn tabs(ui: &mut Ui, items: &[TabItem], current: usize, pal: &Palette) -> Option<usize> {
    let mut picked = None;
    let gap = 4.0;
    let pad = 8.0;
    let avail = ui.available_width();
    let galleys: Vec<_> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let selected = i == current;
            let font = if selected { theme::strong(theme::UI) } else { theme::body(theme::UI) };
            let color = if selected { pal.ink } else { pal.mute };
            let name = line_galley(ui, item.name, font, color, avail);
            let tone = if item.attention { pal.warn.fg } else { pal.faint };
            let count = item
                .count
                .as_ref()
                .map(|c| line_galley(ui, c, theme::body(theme::TINY), tone, 60.0));
            (name, count, color, tone)
        })
        .collect();
    let width = |with_counts: bool| -> f32 {
        galleys
            .iter()
            .map(|(n, c, _, _)| {
                let cw = if with_counts { c.as_ref().map_or(0.0, |g| g.size().x + 5.0) } else { 0.0 };
                n.size().x + cw + pad * 2.0
            })
            .sum::<f32>()
            + gap * (items.len().saturating_sub(1)) as f32
    };
    let counts = width(true) <= avail;
    let (strip, _) = ui.allocate_exact_size(vec2(avail, 34.0), Sense::hover());
    let mut x = strip.left();
    for (i, (item, (name, count, color, tone))) in items.iter().zip(galleys).enumerate() {
        let selected = i == current;
        let count = count.filter(|_| counts);
        let w = name.size().x + count.as_ref().map_or(0.0, |g| g.size().x + 5.0) + pad * 2.0;
        let rect = Rect::from_min_size(pos2(x, strip.top()), vec2(w, strip.height() - 4.0));
        x += w + gap;
        let resp = ui.interact(rect, ui.id().with(("tab", i)), Sense::click());
        resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, selected, item.name));
        let p = ui.painter();
        if resp.hovered() && !selected {
            p.rect_filled(rect, CornerRadius::same(R), pal.hover);
        }
        let ny = rect.center().y - name.size().y / 2.0;
        let nx = rect.left() + pad;
        let nw = name.size().x;
        p.galley(pos2(nx, ny), name, color);
        if let Some(count) = count {
            p.galley(pos2(nx + nw + 5.0, rect.center().y - count.size().y / 2.0 + 0.5), count, tone);
        } else if item.attention {
            // No room for the count: keep a mark that something is waiting.
            p.circle_filled(pos2(nx + nw + 4.0, ny + 3.0), 2.5, tone);
        }
        if selected {
            p.rect_filled(
                Rect::from_min_max(pos2(rect.left() + pad, strip.bottom() - 2.0), pos2(rect.right() - pad, strip.bottom())),
                CornerRadius::same(1),
                pal.ink,
            );
        }
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !selected {
            picked = Some(i);
        }
    }
    picked
}

/// Full-width row button with an icon and an optional shortcut hint.
pub fn wide_button(ui: &mut Ui, icon: &str, text: &str, hint: &str, pal: &Palette) -> Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 34.0), Sense::click());
    let enabled = ui.is_enabled();
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, text));
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool(resp.id.with("hover"), enabled && resp.hovered());
        let p = ui.painter();
        if !pal.dark {
            p.rect_filled(rect.translate(vec2(0.0, 1.0)), CornerRadius::same(R), Color32::from_black_alpha(8));
        }
        p.rect(
            rect,
            CornerRadius::same(R),
            pal.surface.lerp_to_gamma(pal.surface_alt, t),
            Stroke::new(1.0_f32, pal.line.lerp_to_gamma(pal.faint, t * 0.5)),
            egui::StrokeKind::Inside,
        );
        let ink = if enabled { pal.ink } else { pal.faint };
        p.text(rect.left_center() + vec2(11.0, 0.0), Align2::LEFT_CENTER, icon, theme::body(15.0), ink);
        p.text(rect.left_center() + vec2(33.0, 0.0), Align2::LEFT_CENTER, text, theme::body(theme::UI), ink);
        if !hint.is_empty() {
            p.text(rect.right_center() - vec2(10.0, 0.0), Align2::RIGHT_CENTER, hint, theme::body(theme::TINY), pal.faint);
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
