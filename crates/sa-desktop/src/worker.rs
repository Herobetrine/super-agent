//! Background reader. Owns the data source, polls it about once a second and
//! forwards changes to the UI thread over a channel. The UI never touches SQLite.

use crate::data::{DataSource, Opener};
use sa_store::{JobsPage, Snapshot, TicketSummary};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(1000);
const REOPEN: Duration = Duration::from_secs(3);
/// Maximum catch-up rounds per poll when a ticket has a large backlog.
const CATCH_UP: usize = 8;

pub enum Cmd {
    /// Switch the observed ticket. `epoch` tags every reply for that selection.
    Select {
        ticket: Option<i64>,
        epoch: u64,
    },
    Search(String),
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
        }
    }

    fn send(&self, msg: Msg) -> bool {
        let ok = self.tx.send(msg).is_ok();
        self.ctx.request_repaint();
        ok
    }

    fn run(mut self, rx: Receiver<Cmd>) {
        let mut next_poll = Instant::now();
        loop {
            let wait = next_poll.saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait) {
                Ok(cmd) => {
                    let mut urgent = self.handle(cmd);
                    while let Ok(cmd) = rx.try_recv() {
                        urgent |= self.handle(cmd);
                    }
                    if !urgent {
                        continue;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if !self.poll() {
                return;
            }
            next_poll = Instant::now() + POLL;
        }
    }

    /// Returns true when the command needs an immediate poll.
    fn handle(&mut self, cmd: Cmd) -> bool {
        match cmd {
            Cmd::Select { ticket, epoch } => {
                self.selected = ticket.map(|t| (t, epoch));
                self.cursor = 0;
                self.last_meta = None;
                true
            }
            Cmd::Search(q) => {
                self.query = q;
                self.last_history = None;
                true
            }
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
        self.cursor = 0;
        self.last_meta = None;
        if self.last_error.as_deref() != Some(error.as_str()) {
            self.last_error = Some(error.clone());
            self.send(Msg::SourceError(error));
        }
    }

    /// One polling round. Returns false when the UI has gone away.
    fn poll(&mut self) -> bool {
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

        let Some((ticket, epoch)) = self.selected else {
            return true;
        };
        for _ in 0..CATCH_UP {
            let Some(src) = self.source.as_mut() else {
                return true;
            };
            let full = self.cursor == 0;
            let snap = match src.snapshot(ticket, self.cursor) {
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
            let caught_up = full || snap.events.len() < 180 || snap.cursor <= self.cursor;
            self.cursor = snap.cursor.max(self.cursor);
            let mut meta = snap.clone();
            meta.events.clear();
            meta.cursor = 0;
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
