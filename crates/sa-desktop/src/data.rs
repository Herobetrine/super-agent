//! Read-only data sources behind the observer UI.
//!
//! A source is created and used only on the background worker thread, so it
//! does not need to be `Send` or `Sync` (the SQLite-backed `Store` is neither
//! guaranteed to be).

use anyhow::{Result, bail};
use sa_store::{DB_FILE, JobsPage, Snapshot, Store, TicketSummary};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub trait DataSource {
    /// Short description shown in the sidebar footer.
    fn label(&self) -> String;
    /// Sidebar history, newest first.
    fn history(&mut self, q: &str) -> Result<Vec<TicketSummary>>;
    /// Ticket snapshot; `after == 0` loads the latest events.
    fn snapshot(&mut self, ticket: i64, after: i64) -> Result<Snapshot>;
    /// Like `snapshot`, but a source with a long history may stop replaying
    /// it after `budget` and return a `loading` snapshot instead (see
    /// `Store::snapshot_within`). Sources without a replay load in full.
    fn snapshot_within(&mut self, ticket: i64, after: i64, budget: Duration) -> Result<Snapshot> {
        let _ = budget;
        self.snapshot(ticket, after)
    }
    /// Collaboration jobs, newest first, 80 per page.
    fn jobs_page(&mut self, ticket: i64, before: Option<i64>) -> Result<JobsPage>;
    /// A value that changes when the underlying data does, or `None` when the
    /// source cannot tell cheaply (it is then read on a fixed cadence).
    fn version(&mut self) -> Option<i64> {
        None
    }
}

/// Opens a source on the worker thread. Called again after failures.
pub type Opener = Box<dyn Fn() -> Result<Box<dyn DataSource>> + Send>;

/// The ticket journal, opened read-only next to the running scheduler.
pub struct StoreSource {
    store: Store,
    _reader_lock: Option<File>,
    dir: PathBuf,
}

impl StoreSource {
    pub fn open(dir: &Path) -> Result<Self> {
        let db = dir.join(DB_FILE);
        if !db.is_file() {
            bail!("未找到工单数据库：{}", db.display());
        }
        let reader_lock = sa_store::journal::lock_journal_reader(dir)?;
        let store = Store::open_read_only(dir)
            .map_err(|e| anyhow::anyhow!("无法以只读方式打开工单数据库：{e:#}"))?;
        Ok(Self {
            store,
            _reader_lock: reader_lock,
            dir: dir.to_path_buf(),
        })
    }

    pub fn opener(dir: PathBuf) -> Opener {
        Box::new(move || Ok(Box::new(StoreSource::open(&dir)?) as Box<dyn DataSource>))
    }
}

impl DataSource for StoreSource {
    fn label(&self) -> String {
        format!("只读连接 · {}", self.dir.join(DB_FILE).display())
    }

    fn history(&mut self, q: &str) -> Result<Vec<TicketSummary>> {
        self.store.history(q)
    }

    fn snapshot(&mut self, ticket: i64, after: i64) -> Result<Snapshot> {
        self.store.snapshot(ticket, after)
    }

    fn snapshot_within(&mut self, ticket: i64, after: i64, budget: Duration) -> Result<Snapshot> {
        self.store.snapshot_within(ticket, after, budget)
    }

    fn jobs_page(&mut self, ticket: i64, before: Option<i64>) -> Result<JobsPage> {
        self.store.jobs_page(ticket, before)
    }

    fn version(&mut self) -> Option<i64> {
        self.store.data_version().ok()
    }
}
