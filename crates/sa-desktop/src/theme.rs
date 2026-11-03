//! Brand palette, typography and the custom egui style.
//!
//! The light palette follows `desktop/ui/studio.css`; the dark palette keeps the
//! same green identity with low-glare surfaces.

use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Shadow, Stroke,
    TextStyle, Vec2, Visuals, epaint::text::FontTweak,
};
use std::sync::Arc;

pub const RADIUS: u8 = 12;
pub const BODY: f32 = 14.5;
pub const SMALL: f32 = 12.5;
pub const TINY: f32 = 11.5;
pub const MONO: f32 = 13.0;

/// Named family for semibold text (titles, speakers, **bold**).
pub fn semibold_family() -> FontFamily {
    FontFamily::Name("semibold".into())
}

pub fn body(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

pub fn strong(size: f32) -> FontId {
    FontId::new(size, semibold_family())
}

pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

#[derive(Clone, Copy, Debug)]
pub struct Tone {
    pub bg: Color32,
    pub fg: Color32,
}

#[derive(Clone, Debug)]
pub struct Palette {
    pub dark: bool,
    pub canvas: Color32,
    pub surface: Color32,
    pub surface_alt: Color32,
    pub sidebar: Color32,
    pub rail: Color32,
    pub rail_ink: Color32,
    pub rail_mute: Color32,
    pub ink: Color32,
    pub mute: Color32,
    pub faint: Color32,
    pub line: Color32,
    pub line_soft: Color32,
    pub accent: Color32,
    pub pale: Color32,
    pub hover: Color32,
    pub selected: Color32,
    pub user_bubble: Color32,
    pub code_bg: Color32,
    pub inline_code_bg: Color32,
    pub inline_code_fg: Color32,
    pub link: Color32,
    pub mark_bg: Color32,
    pub mark_fg: Color32,
    pub ok: Tone,
    pub run: Tone,
    pub wait: Tone,
    pub warn: Tone,
    pub fail: Tone,
    pub stop: Tone,
    pub neutral: Tone,
    pub report: Tone,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

const fn tone(bg: u32, fg: u32) -> Tone {
    Tone {
        bg: hex(bg),
        fg: hex(fg),
    }
}

impl Palette {
    pub fn light() -> Self {
        Self {
            dark: false,
            canvas: hex(0xf7f8f4),
            surface: hex(0xffffff),
            surface_alt: hex(0xfbfcf9),
            sidebar: hex(0xf3f5ef),
            rail: hex(0x1e302a),
            rail_ink: hex(0xe6efe7),
            rail_mute: hex(0x8fa597),
            ink: hex(0x263830),
            mute: hex(0x65746c),
            faint: hex(0x98a59d),
            line: hex(0xd9dfd8),
            line_soft: hex(0xe8ece6),
            accent: hex(0x355c47),
            pale: hex(0xe9efdf),
            hover: hex(0xebefe7),
            selected: hex(0xe3ebda),
            user_bubble: hex(0xe9f0df),
            code_bg: hex(0xf3f5f0),
            inline_code_bg: hex(0xedf1e8),
            inline_code_fg: hex(0x2f5340),
            link: hex(0x2f6a8f),
            mark_bg: hex(0xceef9f),
            mark_fg: hex(0x233c32),
            ok: tone(0xe3ecd9, 0x3b6042),
            run: tone(0xdceee1, 0x276444),
            wait: tone(0xe4ebf3, 0x3c5a7d),
            warn: tone(0xf8eed6, 0x7d5b14),
            fail: tone(0xf9e5d8, 0x8f481e),
            stop: tone(0xecebe6, 0x625d55),
            neutral: tone(0xeef0eb, 0x5b6a62),
            report: tone(0xf1f5eb, 0x47614b),
        }
    }

    pub fn dark() -> Self {
        Self {
            dark: true,
            canvas: hex(0x141a17),
            surface: hex(0x1a221e),
            surface_alt: hex(0x1f2824),
            sidebar: hex(0x171e1b),
            rail: hex(0x0e1412),
            rail_ink: hex(0xe1ebe3),
            rail_mute: hex(0x7d9286),
            ink: hex(0xe2e9e4),
            mute: hex(0x9aaaa1),
            faint: hex(0x6d7d74),
            line: hex(0x2d3934),
            line_soft: hex(0x25302b),
            accent: hex(0x8cc4a0),
            pale: hex(0x223129),
            hover: hex(0x222c27),
            selected: hex(0x26372d),
            user_bubble: hex(0x25382d),
            code_bg: hex(0x111714),
            inline_code_bg: hex(0x26322c),
            inline_code_fg: hex(0xb7dcc3),
            link: hex(0x8cc0e0),
            mark_bg: hex(0xceef9f),
            mark_fg: hex(0x233c32),
            ok: tone(0x24372a, 0xa9d4b0),
            run: tone(0x1f3a2b, 0x8fdbaa),
            wait: tone(0x22303f, 0xa3bfdf),
            warn: tone(0x3a3120, 0xe7c47e),
            fail: tone(0x3d2620, 0xf0a888),
            stop: tone(0x2c2b28, 0xb8b1a6),
            neutral: tone(0x252d29, 0xa5b2aa),
            report: tone(0x1f2b23, 0xb5cfb3),
        }
    }

    pub fn of(dark: bool) -> Self {
        if dark { Self::dark() } else { Self::light() }
    }

    /// Chip colors for a job/ticket state.
    pub fn state_tone(&self, state: &str) -> Tone {
        match state {
            "running" | "started" => self.run,
            "done" | "completed" | "success" | "reported" => self.ok,
            "queued" | "waiting" | "draining" => self.wait,
            "partial" => self.warn,
            "failed" | "error" => self.fail,
            "cancelled" | "interrupted" | "stopped" => self.stop,
            _ => self.neutral,
        }
    }

    /// Avatar color of an agent.
    pub fn agent_color(&self, agent: &str) -> Color32 {
        match (agent, self.dark) {
            ("codex", false) => hex(0x2f6f5e),
            ("codex", true) => hex(0x4f9c84),
            ("claude", false) => hex(0xc06a3e),
            ("claude", true) => hex(0xd88a5f),
            ("deepseek", false) => hex(0x3d63b8),
            ("deepseek", true) => hex(0x6b8fdc),
            ("antigravity", false) => hex(0x7a58b0),
            ("antigravity", true) => hex(0x9d80d0),
            (_, false) => hex(0x6f7d75),
            (_, true) => hex(0x7f8f86),
        }
    }
}

/// Install the custom look on the context.
pub fn apply(ctx: &egui::Context, pal: &Palette) {
    let mut style = (*ctx.style()).clone();
    let mut v = if pal.dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };

    v.override_text_color = Some(pal.ink);
    v.panel_fill = pal.canvas;
    v.window_fill = pal.surface;
    v.extreme_bg_color = pal.surface;
    v.faint_bg_color = pal.surface_alt;
    v.code_bg_color = pal.inline_code_bg;
    v.hyperlink_color = pal.link;
    v.window_stroke = Stroke::new(1.0_f32, pal.line);
    v.window_corner_radius = CornerRadius::same(RADIUS);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(if pal.dark { 90 } else { 28 }),
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 14,
        spread: 0,
        color: Color32::from_black_alpha(if pal.dark { 80 } else { 22 }),
    };
    v.selection.bg_fill = if pal.dark {
        Color32::from_rgba_unmultiplied(140, 196, 160, 70)
    } else {
        Color32::from_rgba_unmultiplied(53, 92, 71, 52)
    };
    v.selection.stroke = Stroke::new(1.0_f32, pal.accent);
    v.text_cursor.stroke = Stroke::new(1.5_f32, pal.accent);
    v.striped = false;
    v.slider_trailing_fill = true;
    v.handle_shape = egui::style::HandleShape::Circle;
    v.resize_corner_size = 10.0;
    v.clip_rect_margin = 3.0;
    v.button_frame = true;
    v.collapsing_header_frame = false;
    v.indent_has_left_vline = false;

    let r = CornerRadius::same(8);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = pal.surface;
    w.noninteractive.weak_bg_fill = pal.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, pal.line);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, pal.ink);
    w.noninteractive.corner_radius = r;

    w.inactive.bg_fill = pal.surface;
    w.inactive.weak_bg_fill = Color32::TRANSPARENT;
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.2_f32, pal.ink);
    w.inactive.corner_radius = r;
    w.inactive.expansion = 0.0;

    w.hovered.bg_fill = pal.hover;
    w.hovered.weak_bg_fill = pal.hover;
    w.hovered.bg_stroke = Stroke::new(1.0_f32, pal.line);
    w.hovered.fg_stroke = Stroke::new(1.4_f32, pal.ink);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;

    w.active.bg_fill = pal.selected;
    w.active.weak_bg_fill = pal.selected;
    w.active.bg_stroke = Stroke::new(1.0_f32, pal.accent);
    w.active.fg_stroke = Stroke::new(1.5_f32, pal.ink);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;

    w.open.bg_fill = pal.selected;
    w.open.weak_bg_fill = pal.selected;
    w.open.bg_stroke = Stroke::new(1.0_f32, pal.line);
    w.open.fg_stroke = Stroke::new(1.2_f32, pal.ink);
    w.open.corner_radius = r;

    style.visuals = v;
    style.text_styles = [
        (TextStyle::Small, body(SMALL)),
        (TextStyle::Body, body(BODY)),
        (TextStyle::Button, body(13.5)),
        (TextStyle::Heading, strong(19.0)),
        (TextStyle::Monospace, mono(MONO)),
    ]
    .into();
    let s = &mut style.spacing;
    s.item_spacing = Vec2::new(8.0, 6.0);
    s.button_padding = Vec2::new(10.0, 5.0);
    s.interact_size = Vec2::new(28.0, 26.0);
    s.window_margin = Margin::same(14);
    s.menu_margin = Margin::same(8);
    s.indent = 18.0;
    s.icon_width = 15.0;
    s.icon_spacing = 6.0;
    s.tooltip_width = 420.0;
    s.scroll = egui::style::ScrollStyle {
        bar_width: 8.0,
        floating_width: 3.0,
        floating_allocated_width: 0.0,
        bar_inner_margin: 3.0,
        bar_outer_margin: 2.0,
        ..egui::style::ScrollStyle::floating()
    };
    style.interaction.selectable_labels = true;
    style.interaction.tooltip_delay = 0.35;
    style.url_in_tooltip = false;
    ctx.set_style(style);
}

/// Load the system fonts. Missing files are skipped so the app still starts on
/// systems without them (glyphs then fall back to egui's bundled fonts).
pub fn install_fonts(ctx: &egui::Context) -> Vec<String> {
    let mut fonts = FontDefinitions::default();
    let mut loaded = Vec::new();
    let dir = std::env::var_os("WINDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "C:\\Windows".into())
        .join("Fonts");
    let mut load = |key: &str, file: &str, tweak: FontTweak| -> bool {
        match std::fs::read(dir.join(file)) {
            Ok(bytes) => {
                let mut data = FontData::from_owned(bytes);
                data.index = 0;
                data.tweak = tweak;
                fonts.font_data.insert(key.into(), Arc::new(data));
                loaded.push(file.to_string());
                true
            }
            Err(_) => false,
        }
    };
    let plain = FontTweak::default();
    // YaHei sits slightly high next to Segoe UI; nudge it down a touch.
    let cjk = FontTweak {
        y_offset_factor: 0.02,
        ..FontTweak::default()
    };
    let has_segoe = load("segoe", "segoeui.ttf", plain);
    let has_segoe_sb = load("segoe-sb", "seguisb.ttf", plain);
    let has_yahei = load("yahei", "msyh.ttc", cjk);
    let has_yahei_b = load("yahei-b", "msyhbd.ttc", cjk);
    let has_consolas = load("consolas", "consola.ttf", plain);
    fonts.font_data.insert(
        "phosphor".into(),
        Arc::new(egui_phosphor::Variant::Regular.font_data()),
    );

    let defaults_prop = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let defaults_mono = fonts
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();

    let mut prop = Vec::new();
    if has_segoe {
        prop.push("segoe".to_string());
    }
    prop.push("phosphor".into());
    if has_yahei {
        prop.push("yahei".into());
    }
    prop.extend(defaults_prop.iter().cloned());

    let mut bold = Vec::new();
    if has_segoe_sb {
        bold.push("segoe-sb".to_string());
    } else if has_segoe {
        bold.push("segoe".into());
    }
    bold.push("phosphor".into());
    if has_yahei_b {
        bold.push("yahei-b".into());
    } else if has_yahei {
        bold.push("yahei".into());
    }
    bold.extend(defaults_prop.iter().cloned());

    let mut mono = Vec::new();
    if has_consolas {
        mono.push("consolas".to_string());
    }
    mono.extend(defaults_mono.iter().cloned());
    mono.push("phosphor".into());
    if has_yahei {
        mono.push("yahei".into());
    }

    fonts.families.insert(FontFamily::Proportional, prop);
    fonts.families.insert(FontFamily::Monospace, mono);
    fonts.families.insert(semibold_family(), bold);
    ctx.set_fonts(fonts);
    loaded
}

// Ascent per point of the primary proportional and monospace fonts, measured
// at runtime so inline code can share the body baseline under a custom line height.
static ASCENT_BODY: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static ASCENT_MONO: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn measure_fonts(ctx: &egui::Context) {
    use std::sync::atomic::Ordering::Relaxed;
    for (font, slot) in [(body(100.0), &ASCENT_BODY), (mono(100.0), &ASCENT_MONO)] {
        let galley = ctx.fonts_mut(|f| f.layout_no_wrap("M".into(), font, Color32::WHITE));
        if let Some(g) = galley.rows.first().and_then(|r| r.glyphs.first()) {
            slot.store((g.pos.y / 100.0).to_bits(), Relaxed);
        }
    }
}

/// (body, mono) ascent per point; zero until measured.
pub fn ascents() -> (f32, f32) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        f32::from_bits(ASCENT_BODY.load(Relaxed)),
        f32::from_bits(ASCENT_MONO.load(Relaxed)),
    )
}
