//! Background reader. Owns the data source and forwards changes to the UI
//! thread over a channel. The UI never touches SQLite.
//!
//! The reader watches the journal's change counter many times a second and
//! loads a snapshot only when another connection has committed, so streamed
//! output and collaborator state reach the window within a few tens of
//! milliseconds instead of on the next one-second poll. Loads are spaced out
//! so a burst of deltas costs a bounded number of snapshots.

use crate::data::{DataSource, Opener};
use sa_store::{JobsPage, Snapshot, TicketSummary};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// Unconditional reload: sources without a change counter, clock-driven
/// state, and a safety net should a change ever go unnoticed.
const POLL: Duration = Duration::from_millis(1000);
/// How often the change counter is checked.
const WATCH: Duration = Duration::from_millis(40);
/// Minimum spacing between two change-driven loads.
const MIN_GAP: Duration = Duration::from_millis(70);
/// The session list changes rarely; reload it at most this often while a
/// ticket is streaming (the unconditional reload catches up afterwards).
const HISTORY_GAP: Duration = Duration::from_millis(500);
const REOPEN: Duration = Duration::from_secs(3);
/// Maximum catch-up rounds per poll when a ticket has a large backlog.
const CATCH_UP: usize = 8;
/// Longest a single load may spend replaying a ticket's history, so a long
/// journal neither delays the first view nor commands such as a new selection.
const LOAD_BUDGET: Duration = Duration::from_millis(120);
/// Spacing of loads while a replay or usage scan is still catching up.
const BUSY_GAP: Duration = Duration::from_millis(10);

pub enum Cmd {
    /// Switch the observed ticket. `epoch` tags every reply for that selection.
    Select {
        ticket: Option<i64>,
        epoch: u64,
    },
    Search(String),
    /// Poll now (after a write) and resend the history.
    Refresh,
    JobsPage {
        ticket: i64,
        before: Option<i64>,
        epoch: u64,
    },
}

pub enum Msg {
    Connected(String),
    SourceError(String),
    History {
        query: String,
        rows: Vec<TicketSummary>,
    },
    Snapshot {
        epoch: u64,
        ticket: i64,
        full: bool,
        snap: Box<Snapshot>,
    },
    SnapshotError {
        epoch: u64,
        ticket: i64,
        error: String,
    },
    JobsPage {
        epoch: u64,
        ticket: i64,
        page: Result<JobsPage, String>,
    },
}

pub struct Worker {
    pub tx: Sender<Cmd>,
    pub rx: Receiver<Msg>,
}

pub fn spawn(opener: Opener, ctx: egui::Context) -> Worker {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (msg_tx, msg_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("sa-desktop-reader".into())
        .spawn(move || Reader::new(opener, msg_tx, ctx).run(cmd_rx))
        .expect("spawn reader thread");
    Worker {
        tx: cmd_tx,
        rx: msg_rx,
    }
}

struct Reader {
    opener: Opener,
    source: Option<Box<dyn DataSource>>,
    next_open: Instant,
    tx: Sender<Msg>,
    ctx: egui::Context,
    query: String,
    last_history: Option<Vec<TicketSummary>>,
    selected: Option<(i64, u64)>,
    cursor: i64,
    /// Last snapshot sent, without events, to skip unchanged polls.
    last_meta: Option<Snapshot>,
    last_error: Option<String>,
    /// Change counter read just before the last load.
    seen: Option<i64>,
    /// When the session list was last read.
    history_at: Option<Instant>,
    /// The selected ticket's history replay or usage scan has not caught up.
    busy: bool,
    /// A `loading` snapshot of the selected ticket was already shown.
    loading_sent: bool,
}

fn message(e: &anyhow::Error) -> String {
    format!("{e:#}")
}

impl Reader {
    fn new(opener: Opener, tx: Sender<Msg>, ctx: egui::Context) -> Self {
        Self {
            opener,
            source: None,
            next_open: Instant::now(),
            tx,
            ctx,
            query: String::new(),
            last_history: None,
            selected: None,
            cursor: 0,
            last_meta: None,
            last_error: None,
            seen: None,
            history_at: None,
            busy: false,
            loading_sent: false,
        }
    }

    /// Whether the journal was committed to since the last load.
    fn changed(&mut self) -> bool {
        let seen = self.seen;
        self.source
            .as_mut()
            .and_then(|src| src.version())
            .is_some_and(|v| Some(v) != seen)
    }

    fn send(&self, msg: Msg) -> bool {
        let ok = self.tx.send(msg).is_ok();
        self.ctx.request_repaint();
        ok
    }

    fn run(mut self, rx: Receiver<Cmd>) {
        let mut next_poll = Instant::now();
        let mut last_load: Option<Instant> = None;
        loop {
            let now = Instant::now();
            let wait = next_poll.saturating_duration_since(now).min(WATCH);
            let mut urgent = false;
            match rx.recv_timeout(wait) {
                Ok(cmd) => {
                    urgent = self.handle(cmd);
                    while let Ok(cmd) = rx.try_recv() {
                        urgent |= self.handle(cmd);
                    }
                    if !urgent {
                        continue;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if Instant::now() < next_poll {
                        // A watch tick: load only when something was committed,
                        // and not more often than `MIN_GAP`. An early change is
                        // picked up by the next tick because `seen` is untouched.
                        if !self.changed() || last_load.is_some_and(|at| at.elapsed() < MIN_GAP) {
                            continue;
                        }
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if !self.poll(urgent) {
                return;
            }
            last_load = Some(Instant::now());
            next_poll = Instant::now() + if self.busy { BUSY_GAP } else { POLL };
        }
    }

    /// Returns true when the command needs an immediate poll.
    fn handle(&mut self, cmd: Cmd) -> bool {
        match cmd {
            Cmd::Select { ticket, epoch } => {
                self.selected = ticket.map(|t| (t, epoch));
                self.cursor = 0;
                self.last_meta = None;
                self.busy = false;
                self.loading_sent = false;
                true
            }
            Cmd::Search(q) => {
                self.query = q;
                self.last_history = None;
                self.history_at = None;
                true
            }
            Cmd::Refresh => true,
            Cmd::JobsPage {
                ticket,
                before,
                epoch,
            } => {
                if let Some(src) = self.source.as_mut() {
                    let page = src.jobs_page(ticket, before).map_err(|e| message(&e));
                    self.send(Msg::JobsPage {
                        epoch,
                        ticket,
                        page,
                    });
                } else {
                    let error = self
                        .last_error
                        .clone()
                        .unwrap_or_else(|| "数据源尚未连接".into());
                    self.send(Msg::JobsPage {
                        epoch,
                        ticket,
                        page: Err(error),
                    });
                }
                false
            }
        }
    }

    fn fail(&mut self, error: String) {
        // Drop the connection so a replaced or repaired database is reopened.
        self.source = None;
        self.next_open = Instant::now() + REOPEN;
        self.last_history = None;
        self.history_at = None;
        self.seen = None;
        self.cursor = 0;
        self.last_meta = None;
        self.busy = false;
        self.loading_sent = false;
        if self.last_error.as_deref() != Some(error.as_str()) {
            self.last_error = Some(error.clone());
            self.send(Msg::SourceError(error));
        }
    }

    /// One load. `urgent` (the UI asked) also reloads the session list at
    /// once. Returns false when the UI has gone away.
    fn poll(&mut self, urgent: bool) -> bool {
        if self.source.is_none() {
            if Instant::now() < self.next_open {
                return true;
            }
            match (self.opener)() {
                Ok(src) => {
                    let label = src.label();
                    self.source = Some(src);
                    self.last_error = None;
                    if !self.send(Msg::Connected(label)) {
                        return false;
                    }
                }
                Err(e) => {
                    self.fail(message(&e));
                    return true;
                }
            }
        }

        let Some(src) = self.source.as_mut() else {
            return true;
        };
        // Read the counter first: a commit that lands during this load must
        // trigger another one.
        self.seen = src.version();
        let history_due = urgent
            || self.selected.is_none()
            || self.history_at.is_none_or(|at| at.elapsed() >= HISTORY_GAP);
        if history_due {
            self.history_at = Some(Instant::now());
            match src.history(&self.query) {
                Ok(rows) => {
                    if self.last_history.as_ref() != Some(&rows) {
                        self.last_history = Some(rows.clone());
                        if !self.send(Msg::History {
                            query: self.query.clone(),
                            rows,
                        }) {
                            return false;
                        }
                    }
                }
                Err(e) => {
                    self.fail(message(&e));
                    return true;
                }
            }
        }

        let Some((ticket, epoch)) = self.selected else {
            self.busy = false;
            return true;
        };
        for _ in 0..CATCH_UP {
            let Some(src) = self.source.as_mut() else {
                return true;
            };
            let full = self.cursor == 0;
            let snap = match src.snapshot_within(ticket, self.cursor, LOAD_BUDGET) {
                Ok(s) => s,
                Err(e) => {
                    let error = message(&e);
                    // A missing ticket is not a connection failure.
                    return self.send(Msg::SnapshotError {
                        epoch,
                        ticket,
                        error,
                    });
                }
            };
            if snap.loading {
                // The history replay has not reached the end: show the visible
                // events once, then keep loading a full snapshot. A loading
                // snapshot never advances the cursor, so no activity is lost.
                self.busy = true;
                if full && !self.loading_sent {
                    self.loading_sent = true;
                    return self.send(Msg::Snapshot {
                        epoch,
                        ticket,
                        full,
                        snap: Box::new(snap),
                    });
                }
                return true;
            }
            self.busy = snap.usage.pending;
            let caught_up = full || snap.events.len() < 180 || snap.cursor <= self.cursor;
            self.cursor = snap.cursor.max(self.cursor);
            // Keep quota in metadata: window updates and clock-driven expiry
            // must reach the UI even when no journal events were appended.
            let mut meta = snap.clone();
            meta.events.clear();
            meta.cursor = 0;
            // Observation refreshes alone do not change a window's display.
            // Expiry is still detected when the source drops the row.
            for observed in &mut meta.quota {
                observed.observed_at = 0.0;
            }
            let changed = full || !snap.events.is_empty() || self.last_meta.as_ref() != Some(&meta);
            self.last_meta = Some(meta);
            if changed
                && !self.send(Msg::Snapshot {
                    epoch,
                    ticket,
                    full,
                    snap: Box::new(snap),
                })
            {
                return false;
            }
            if caught_up {
                break;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

    /// A source whose history is one ticket titled with the current version.
    struct Counted {
        version: Arc<AtomicI64>,
        loads: Arc<AtomicUsize>,
    }

    impl DataSource for Counted {
        fn label(&self) -> String {
            "test".into()
        }
        fn history(&mut self, _: &str) -> anyhow::Result<Vec<TicketSummary>> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            Ok(vec![TicketSummary {
                id: 1,
                goal: self.version.load(Ordering::SeqCst).to_string(),
                cwd: String::new(),
                lead: "codex".into(),
                created: 0.0,
                state: "running".into(),
                title: None,
                pinned: false,
                archived: false,
                attention: None,
            }])
        }
        fn snapshot(&mut self, _: i64, _: i64) -> anyhow::Result<Snapshot> {
            anyhow::bail!("no ticket")
        }
        fn jobs_page(&mut self, _: i64, _: Option<i64>) -> anyhow::Result<JobsPage> {
            anyhow::bail!("no ticket")
        }
        fn version(&mut self) -> Option<i64> {
            Some(self.version.load(Ordering::SeqCst))
        }
    }

    struct SnapshotSource {
        current: Arc<std::sync::Mutex<Snapshot>>,
    }

    impl DataSource for SnapshotSource {
        fn label(&self) -> String {
            "snapshot fixture".into()
        }
        fn history(&mut self, _: &str) -> anyhow::Result<Vec<TicketSummary>> {
            Ok(Vec::new())
        }
        fn snapshot(&mut self, _: i64, _: i64) -> anyhow::Result<Snapshot> {
            Ok(self.current.lock().unwrap().clone())
        }
        fn jobs_page(&mut self, _: i64, _: Option<i64>) -> anyhow::Result<JobsPage> {
            anyhow::bail!("no page")
        }
        fn version(&mut self) -> Option<i64> {
            // The clock can expire a window without changing the journal.
            Some(1)
        }
    }

    fn quota_snapshot() -> Snapshot {
        Snapshot {
            ticket: sa_store::TicketDetail {
                id: 1,
                goal: "quota polling".into(),
                objective: None,
                cwd: String::new(),
                lead: "codex".into(),
                created: 0.0,
                permission_profile: "read".into(),
                agent_routes: serde_json::json!({}),
                allow_lead_routing: false,
                state: "queued".into(),
                pause_status: None,
                attention: None,
            },
            jobs: Vec::new(),
            total_jobs: 1,
            running_peers: 0,
            queued_peers: 0,
            native_activity: Default::default(),
            loading: false,
            lead_resumed: 0,
            usage: Default::default(),
            quota: vec![sa_store::quota::Observed {
                agent: "codex".into(),
                provider: "account".into(),
                window: sa_store::quota::Window {
                    kind: "primary".into(),
                    used: Some(0.85),
                    resets_at: Some(3600.0),
                    exhausted: false,
                    minutes: Some(300),
                },
                observed_at: 1.0,
            }],
            events: Vec::new(),
            cursor: 1,
            approvals: Vec::new(),
            questions: Vec::new(),
            run: None,
            plan: Vec::new(),
        }
    }

    fn snapshot_messages(rx: &Receiver<Msg>) -> Vec<(bool, Snapshot)> {
        rx.try_iter().filter_map(|msg| match msg {
            Msg::Snapshot { full, snap, .. } => Some((full, *snap)),
            _ => None,
        }).collect()
    }

    #[test]
    fn window_updates_and_expiry_reach_ui_without_events_and_unchanged_polls_are_skipped() {
        let current = Arc::new(std::sync::Mutex::new(quota_snapshot()));
        let (tx, rx) = mpsc::channel();
        let mut reader = Reader::new(
            Box::new(|| anyhow::bail!("fixture is already open")),
            tx,
            egui::Context::default(),
        );
        reader.source = Some(Box::new(SnapshotSource { current: current.clone() }));
        reader.handle(Cmd::Select { ticket: Some(1), epoch: 7 });
        assert!(reader.poll(true));
        let initial = snapshot_messages(&rx);
        assert_eq!(initial.len(), 1);
        assert!(initial[0].0);
        assert_eq!(initial[0].1.quota.len(), 1);
        assert!(reader.poll(false));
        assert!(snapshot_messages(&rx).is_empty());
        current.lock().unwrap().quota[0].observed_at = 2.0;
        assert!(reader.poll(false));
        assert!(snapshot_messages(&rx).is_empty(), "observation refresh is not a window change");

        // A refreshed quota row is forwarded despite an unchanged event cursor.
        current.lock().unwrap().quota[0].window.used = Some(0.95);
        assert!(!reader.changed());
        assert!(reader.poll(false));
        let changed = snapshot_messages(&rx);
        assert_eq!(changed.len(), 1);
        assert!(!changed[0].0);
        assert!(changed[0].1.events.is_empty());
        assert_eq!(changed[0].1.cursor, 1);
        assert_eq!(changed[0].1.quota[0].window.used, Some(0.95));
        assert_eq!(changed[0].1.quota[0].observed_at, 2.0);
        assert!(reader.poll(false));
        assert!(snapshot_messages(&rx).is_empty());

        // A fallback poll sees the source drop an expired window, even though
        // neither the change counter nor the event cursor has advanced.
        current.lock().unwrap().quota.clear();
        assert!(!reader.changed());
        assert!(reader.poll(false));
        let expired = snapshot_messages(&rx);
        assert_eq!(expired.len(), 1);
        assert!(expired[0].1.quota.is_empty());
        assert!(expired[0].1.events.is_empty());
        assert_eq!(expired[0].1.cursor, 1);
        assert!(reader.poll(false));
        assert!(snapshot_messages(&rx).is_empty());
    }

    /// Replays a long history: `loading` for the first `remaining` loads.
    struct Replaying {
        remaining: usize,
        afters: Arc<std::sync::Mutex<Vec<i64>>>,
    }

    impl DataSource for Replaying {
        fn label(&self) -> String {
            "replaying".into()
        }
        fn history(&mut self, _: &str) -> anyhow::Result<Vec<TicketSummary>> {
            Ok(Vec::new())
        }
        fn snapshot(&mut self, _: i64, _: i64) -> anyhow::Result<Snapshot> {
            anyhow::bail!("the reader must use the budgeted load")
        }
        fn snapshot_within(&mut self, _: i64, after: i64, budget: Duration) -> anyhow::Result<Snapshot> {
            assert_eq!(budget, LOAD_BUDGET);
            self.afters.lock().unwrap().push(after);
            let mut snap = quota_snapshot();
            snap.loading = self.remaining > 0;
            self.remaining = self.remaining.saturating_sub(1);
            Ok(snap)
        }
        fn jobs_page(&mut self, _: i64, _: Option<i64>) -> anyhow::Result<JobsPage> {
            anyhow::bail!("no page")
        }
        fn version(&mut self) -> Option<i64> {
            Some(1)
        }
    }

    #[test]
    fn a_loading_snapshot_is_shown_once_and_reloaded_in_full_until_the_replay_ends() {
        let afters = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel();
        let mut reader = Reader::new(
            Box::new(|| anyhow::bail!("fixture is already open")),
            tx,
            egui::Context::default(),
        );
        reader.source = Some(Box::new(Replaying { remaining: 3, afters: afters.clone() }));
        reader.handle(Cmd::Select { ticket: Some(1), epoch: 7 });
        assert!(reader.poll(true));
        let first = snapshot_messages(&rx);
        assert_eq!(first.len(), 1);
        assert!(first[0].0 && first[0].1.loading, "the visible events are shown at once");
        assert!(reader.busy, "the next load follows without the one-second poll");
        for _ in 0..2 {
            assert!(reader.poll(false));
            assert!(snapshot_messages(&rx).is_empty(), "an unfinished replay is not resent");
            assert!(reader.busy);
        }
        assert!(reader.poll(false));
        let done = snapshot_messages(&rx);
        assert_eq!(done.len(), 1);
        assert!(done[0].0 && !done[0].1.loading, "the finished replay replaces the view");
        assert!(!reader.busy);
        assert_eq!(*afters.lock().unwrap(), vec![0, 0, 0, 0], "loading never advances the cursor");

        // A new selection starts over and shows its first loading view again.
        reader.source = Some(Box::new(Replaying { remaining: 1, afters: afters.clone() }));
        reader.handle(Cmd::Select { ticket: Some(2), epoch: 8 });
        assert!(reader.poll(true));
        let again = snapshot_messages(&rx);
        assert_eq!(again.len(), 1);
        assert!(again[0].1.loading);
    }

    fn history(rx: &Receiver<Msg>, within: Duration) -> Option<String> {
        let end = Instant::now() + within;
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(Msg::History { rows, .. }) => return rows.first().map(|r| r.goal.clone()),
                Ok(_) => {}
                Err(_) => return None,
            }
        }
        None
    }

    #[test]
    fn a_commit_reaches_the_ui_well_before_the_fallback_poll() {
        let version = Arc::new(AtomicI64::new(1));
        let loads = Arc::new(AtomicUsize::new(0));
        let (v, l) = (version.clone(), loads.clone());
        let worker = spawn(
            Box::new(move || {
                Ok(Box::new(Counted { version: v.clone(), loads: l.clone() }) as Box<dyn DataSource>)
            }),
            egui::Context::default(),
        );
        assert_eq!(history(&worker.rx, Duration::from_secs(2)).as_deref(), Some("1"));

        // Nothing changed: the watch ticks must not load.
        std::thread::sleep(Duration::from_millis(400));
        let idle = loads.load(Ordering::SeqCst);
        assert!(idle <= 2, "an unchanged source was loaded {idle} times in 400 ms");

        // A change is delivered within a few watch ticks, not after `POLL`.
        let started = Instant::now();
        version.store(2, Ordering::SeqCst);
        assert_eq!(history(&worker.rx, Duration::from_millis(600)).as_deref(), Some("2"));
        let took = started.elapsed();
        assert!(took < Duration::from_millis(500), "change took {took:?}");
    }
}
