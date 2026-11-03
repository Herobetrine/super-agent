//! Markdown for agent replies: parsed once with `pulldown-cmark`, rendered with
//! the app's own typography (semibold family for bold/headings, code cards,
//! bordered tables). Soft line breaks are kept as line breaks because agent
//! output, like the old `pre-wrap` view, relies on them.

use crate::theme::{self, Palette};
use egui::{
    Align, Color32, CornerRadius, FontId, Frame, Label, Layout, Margin, RichText, Sense, Stroke,
    Ui, Vec2,
    text::{LayoutJob, TextFormat},
};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpanStyle {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strike: bool,
    pub link: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub text: String,
    pub style: SpanStyle,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Para(Vec<Span>),
    Heading(u8, Vec<Span>),
    Code {
        lang: String,
        text: String,
    },
    Quote(Vec<Block>),
    List {
        start: Option<u64>,
        items: Vec<Vec<Block>>,
    },
    Table {
        head: Vec<Vec<Span>>,
        rows: Vec<Vec<Vec<Span>>>,
    },
    Rule,
}

enum Container {
    Root(Vec<Block>),
    Quote(Vec<Block>),
    List {
        start: Option<u64>,
        items: Vec<Vec<Block>>,
    },
    Item(Vec<Block>),
}

enum InlineKind {
    Para,
    Heading(u8),
}

#[derive(Default)]
struct TableBuild {
    head: Vec<Vec<Span>>,
    rows: Vec<Vec<Vec<Span>>>,
    row: Vec<Vec<Span>>,
    cell: Option<Vec<Span>>,
    in_head: bool,
}

struct Builder {
    stack: Vec<Container>,
    inline: Option<(InlineKind, Vec<Span>)>,
    style: SpanStyle,
    bold: u32,
    italic: u32,
    strike: u32,
    link: u32,
    code: Option<(String, String)>,
    table: Option<TableBuild>,
}

impl Builder {
    fn push_block(&mut self, b: Block) {
        match self.stack.last_mut() {
            Some(Container::Root(v)) | Some(Container::Quote(v)) | Some(Container::Item(v)) => {
                v.push(b)
            }
            Some(Container::List { items, .. }) => {
                // Content directly inside a list (should not happen): open an item.
                items.push(vec![b]);
            }
            None => {}
        }
    }

    fn flush(&mut self) {
        if let Some((kind, spans)) = self.inline.take() {
            let empty = spans.iter().all(|s| s.text.trim().is_empty());
            if !empty {
                let spans = trim_spans(spans);
                self.push_block(match kind {
                    InlineKind::Para => Block::Para(spans),
                    InlineKind::Heading(l) => Block::Heading(l, spans),
                });
            }
        }
    }

    fn refresh_style(&mut self) {
        self.style = SpanStyle {
            bold: self.bold > 0,
            italic: self.italic > 0,
            strike: self.strike > 0,
            link: self.link > 0,
            code: false,
        };
    }

    fn text(&mut self, text: &str, code: bool) {
        if let Some((_, buf)) = self.code.as_mut() {
            buf.push_str(text);
            return;
        }
        let mut style = self.style.clone();
        style.code = code;
        let span = Span {
            text: text.to_string(),
            style,
        };
        if let Some(t) = self.table.as_mut()
            && let Some(cell) = t.cell.as_mut()
        {
            cell.push(span);
            return;
        }
        if self.inline.is_none() {
            self.inline = Some((InlineKind::Para, Vec::new()));
        }
        if let Some((_, spans)) = self.inline.as_mut() {
            spans.push(span);
        }
    }
}

/// Remove leading/trailing line breaks inside a paragraph.
fn trim_spans(mut spans: Vec<Span>) -> Vec<Span> {
    while spans.first().is_some_and(|s| s.text == "\n") {
        spans.remove(0);
    }
    while spans.last().is_some_and(|s| s.text == "\n") {
        spans.pop();
    }
    spans
}

pub fn parse(src: &str) -> Vec<Block> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let mut b = Builder {
        stack: vec![Container::Root(Vec::new())],
        inline: None,
        style: SpanStyle::default(),
        bold: 0,
        italic: 0,
        strike: 0,
        link: 0,
        code: None,
        table: None,
    };
    for ev in Parser::new_ext(src, opts) {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    b.flush();
                    if b.table.is_none() {
                        b.inline = Some((InlineKind::Para, Vec::new()));
                    }
                }
                Tag::Heading { level, .. } => {
                    b.flush();
                    let l = match level {
                        HeadingLevel::H1 => 1,
                        HeadingLevel::H2 => 2,
                        HeadingLevel::H3 => 3,
                        _ => 4,
                    };
                    b.inline = Some((InlineKind::Heading(l), Vec::new()));
                }
                Tag::BlockQuote(_) => {
                    b.flush();
                    b.stack.push(Container::Quote(Vec::new()));
                }
                Tag::CodeBlock(kind) => {
                    b.flush();
                    let lang = match kind {
                        CodeBlockKind::Fenced(l) => {
                            l.split_whitespace().next().unwrap_or("").to_string()
                        }
                        CodeBlockKind::Indented => String::new(),
                    };
                    b.code = Some((lang, String::new()));
                }
                Tag::List(start) => {
                    b.flush();
                    b.stack.push(Container::List {
                        start,
                        items: Vec::new(),
                    });
                }
                Tag::Item => {
                    b.flush();
                    b.stack.push(Container::Item(Vec::new()));
                }
                Tag::Table(_) => {
                    b.flush();
                    b.table = Some(TableBuild::default());
                }
                Tag::TableHead => {
                    if let Some(t) = b.table.as_mut() {
                        t.in_head = true;
                    }
                }
                Tag::TableRow => {
                    if let Some(t) = b.table.as_mut() {
                        t.row.clear();
                    }
                }
                Tag::TableCell => {
                    if let Some(t) = b.table.as_mut() {
                        t.cell = Some(Vec::new());
                    }
                }
                Tag::Emphasis => {
                    b.italic += 1;
                    b.refresh_style();
                }
                Tag::Strong => {
                    b.bold += 1;
                    b.refresh_style();
                }
                Tag::Strikethrough => {
                    b.strike += 1;
                    b.refresh_style();
                }
                Tag::Link { .. } => {
                    b.link += 1;
                    b.refresh_style();
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph | TagEnd::Heading(_) => b.flush(),
                TagEnd::BlockQuote(_) => {
                    b.flush();
                    if let Some(Container::Quote(v)) = b.stack.pop() {
                        b.push_block(Block::Quote(v));
                    }
                }
                TagEnd::CodeBlock => {
                    if let Some((lang, mut text)) = b.code.take() {
                        if text.ends_with('\n') {
                            text.pop();
                        }
                        b.push_block(Block::Code { lang, text });
                    }
                }
                TagEnd::List(_) => {
                    b.flush();
                    if let Some(Container::List { start, items }) = b.stack.pop() {
                        b.push_block(Block::List { start, items });
                    }
                }
                TagEnd::Item => {
                    b.flush();
                    if let Some(Container::Item(v)) = b.stack.pop()
                        && let Some(Container::List { items, .. }) = b.stack.last_mut()
                    {
                        items.push(v);
                    }
                }
                TagEnd::TableCell => {
                    if let Some(t) = b.table.as_mut() {
                        let cell = trim_spans(t.cell.take().unwrap_or_default());
                        t.row.push(cell);
                    }
                }
                TagEnd::TableHead => {
                    if let Some(t) = b.table.as_mut() {
                        t.head = std::mem::take(&mut t.row);
                        t.in_head = false;
                    }
                }
                TagEnd::TableRow => {
                    if let Some(t) = b.table.as_mut()
                        && !t.in_head
                    {
                        let row = std::mem::take(&mut t.row);
                        t.rows.push(row);
                    }
                }
                TagEnd::Table => {
                    if let Some(t) = b.table.take() {
                        b.push_block(Block::Table {
                            head: t.head,
                            rows: t.rows,
                        });
                    }
                }
                TagEnd::Emphasis => {
                    b.italic = b.italic.saturating_sub(1);
                    b.refresh_style();
                }
                TagEnd::Strong => {
                    b.bold = b.bold.saturating_sub(1);
                    b.refresh_style();
                }
                TagEnd::Strikethrough => {
                    b.strike = b.strike.saturating_sub(1);
                    b.refresh_style();
                }
                TagEnd::Link => {
                    b.link = b.link.saturating_sub(1);
                    b.refresh_style();
                }
                _ => {}
            },
            Event::Text(t) => b.text(&t, false),
            Event::Code(t) => b.text(&t, true),
            Event::InlineMath(t) | Event::DisplayMath(t) => b.text(&t, true),
            Event::SoftBreak | Event::HardBreak => b.text("\n", false),
            Event::Html(t) | Event::InlineHtml(t) => b.text(&t, false),
            Event::TaskListMarker(done) => b.text(if done { "☑ " } else { "☐ " }, false),
            Event::Rule => {
                b.flush();
                b.push_block(Block::Rule);
            }
            Event::FootnoteReference(t) => b.text(&format!("[{t}]"), false),
        }
    }
    b.flush();
    // Close anything left open by malformed input.
    while b.stack.len() > 1 {
        match b.stack.pop() {
            Some(Container::Quote(v)) | Some(Container::Item(v)) => {
                for blk in v {
                    b.push_block(blk);
                }
            }
            Some(Container::List { start, items }) => b.push_block(Block::List { start, items }),
            _ => {}
        }
    }
    match b.stack.pop() {
        Some(Container::Root(v)) => v,
        _ => Vec::new(),
    }
}

/// Rendering parameters.
#[derive(Clone)]
pub struct Look<'a> {
    pub pal: &'a Palette,
    pub size: f32,
    pub color: Color32,
}

pub fn spans_job(spans: &[Span], look: &Look, bold_all: bool, size: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    for s in spans {
        let font: FontId = if s.style.code {
            theme::mono(size - 1.5)
        } else if s.style.bold || bold_all {
            theme::strong(size)
        } else {
            theme::body(size)
        };
        let mut fmt = TextFormat {
            font_id: font,
            color: if s.style.link {
                look.pal.link
            } else {
                look.color
            },
            italics: s.style.italic,
            line_height: Some((size * 1.62).round()),
            valign: Align::Center,
            ..Default::default()
        };
        if s.style.strike {
            fmt.strikethrough = Stroke::new(1.0_f32, look.color);
        }
        if s.style.link {
            fmt.underline = Stroke::new(1.0_f32, look.pal.link.gamma_multiply(0.6));
        }
        if s.style.code {
            // A row-tall background box looks heavy next to CJK text; tint the
            // monospace glyphs instead and pad them with thin spaces.
            fmt.color = look.pal.inline_code_fg;
            // Glyphs sit at `ascent` from the top of their line box; shrink the
            // code line box so its baseline lands on the body baseline.
            let (ab, am) = theme::ascents();
            let line = (size * 1.62).round();
            fmt.line_height = Some(line - (ab * size - am * (size - 1.5)).max(0.0));
            fmt.valign = Align::BOTTOM;
            job.append(&format!("\u{2009}{}\u{2009}", s.text), 0.0, fmt);
        } else {
            job.append(&s.text, 0.0, fmt);
        }
    }
    job
}

/// Plain text paragraph with the body line height.
pub fn plain_job(text: &str, look: &Look) -> LayoutJob {
    spans_job(
        &[Span {
            text: text.into(),
            style: SpanStyle::default(),
        }],
        look,
        false,
        look.size,
    )
}

pub fn show(ui: &mut Ui, blocks: &[Block], look: &Look, salt: u64) {
    ui.spacing_mut().item_spacing.y = 0.0;
    for (i, block) in blocks.iter().enumerate() {
        if i > 0 {
            ui.add_space(match block {
                Block::Heading(..) => 12.0,
                _ => 8.0,
            });
        }
        show_block(
            ui,
            block,
            look,
            salt.wrapping_mul(31).wrapping_add(i as u64),
        );
    }
}

fn show_block(ui: &mut Ui, block: &Block, look: &Look, salt: u64) {
    let pal = look.pal;
    match block {
        Block::Para(spans) => {
            ui.add(Label::new(spans_job(spans, look, false, look.size)).wrap());
        }
        Block::Heading(level, spans) => {
            let size = match level {
                1 => look.size + 5.0,
                2 => look.size + 3.0,
                3 => look.size + 1.5,
                _ => look.size,
            };
            ui.add(Label::new(spans_job(spans, look, true, size)).wrap());
        }
        Block::Code { lang, text } => code_card(ui, lang, text, pal, salt),
        Block::Quote(inner) => {
            let inner_look = Look {
                color: pal.mute,
                ..look.clone()
            };
            let resp = Frame::new()
                .inner_margin(Margin {
                    left: 14,
                    right: 0,
                    top: 2,
                    bottom: 2,
                })
                .show(ui, |ui| {
                    show(ui, inner, &inner_look, salt);
                });
            let r = resp.response.rect;
            ui.painter().rect_filled(
                egui::Rect::from_min_size(r.min, Vec2::new(3.0, r.height())),
                CornerRadius::same(2),
                pal.line,
            );
        }
        Block::List { start, items } => {
            let row_h = (look.size * 1.62).round();
            for (n, item) in items.iter().enumerate() {
                if n > 0 {
                    ui.add_space(4.0);
                }
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let marker_w = if start.is_some() { 26.0 } else { 20.0 };
                    let (rect, _) =
                        ui.allocate_exact_size(Vec2::new(marker_w, row_h), Sense::hover());
                    match start {
                        Some(s) => {
                            ui.painter().text(
                                egui::pos2(rect.right() - 7.0, rect.center().y),
                                egui::Align2::RIGHT_CENTER,
                                format!("{}.", s + n as u64),
                                theme::body(look.size - 0.5),
                                pal.mute,
                            );
                        }
                        None => {
                            ui.painter().circle_filled(
                                egui::pos2(rect.left() + 8.0, rect.center().y),
                                2.6,
                                pal.mute,
                            );
                        }
                    }
                    ui.vertical(|ui| show(ui, item, look, salt.wrapping_add(n as u64 * 7919)));
                });
            }
        }
        Block::Table { head, rows } => table(ui, head, rows, look, salt),
        Block::Rule => {
            ui.add_space(4.0);
            let w = ui.available_width();
            let (r, _) = ui.allocate_exact_size(Vec2::new(w, 1.0), Sense::hover());
            ui.painter().rect_filled(r, 0.0, pal.line);
            ui.add_space(4.0);
        }
    }
}

fn code_card(ui: &mut Ui, lang: &str, text: &str, pal: &Palette, salt: u64) {
    Frame::new()
        .fill(pal.code_bg)
        .stroke(Stroke::new(1.0_f32, pal.line_soft))
        .corner_radius(CornerRadius::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            Frame::new()
                .inner_margin(Margin {
                    left: 14,
                    right: 6,
                    top: 5,
                    bottom: 4,
                })
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let label = if lang.is_empty() { "代码" } else { lang };
                        ui.label(
                            RichText::new(label)
                                .font(theme::body(theme::TINY))
                                .color(pal.mute),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let id = ui.id().with(("copied", salt));
                            let copied_at: Option<f64> = ui.data(|d| d.get_temp(id));
                            let now = ui.input(|i| i.time);
                            let recent = copied_at.is_some_and(|t| now - t < 1.5);
                            let caption = if recent {
                                format!("{} 已复制", egui_phosphor::regular::CHECK)
                            } else {
                                format!("{} 复制代码", egui_phosphor::regular::COPY)
                            };
                            let b = ui.add(
                                egui::Button::new(
                                    RichText::new(caption)
                                        .font(theme::body(theme::TINY))
                                        .color(pal.mute),
                                )
                                .frame(false)
                                .min_size(Vec2::new(0.0, 22.0)),
                            );
                            if b.clicked() {
                                ui.ctx().copy_text(text.to_string());
                                ui.data_mut(|d| d.insert_temp(id, now));
                            }
                            if recent {
                                ui.ctx()
                                    .request_repaint_after(std::time::Duration::from_millis(300));
                            }
                        });
                    });
                });
            let (r, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
            ui.painter().rect_filled(r, 0.0, pal.line_soft);
            Frame::new()
                .inner_margin(Margin {
                    left: 14,
                    right: 14,
                    top: 10,
                    bottom: 12,
                })
                .show(ui, |ui| {
                    egui::ScrollArea::horizontal()
                        .id_salt(("code", salt))
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            let mut job = LayoutJob::default();
                            job.append(
                                text,
                                0.0,
                                TextFormat {
                                    font_id: theme::mono(theme::MONO),
                                    color: pal.ink,
                                    line_height: Some(20.0),
                                    valign: Align::Center,
                                    ..Default::default()
                                },
                            );
                            ui.add(Label::new(job).extend());
                        });
                });
        });
}

fn table(ui: &mut Ui, head: &[Vec<Span>], rows: &[Vec<Vec<Span>>], look: &Look, salt: u64) {
    let pal = look.pal;
    let size = look.size - 1.0;
    Frame::new()
        .stroke(Stroke::new(1.0_f32, pal.line))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            egui::ScrollArea::horizontal()
                .id_salt(("table", salt))
                .auto_shrink([true, true])
                .show(ui, |ui| {
                    egui::Grid::new(("md-table", salt))
                        .spacing([28.0, 8.0])
                        .show(ui, |ui| {
                            for cell in head {
                                let mut job = spans_job(
                                    cell,
                                    &Look {
                                        color: pal.mute,
                                        ..look.clone()
                                    },
                                    true,
                                    size,
                                );
                                job.wrap.max_width = f32::INFINITY;
                                ui.add(Label::new(job).extend());
                            }
                            ui.end_row();
                            for row in rows {
                                for cell in row {
                                    let job = spans_job(cell, look, false, size);
                                    ui.add(Label::new(job).extend());
                                }
                                ui.end_row();
                            }
                        });
                });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn parses_common_blocks() {
        let doc = parse(
            "# 标题\n\n第一行\n第二行 **粗体** `code`\n\n- a\n- b\n\n```rust\nfn x() {}\n```\n\n| A | B |\n|---|---|\n| 1 | 2 |\n",
        );
        assert!(matches!(&doc[0], Block::Heading(1, s) if text_of(s) == "标题"));
        match &doc[1] {
            Block::Para(s) => {
                assert_eq!(text_of(s), "第一行\n第二行 粗体 code");
                assert!(s.iter().any(|x| x.style.bold && x.text == "粗体"));
                assert!(s.iter().any(|x| x.style.code && x.text == "code"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(&doc[2], Block::List { start: None, items } if items.len() == 2));
        assert!(
            matches!(&doc[3], Block::Code { lang, text } if lang == "rust" && text == "fn x() {}")
        );
        assert!(
            matches!(&doc[4], Block::Table { head, rows } if head.len() == 2 && rows.len() == 1)
        );
    }

    #[test]
    fn survives_unclosed_fence_and_nesting() {
        let doc = parse("> quote\n> - item\n\n```\nopen fence");
        assert!(matches!(&doc[0], Block::Quote(_)));
        assert!(matches!(doc.last(), Some(Block::Code { .. })));
    }
}
