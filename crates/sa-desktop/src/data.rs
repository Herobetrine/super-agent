//! Read-only data sources behind the observer UI.
//!
//! A source is created and used only on the background worker thread, so it
//! does not need to be `Send` or `Sync` (the SQLite-backed `Store` is neither
//! guaranteed to be).

use anyhow::{Result, bail};
use sa_store::{DB_FILE, JobsPage, Snapshot, Store, TicketSummary};
use std::path::{Path, PathBuf};

pub trait DataSource {
    /// Short description shown in the sidebar footer.
    fn label(&self) -> String;
    /// Sidebar history, newest first.
    fn history(&mut self, q: &str) -> Result<Vec<TicketSummary>>;
    /// Ticket snapshot; `after == 0` loads the latest events.
    fn snapshot(&mut self, ticket: i64, after: i64) -> Result<Snapshot>;
    /// Collaboration jobs, newest first, 80 per page.
    fn jobs_page(&mut self, ticket: i64, before: Option<i64>) -> Result<JobsPage>;
}

/// Opens a source on the worker thread. Called again after failures.
pub type Opener = Box<dyn Fn() -> Result<Box<dyn DataSource>> + Send>;

/// The ticket journal, opened read-only next to the running scheduler.
pub struct StoreSource {
    store: Store,
    dir: PathBuf,
}

impl StoreSource {
    pub fn open(dir: &Path) -> Result<Self> {
        let db = dir.join(DB_FILE);
        if !db.is_file() {
            bail!("未找到工单数据库：{}", db.display());
        }
        let store = Store::open_read_only(dir)
            .map_err(|e| anyhow::anyhow!("无法以只读方式打开工单数据库：{e:#}"))?;
        Ok(Self {
            store,
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

    fn jobs_page(&mut self, ticket: i64, before: Option<i64>) -> Result<JobsPage> {
        self.store.jobs_page(ticket, before)
    }
}
