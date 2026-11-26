use super::{Notice, NoticeKind};
use crate::{theme, widgets};
use egui::{
    Align, Align2, Area, Id, Label, Layout, Order, Rect, RichText, Sense, Stroke, UiBuilder, vec2,
};
use std::time::{Duration, Instant};

const LIFETIME: Duration = Duration::from_secs(8);
pub(super) const MAX_VISIBLE: usize = 3;
const HEIGHT: f32 = 62.0;
const UNNAMED: &str = "未命名会话";

pub(super) fn ticket_title(title: Option<&str>, goal: &str) -> String {
    let text = title
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(goal.trim());
    if text.is_empty() {
        UNNAMED.into()
    } else {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

struct Card {
    id: u64,
    notice: Notice,
    title: String,
    last_tick: Instant,
    remaining: Duration,
    hovered: bool,
}

impl Card {
    fn heading(&self) -> String {
        match self.notice.kind {
            NoticeKind::Question => format!("工单 #{} 等你回答", self.notice.ticket),
            _ => self.notice.title(),
        }
    }

    fn tick(&mut self, now: Instant, hovered: bool) -> bool {
        if !self.hovered {
            self.remaining = self
                .remaining
                .saturating_sub(now.saturating_duration_since(self.last_tick));
        }
        self.hovered = hovered;
        self.last_tick = now;
        !self.remaining.is_zero() || hovered
    }
}

#[derive(Default)]
pub(crate) struct Cards {
    next_id: u64,
    cards: Vec<Card>,
}

impl Cards {
    pub(crate) fn push(&mut self, notice: Notice, title: Option<String>, now: Instant) {
        self.remove_ticket(notice.ticket);
        self.next_id += 1;
        self.cards.insert(
            0,
            Card {
                id: self.next_id,
                notice,
                title: title.unwrap_or_else(|| UNNAMED.into()),
                last_tick: now,
                remaining: LIFETIME,
                hovered: false,
            },
        );
        self.cards.truncate(MAX_VISIBLE);
    }

    pub(crate) fn clear(&mut self) {
        self.cards.clear();
    }

    pub(crate) fn remove_ticket(&mut self, ticket: i64) {
        self.cards.retain(|card| card.notice.ticket != ticket);
    }

    pub(crate) fn show(
        &mut self,
        ctx: &egui::Context,
        conversation: Rect,
        pal: &theme::Palette,
    ) -> Option<i64> {
        if self.cards.is_empty() {
            return None;
        }
        let width = (conversation.width() - 24.0).clamp(1.0, 344.0);
        let now = Instant::now();
        let pointer = ctx.input(|input| input.pointer.hover_pos());
        let top_right = conversation.right_top() + vec2(-12.0, 10.0);
        let mut open = None;
        Area::new(Id::new("desktop-notification-cards"))
            .order(Order::Foreground)
            .pivot(Align2::RIGHT_TOP)
            .fixed_pos(top_right)
            .default_size(vec2(width, HEIGHT))
            .constrain(false)
            .fade_in(false)
            .sense(Sense::hover())
            .show(ctx, |ui| {
                ui.set_width(width);
                ui.spacing_mut().item_spacing.y = 6.0;
                self.cards.retain_mut(|card| {
                    let next = Rect::from_min_size(ui.next_widget_position(), vec2(width, HEIGHT));
                    let hovered = pointer.is_some_and(|p| next.contains(p));
                    if !card.tick(now, hovered) {
                        return false;
                    }
                    if !hovered {
                        ctx.request_repaint_after(card.remaining);
                    }
                    let (rect, _) = ui.allocate_exact_size(vec2(width, HEIGHT), Sense::hover());
                    let heading = card.heading();
                    let click = ui
                        .interact(rect, ui.id().with(card.id), Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    click.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            true,
                            format!("{heading} · {}", card.title),
                        )
                    });
                    ui.painter().rect(
                        rect,
                        theme::CONTROL_RADIUS,
                        if hovered {
                            pal.surface_alt
                        } else {
                            pal.surface
                        },
                        Stroke::new(1.0_f32, pal.line),
                        egui::StrokeKind::Inside,
                    );
                    let mut content = ui.new_child(
                        UiBuilder::new()
                            .id_salt(card.id)
                            .max_rect(rect.shrink2(vec2(10.0, 7.0)))
                            .layout(Layout::right_to_left(Align::Center)),
                    );
                    content.spacing_mut().item_spacing.x = 4.0;
                    let close = widgets::quiet_button(&mut content, "", "×", pal, false);
                    let button = widgets::outline_button(&mut content, "", "打开", pal);
                    content.with_layout(Layout::top_down(Align::Min), |ui| {
                        ui.add(
                            Label::new(
                                RichText::new(heading)
                                    .font(theme::strong(theme::UI))
                                    .color(pal.ink),
                            )
                            .truncate()
                            .show_tooltip_when_elided(false)
                            .selectable(false),
                        );
                        ui.add(
                            Label::new(
                                RichText::new(&card.title)
                                    .font(theme::body(theme::SMALL))
                                    .color(pal.mute),
                            )
                            .truncate()
                            .show_tooltip_when_elided(false)
                            .selectable(false),
                        );
                    });
                    if close.clicked() {
                        return false;
                    }
                    if button.clicked() || click.clicked() {
                        open = Some(card.notice.ticket);
                        return false;
                    }
                    true
                });
            });
        open
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_card_headings_match_the_terminal_states() {
        let mut cards = Cards::default();
        let now = Instant::now();
        for (state, expected) in [
            ("done", "工单 #42 已完成"),
            ("failed", "工单 #42 失败"),
            ("partial", "工单 #42 部分完成"),
            ("interrupted", "工单 #42 已中断"),
        ] {
            cards.push(
                Notice {
                    ticket: 42,
                    kind: NoticeKind::Completion,
                    status: crate::labels::job_state(state),
                },
                None,
                now,
            );
            assert_eq!(cards.cards[0].heading(), expected);
        }
    }

    #[test]
    fn card_lifetime_pauses_over_the_card_and_resumes_with_time_left() {
        let mut cards = Cards::default();
        let start = Instant::now();
        cards.push(
            Notice {
                ticket: 1,
                kind: NoticeKind::Approval,
                status: String::new(),
            },
            None,
            start,
        );
        let card = &mut cards.cards[0];
        assert!(card.tick(start + Duration::from_secs(6), true));
        assert!(card.tick(start + Duration::from_secs(20), true));
        assert!(card.tick(start + Duration::from_secs(21), false));
        assert!(card.tick(start + Duration::from_millis(22_990), false));
        assert!(!card.tick(start + Duration::from_millis(23_010), false));
    }
}
