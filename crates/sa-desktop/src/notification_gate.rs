//! Desktop routing and the journal snapshot it consumes.

use super::{
    CompletionRow, Notice, NoticeKind, completion_head, open_journal, read_completion_rows,
};
use crate::ops::Pending;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

const GRACE: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Mode {
    Off,
    #[default]
    Visual,
    Sound,
    Both,
}

impl Mode {
    pub(crate) const ALL: [Self; 4] = [Self::Off, Self::Visual, Self::Sound, Self::Both];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Off => "关闭",
            Self::Visual => "仅通知",
            Self::Sound => "仅声音",
            Self::Both => "通知和声音",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Off => "0",
            Self::Visual => "1",
            Self::Sound => "sound",
            Self::Both => "both",
        }
    }

    fn from_key(key: Option<String>) -> Self {
        match key.as_deref() {
            Some("0") => Self::Off,
            Some("sound") => Self::Sound,
            Some("both") => Self::Both,
            _ => Self::Visual,
        }
    }

    pub(crate) fn visual(self) -> bool {
        matches!(self, Self::Visual | Self::Both)
    }

    fn sound(self) -> bool {
        matches!(self, Self::Sound | Self::Both)
    }
}

#[derive(Default)]
pub(crate) struct Preferences {
    mode: Mode,
    last_on: Mode,
}

impl Preferences {
    pub(crate) fn load(get: impl Fn(&str) -> Option<String>) -> Self {
        let mut prefs = Self {
            mode: Mode::Off,
            ..Self::default()
        };
        prefs.set(Mode::from_key(get("notifications_last_on")));
        prefs.set(Mode::from_key(get("notifications")));
        prefs
    }

    pub(crate) fn save(&self, storage: &mut dyn eframe::Storage) {
        storage.set_string("notifications", self.mode.key().into());
        storage.set_string("notifications_last_on", self.last_on.key().into());
    }

    pub(crate) fn mode(&self) -> Mode {
        self.mode
    }

    pub(crate) fn set(&mut self, mode: Mode) {
        self.mode = mode;
        if mode != Mode::Off {
            self.last_on = mode;
        }
    }

    pub(crate) fn toggled(&self) -> Mode {
        if self.mode == Mode::Off {
            self.last_on
        } else {
            Mode::Off
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Window {
    pub main: bool,
    pub focused: bool,
    pub ticket: Option<i64>,
    pub mode: Mode,
}

impl Window {
    fn viewed(self) -> Option<i64> {
        self.ticket.filter(|_| self.main && self.focused)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Visual {
    None,
    Toast,
    Card,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Delivery {
    pub notice: Notice,
    pub visual: Visual,
    pub sound: bool,
    pub title: Option<String>,
}

fn route(notice: Notice, window: Window) -> Option<Delivery> {
    if !window.main || window.viewed() == Some(notice.ticket) || window.mode == Mode::Off {
        return None;
    }
    let visual = if !window.mode.visual() {
        Visual::None
    } else if window.focused {
        Visual::Card
    } else {
        Visual::Toast
    };
    Some(Delivery {
        notice,
        visual,
        sound: window.mode.sound(),
        title: None,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ResultKey {
    ticket: i64,
    job: i64,
    turn: i64,
    state: String,
}

struct DueResult {
    key: ResultKey,
    deadline: Instant,
}

#[derive(Default)]
struct Snapshot {
    head: i64,
    approvals: Vec<(i64, i64)>,
    questions: Vec<(i64, i64)>,
    results: Vec<CompletionRow>,
    /// Grace periods checked against a journal transaction started after the deadline.
    checked: Vec<(ResultKey, bool)>,
    titles: HashMap<i64, String>,
}

#[derive(Default)]
pub(crate) struct Gate {
    cursor: Option<i64>,
    approvals: HashSet<i64>,
    questions: HashSet<i64>,
    needs_you: HashSet<i64>,
    unread: HashSet<i64>,
    last_result: HashMap<i64, ResultKey>,
    waiting: BTreeMap<i64, DueResult>,
}

impl Gate {
    #[cfg(test)]
    pub(crate) fn has_baseline(&self) -> bool {
        self.cursor.is_some()
    }

    pub(crate) fn view(&mut self, window: Window) {
        if let Some(ticket) = window.viewed() {
            self.unread.remove(&ticket);
            self.waiting.remove(&ticket);
        }
    }

    pub(crate) fn unread(&self, ticket: i64) -> bool {
        self.unread.contains(&ticket)
    }

    pub(crate) fn count(&self) -> usize {
        self.needs_you.union(&self.unread).count()
    }

    fn due(&self, now: Instant) -> Vec<ResultKey> {
        self.waiting
            .values()
            .filter(|result| now >= result.deadline)
            .map(|result| result.key.clone())
            .collect()
    }

    fn next_poll(&self, last: Instant) -> Instant {
        self.waiting
            .values()
            .map(|result| result.deadline)
            .filter(|&deadline| deadline > last)
            .fold(last + POLL_INTERVAL, std::cmp::min)
    }

    fn ingest(&mut self, snapshot: Snapshot, now: Instant, window: Window) -> Vec<Delivery> {
        if !window.main {
            return Vec::new();
        }
        self.view(window);
        let mut deliveries = Vec::new();
        let baseline = self.cursor.is_none_or(|cursor| snapshot.head < cursor);
        self.needs_you = snapshot
            .approvals
            .iter()
            .chain(&snapshot.questions)
            .map(|&(_, ticket)| ticket)
            .collect();
        for (kind, rows, seen) in [
            (
                NoticeKind::Approval,
                snapshot.approvals,
                &mut self.approvals,
            ),
            (
                NoticeKind::Question,
                snapshot.questions,
                &mut self.questions,
            ),
        ] {
            if !baseline {
                let tickets: BTreeSet<_> = rows
                    .iter()
                    .filter(|(id, _)| !seen.contains(id))
                    .map(|&(_, ticket)| ticket)
                    .collect();
                deliveries.extend(tickets.into_iter().filter_map(|ticket| {
                    route(
                        Notice {
                            ticket,
                            kind,
                            status: String::new(),
                        },
                        window,
                    )
                }));
            }
            *seen = rows.into_iter().map(|(id, _)| id).collect();
        }
        if baseline {
            self.last_result.clear();
            self.waiting.clear();
        } else {
            for row in snapshot.results {
                self.record_result(row, now, window);
            }
            for (key, valid) in snapshot.checked {
                if self
                    .waiting
                    .get(&key.ticket)
                    .is_some_and(|result| result.key == key && now >= result.deadline)
                {
                    self.waiting.remove(&key.ticket);
                    if valid {
                        deliveries.extend(route(
                            Notice {
                                ticket: key.ticket,
                                kind: NoticeKind::Completion,
                                status: crate::labels::job_state(&key.state),
                            },
                            window,
                        ));
                    }
                }
            }
        }
        self.cursor = Some(snapshot.head);
        for delivery in &mut deliveries {
            if delivery.visual == Visual::Card {
                delivery.title = snapshot.titles.get(&delivery.notice.ticket).cloned();
            }
        }
        deliveries
    }

    fn record_result(&mut self, row: CompletionRow, now: Instant, window: Window) {
        if self.cursor.is_some_and(|cursor| row.seq <= cursor) {
            return;
        }
        let key = ResultKey {
            ticket: row.ticket,
            job: row.job,
            turn: row.turn,
            state: row.state,
        };
        if self.last_result.get(&key.ticket) == Some(&key) {
            return;
        }
        self.last_result.insert(key.ticket, key.clone());
        if window.viewed() != Some(key.ticket) {
            self.unread.insert(key.ticket);
            self.waiting.insert(
                key.ticket,
                DueResult {
                    key,
                    deadline: now + GRACE,
                },
            );
        }
    }
}

#[derive(Default)]
pub(crate) struct Watch {
    last: Option<Instant>,
    pending: Option<Pending<Snapshot>>,
}

impl Watch {
    pub(crate) fn poll(
        &mut self,
        ctx: &egui::Context,
        data: &Path,
        gate: &mut Gate,
        window: Window,
    ) -> Vec<Delivery> {
        let now = Instant::now();
        let deliveries = if let Some(result) = self.pending.as_ref().and_then(Pending::poll) {
            self.pending = None;
            result.map_or_else(
                |_| Vec::new(),
                |snapshot| gate.ingest(snapshot, now, window),
            )
        } else {
            Vec::new()
        };
        if self.pending.is_none() {
            let next = self.last.map_or(now, |last| gate.next_poll(last));
            if now >= next {
                let path = data.join(sa_store::DB_FILE);
                let cursor = gate.cursor;
                let due = gate.due(now);
                self.pending = Some(Pending::spawn(ctx, move || {
                    read_snapshot(&path, cursor, due)
                }));
                self.last = Some(now);
            } else {
                ctx.request_repaint_after(next - now);
            }
        }
        deliveries
    }
}

fn read_snapshot(
    path: &Path,
    cursor: Option<i64>,
    due: Vec<ResultKey>,
) -> anyhow::Result<Snapshot> {
    let mut conn = open_journal(path)?;
    let tx = conn.transaction()?;
    let head = completion_head(&tx)?;
    let approvals = super::read_pending(&tx, NoticeKind::Approval)?;
    let questions = super::read_pending(&tx, NoticeKind::Question)?;
    let results = read_completion_rows(&tx, cursor, head)?;
    let mut checked = Vec::with_capacity(due.len());
    if !due.is_empty() {
        let mut statement = tx.prepare(
            "SELECT 1 FROM jobs WHERE id=?1 AND ticket=?2 AND turn=?3 AND state=?4 AND parent IS NULL",
        )?;
        for key in due {
            let valid =
                statement.exists(rusqlite::params![key.job, key.ticket, key.turn, key.state])?;
            checked.push((key, valid));
        }
    }
    let tickets: BTreeSet<_> = approvals
        .iter()
        .chain(&questions)
        .map(|&(_, ticket)| ticket)
        .chain(
            checked
                .iter()
                .filter(|(_, valid)| *valid)
                .map(|(key, _)| key.ticket),
        )
        .collect();
    let titles = read_titles(&tx, tickets)?;
    Ok(Snapshot {
        head,
        approvals,
        questions,
        results,
        checked,
        titles,
    })
}

fn read_titles(
    connection: &rusqlite::Connection,
    tickets: BTreeSet<i64>,
) -> rusqlite::Result<HashMap<i64, String>> {
    use rusqlite::OptionalExtension as _;
    let mut titles = HashMap::new();
    if tickets.is_empty() {
        return Ok(titles);
    }
    let sql = if super::table_exists(connection, "studio_ticket_meta")? {
        "SELECT m.title,t.goal FROM tickets t
         LEFT JOIN studio_ticket_meta m ON m.ticket=t.id WHERE t.id=?1"
    } else {
        "SELECT NULL,goal FROM tickets WHERE id=?1"
    };
    let mut statement = connection.prepare(sql)?;
    for ticket in tickets {
        let title = statement
            .query_row([ticket], |row| {
                let title: Option<String> = row.get(0)?;
                let goal: String = row.get(1)?;
                Ok(super::cards::ticket_title(title.as_deref(), &goal))
            })
            .optional()?;
        if let Some(title) = title {
            titles.insert(ticket, title);
        }
    }
    Ok(titles)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn window(focused: bool, ticket: Option<i64>, mode: Mode) -> Window {
        Window {
            main: true,
            focused,
            ticket,
            mode,
        }
    }

    fn result(seq: i64, ticket: i64, turn: i64, state: &str) -> CompletionRow {
        CompletionRow {
            seq,
            ticket,
            job: ticket,
            turn,
            state: state.into(),
        }
    }

    #[test]
    fn startup_baseline_counts_pending_tickets_without_announcements_or_unread() {
        let now = Instant::now();
        let mut gate = Gate::default();
        let deliveries = gate.ingest(
            Snapshot {
                head: 30,
                approvals: vec![(1, 8), (2, 8)],
                questions: vec![(1, 8), (2, 9)],
                results: vec![result(30, 3, 1, "done")],
                ..Snapshot::default()
            },
            now,
            window(false, None, Mode::Both),
        );
        assert!(deliveries.is_empty());
        assert!(!gate.unread(3));
        assert!(gate.due(now + GRACE).is_empty());
        assert_eq!(gate.count(), 2);
    }

    #[test]
    fn routing_respects_focus_selection_and_all_four_modes() {
        for (mode, visual, sound) in [
            (Mode::Off, false, false),
            (Mode::Visual, true, false),
            (Mode::Sound, false, true),
            (Mode::Both, true, true),
        ] {
            for (focused, selected, expected) in [
                (false, Some(8), Visual::Toast),
                (true, Some(9), Visual::Card),
                (true, Some(8), Visual::None),
            ] {
                let notice = Notice {
                    ticket: 8,
                    kind: NoticeKind::Approval,
                    status: String::new(),
                };
                let delivery = route(notice.clone(), window(focused, selected, mode));
                if expected == Visual::None || mode == Mode::Off {
                    assert!(delivery.is_none());
                } else {
                    let delivery = delivery.unwrap();
                    assert_eq!(delivery.notice, notice);
                    assert_eq!(
                        delivery.visual,
                        if visual { expected } else { Visual::None }
                    );
                    assert_eq!(delivery.sound, sound);
                }
            }
        }
    }

    #[test]
    fn observer_does_not_initialize_the_gate_or_receive_notifications() {
        let mut gate = Gate::default();
        let observer = Window {
            main: false,
            ..window(false, None, Mode::Both)
        };
        assert!(
            gate.ingest(
                Snapshot {
                    head: 1,
                    approvals: vec![(1, 4)],
                    results: vec![result(1, 5, 1, "failed")],
                    ..Snapshot::default()
                },
                Instant::now(),
                observer
            )
            .is_empty()
        );
        assert!(!gate.has_baseline());
        assert_eq!(gate.count(), 0);
        assert!(
            route(
                Notice {
                    ticket: 8,
                    kind: NoticeKind::Approval,
                    status: String::new()
                },
                observer
            )
            .is_none()
        );
    }

    #[test]
    fn repeated_pending_ids_and_result_rows_do_not_notify_twice() {
        let now = Instant::now();
        let mut gate = Gate::default();
        let view = window(false, None, Mode::Both);
        gate.ingest(Snapshot::default(), now, view);
        let approvals = vec![(1, 8), (2, 8)];
        let questions = vec![(1, 8)];
        let first = gate.ingest(
            Snapshot {
                head: 2,
                approvals: approvals.clone(),
                questions: questions.clone(),
                results: vec![result(1, 8, 1, "failed"), result(2, 8, 1, "failed")],
                ..Snapshot::default()
            },
            now,
            view,
        );
        assert_eq!(
            first.into_iter().map(|d| d.notice.kind).collect::<Vec<_>>(),
            [NoticeKind::Approval, NoticeKind::Question]
        );
        let checked = gate
            .due(now + GRACE)
            .into_iter()
            .map(|key| (key, true))
            .collect();
        let second = gate.ingest(
            Snapshot {
                head: 3,
                approvals,
                questions,
                results: vec![result(2, 8, 1, "failed"), result(3, 8, 1, "failed")],
                checked,
                ..Snapshot::default()
            },
            now + GRACE,
            view,
        );
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].notice.title(), "工单 #8 失败");
        assert!(gate.due(now + GRACE * 2).is_empty());
        assert_eq!(
            gate.count(),
            1,
            "pending approvals, question and unread result share a ticket"
        );
        assert_eq!(
            gate.ingest(
                Snapshot {
                    head: 4,
                    approvals: vec![(3, 8)],
                    results: vec![result(4, 8, 2, "failed")],
                    ..Snapshot::default()
                },
                now + GRACE,
                view
            )
            .len(),
            1,
            "a later approval can alert again"
        );
        assert_eq!(
            gate.due(now + GRACE * 2).len(),
            1,
            "a later turn can alert again"
        );
    }

    #[test]
    fn grace_requires_five_seconds_and_a_fresh_matching_result() {
        let now = Instant::now();
        let view = window(false, Some(9), Mode::Both);
        let mut gate = Gate::default();
        gate.ingest(Snapshot::default(), now, view);
        let batch = Snapshot {
            head: 2,
            results: vec![result(1, 8, 1, "done"), result(2, 9, 1, "failed")],
            ..Snapshot::default()
        };
        assert!(gate.ingest(batch, now, view).is_empty());
        assert!(gate.due(now + GRACE - Duration::from_millis(1)).is_empty());
        let data = tempfile::tempdir().unwrap();
        let path = data.path().join(sa_store::DB_FILE);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE events(seq INTEGER PRIMARY KEY);
             CREATE TABLE jobs(id INTEGER PRIMARY KEY, ticket INTEGER, turn INTEGER, state TEXT, parent INTEGER);
             CREATE TABLE tickets(id INTEGER PRIMARY KEY, goal TEXT);
             CREATE TABLE approvals(id INTEGER PRIMARY KEY, ticket INTEGER, decision TEXT);
             INSERT INTO events VALUES (2);
             INSERT INTO jobs VALUES (8,8,1,'done',NULL),(9,9,2,'failed',NULL);
             INSERT INTO tickets VALUES (8,'目标');",
        ).unwrap();
        let snapshot = read_snapshot(&path, gate.cursor, gate.due(now + GRACE)).unwrap();
        let delivered = gate.ingest(snapshot, now + GRACE, window(true, None, Mode::Both));
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].notice.ticket, 8);
        assert_eq!(delivered[0].visual, Visual::Card);
        assert_eq!(delivered[0].title.as_deref(), Some("目标"));
        assert!(gate.due(now + GRACE * 2).is_empty());
    }

    #[test]
    fn opening_a_ticket_cancels_grace_even_after_leaving_it() {
        let now = Instant::now();
        let mut gate = Gate::default();
        let other = window(true, Some(9), Mode::Both);
        gate.ingest(Snapshot::default(), now, other);
        gate.ingest(
            Snapshot {
                head: 1,
                results: vec![result(1, 8, 1, "partial")],
                ..Snapshot::default()
            },
            now,
            other,
        );
        assert!(gate.unread(8));
        gate.view(window(true, Some(8), Mode::Both));
        gate.view(other);
        assert!(!gate.unread(8));
        assert!(gate.due(now + GRACE).is_empty());
    }

    #[test]
    fn unread_counts_in_off_mode_including_selected_tickets_while_unfocused() {
        let now = Instant::now();
        let background = window(false, Some(8), Mode::Off);
        let mut gate = Gate::default();
        let approvals = vec![(1, 8), (2, 8), (3, 7)];
        gate.ingest(
            Snapshot {
                approvals: approvals.clone(),
                ..Snapshot::default()
            },
            now,
            background,
        );
        assert!(
            gate.ingest(
                Snapshot {
                    head: 2,
                    approvals: approvals.clone(),
                    questions: vec![(1, 8)],
                    results: vec![result(1, 8, 1, "interrupted"), result(2, 9, 1, "done")],
                    ..Snapshot::default()
                },
                now,
                background
            )
            .is_empty()
        );
        assert!(gate.unread(8));
        assert_eq!(gate.count(), 3);
        gate.view(window(true, Some(8), Mode::Off));
        assert!(!gate.unread(8));
        assert_eq!(
            gate.count(),
            3,
            "the pending approval/question still counts"
        );
        gate.view(window(true, Some(9), Mode::Off));
        assert!(!gate.unread(9));
        assert_eq!(gate.count(), 2);
        gate.ingest(
            Snapshot {
                head: 2,
                ..Snapshot::default()
            },
            now,
            background,
        );
        assert_eq!(gate.count(), 0, "resolved requests leave the badge count");
    }

    #[test]
    fn preferences_load_legacy_values_and_restore_the_persisted_last_mode() {
        #[derive(Default)]
        struct Storage(HashMap<String, String>);
        impl eframe::Storage for Storage {
            fn get_string(&self, key: &str) -> Option<String> {
                self.0.get(key).cloned()
            }
            fn set_string(&mut self, key: &str, value: String) {
                self.0.insert(key.into(), value);
            }
            fn flush(&mut self) {}
        }
        use eframe::Storage as _;
        let mut storage = Storage::default();
        assert_eq!(
            Preferences::load(|key| storage.get_string(key)).mode(),
            Mode::Visual
        );
        storage.set_string("notifications", "0".into());
        let mut prefs = Preferences::load(|key| storage.get_string(key));
        assert_eq!(prefs.mode(), Mode::Off);
        assert_eq!(prefs.toggled(), Mode::Visual);
        prefs.set(Mode::Sound);
        prefs.set(prefs.toggled());
        prefs.save(&mut storage);
        let mut reopened = Preferences::load(|key| storage.get_string(key));
        assert_eq!(reopened.mode(), Mode::Off);
        reopened.set(reopened.toggled());
        assert_eq!(reopened.mode(), Mode::Sound);
        reopened.set(Mode::Both);
        reopened.save(&mut storage);
        assert_eq!(
            Preferences::load(|key| storage.get_string(key)).mode(),
            Mode::Both
        );
    }
}
