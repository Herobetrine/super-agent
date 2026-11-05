//! Journal connection and read operations (port of `runtime/studio.cjs` reads).

use crate::model::*;
use crate::progress::{Entry, ScanRow, VISIBLE};
use crate::schema::{ADDITIONS, SCHEMA, STUDIO_PLAN};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Database file name inside the data directory.
pub const DB_FILE: &str = "hub.sqlite3";

/// Default data directory: `SUPERAGENT_DATA` or `~/.local/share/superagent/control-room`.
pub fn default_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SUPERAGENT_DATA") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".local")
        .join("share")
        .join("superagent")
        .join("control-room")
}

/// `Number.MAX_SAFE_INTEGER`
const MAX_SAFE: i64 = 9_007_199_254_740_991;
/// Rows per progress scan batch. JS uses 64 only to yield to the event loop;
/// the batch size does not affect the projection.
const SCAN_BATCH: i64 = 1024;

const JOB_DETAILS: &str = "SELECT j.id,agent,parent,role,state,turn,writing,ready_after,permission_override,
  substr(body,1,8000) body,substr(result,1,5000) result,n.route FROM jobs j LEFT JOIN native_routes n ON n.job=j.id";

const PROGRESS_SCAN: &str = "SELECT e.seq,e.job,e.turn,e.ts,e.kind,j.agent,
      CASE WHEN e.kind='native' THEN e.payload END payload,
      CASE WHEN e.kind='job.progress' THEN json_extract(e.payload,'$.summary') END summary,
      CASE WHEN e.kind='mcp.result' THEN json_extract(e.payload,'$.state')
           WHEN e.kind IN ('auto.milestone','auto.exhausted') THEN json_extract(e.payload,'$.status')
           WHEN e.kind='approval.decision' THEN json_extract(e.payload,'$.decision') END status
      FROM events e LEFT JOIN jobs j ON j.id=e.job WHERE e.ticket=? AND e.seq>? AND e.seq<=?
      AND e.kind NOT IN ('delta','message.boundary') ORDER BY e.seq LIMIT ?";

fn expect(ok: bool, msg: &str) -> Result<()> {
    ensure!(ok, "{msg}");
    Ok(())
}

/// `id(v)`: positive safe integer.
fn id(v: i64) -> Result<i64> {
    expect(v > 0 && v <= MAX_SAFE, "编号无效")?;
    Ok(v)
}

fn json(text: &str) -> Result<Value> {
    serde_json::from_str(text).with_context(|| format!("invalid JSON in journal: {:.80}", text))
}

/// Column value only when SQLite returned TEXT (JS `typeof v === 'string'`).
fn text_only(row: &Row, idx: usize) -> rusqlite::Result<Option<String>> {
    Ok(match row.get_ref(idx)? {
        ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
        _ => None,
    })
}

fn truthy_int(row: &Row, idx: usize) -> rusqlite::Result<bool> {
    Ok(match row.get_ref(idx)? {
        ValueRef::Integer(i) => i != 0,
        ValueRef::Real(f) => f != 0.0,
        ValueRef::Null => false,
        ValueRef::Text(t) | ValueRef::Blob(t) => !t.is_empty(),
    })
}

struct RawJob {
    view: JobView,
    route: Option<String>,
}

fn raw_job(r: &Row) -> rusqlite::Result<RawJob> {
    Ok(RawJob {
        view: JobView {
            id: r.get(0)?,
            agent: r.get(1)?,
            parent: r.get(2)?,
            role: r.get(3)?,
            state: r.get(4)?,
            turn: r.get(5)?,
            writing: truthy_int(r, 6)?,
            ready_after: r.get(7)?,
            permission_override: r.get(8)?,
            body: r.get(9)?,
            result: r.get(10)?,
            route: None,
        },
        route: r.get(11)?,
    })
}

/// `jobView(row)`
fn job_view(raw: RawJob) -> Result<JobView> {
    let mut view = raw.view;
    view.route = match raw.route {
        Some(route) if !route.is_empty() => Some(json(&route)?),
        _ => None,
    };
    Ok(view)
}

pub struct Store {
    conn: Connection,
    /// Per-ticket progress projection (`ProgressJournal.seen` + TEMP view).
    progress: HashMap<i64, Entry>,
}

impl Store {
    /// Open an existing journal without writing to it (observer mode).
    /// Safe to use while another process owns the scheduler.
    pub fn open_read_only(dir: &Path) -> Result<Self> {
        let path = dir.join(DB_FILE);
        ensure!(path.is_file(), "journal not found: {}", path.display());
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )
        .with_context(|| format!("open {} read-only", path.display()))?;
        conn.busy_timeout(Duration::from_millis(5000))?;
        Ok(Self {
            conn,
            progress: HashMap::new(),
        })
    }

    /// Open (creating if needed) and migrate the journal like `engine-schema.cjs`.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let path = dir.join(DB_FILE);
        let mut conn =
            Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
        conn.busy_timeout(Duration::from_millis(5000))?;
        let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(SCHEMA)?;
        for (table, columns) in ADDITIONS {
            let existing: HashSet<String> = tx
                .prepare(&format!("PRAGMA table_info({table})"))?
                .query_map([], |r| r.get::<_, String>("name"))?
                .collect::<rusqlite::Result<_>>()?;
            for (name, definition) in *columns {
                if !existing.contains(*name) {
                    tx.execute_batch(&format!(
                        "ALTER TABLE {table} ADD COLUMN {name} {definition}"
                    ))?;
                }
            }
        }
        tx.commit()?;
        conn.execute_batch(STUDIO_PLAN)?;
        Ok(Self {
            conn,
            progress: HashMap::new(),
        })
    }

    /// Sidebar history, newest first, at most 150 rows. `q` filters goal or cwd.
    pub fn history(&self, q: &str) -> Result<Vec<TicketSummary>> {
        expect(crate::activity::js_len(q) <= 512, "文本长度或格式无效")?;
        let mut pattern = String::from("%");
        for c in q.chars() {
            if matches!(c, '\\' | '%' | '_') {
                pattern.push('\\');
            }
            pattern.push(c);
        }
        pattern.push('%');
        // `studio_ticket_meta` is created by the scheduler; older journals lack it.
        let meta = self
            .conn
            .prepare_cached("SELECT 1 FROM sqlite_master WHERE type='table' AND name='studio_ticket_meta'")?
            .exists([])?;
        let sql = if meta {
            "SELECT t.id,substr(t.goal,1,500) goal,t.cwd,t.lead,t.created,j.state,m.title,
        coalesce(m.pinned,0),coalesce(m.archived,0)
      FROM tickets t JOIN jobs j ON j.ticket=t.id AND j.parent IS NULL
      LEFT JOIN studio_ticket_meta m ON m.ticket=t.id
      WHERE t.goal LIKE ?1 ESCAPE '\\' OR t.cwd LIKE ?1 ESCAPE '\\' OR m.title LIKE ?1 ESCAPE '\\'
      ORDER BY t.id DESC LIMIT 150"
        } else {
            "SELECT t.id,substr(t.goal,1,500) goal,t.cwd,t.lead,t.created,j.state,NULL,0,0
      FROM tickets t JOIN jobs j ON j.ticket=t.id AND j.parent IS NULL
      WHERE t.goal LIKE ?1 ESCAPE '\\' OR t.cwd LIKE ?1 ESCAPE '\\' ORDER BY t.id DESC LIMIT 150"
        };
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map([&pattern], |r| {
            Ok(TicketSummary {
                id: r.get(0)?,
                goal: r.get(1)?,
                cwd: r.get(2)?,
                lead: r.get(3)?,
                created: r.get(4)?,
                state: r.get(5)?,
                title: r.get(6)?,
                pinned: r.get::<_, i64>(7)? != 0,
                archived: r.get::<_, i64>(8)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// `ProgressJournal.ensure(ticket, end)`: project every row up to `end`.
    fn ensure(&mut self, ticket: i64, end: i64) -> Result<&Entry> {
        let entry = self.progress.entry(ticket).or_default();
        let mut stmt = self.conn.prepare_cached(PROGRESS_SCAN)?;
        while entry.seq < end {
            let rows = stmt
                .query_map(params![ticket, entry.seq, end, SCAN_BATCH], |r| {
                    Ok(ScanRow {
                        seq: r.get(0)?,
                        job: r.get(1)?,
                        turn: r.get(2)?,
                        ts: r.get(3)?,
                        kind: r.get(4)?,
                        agent: r.get(5)?,
                        payload: r.get(6)?,
                        summary: text_only(r, 7)?,
                        status: text_only(r, 8)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let count = rows.len() as i64;
            for row in rows {
                entry.absorb(row);
            }
            if count < SCAN_BATCH {
                entry.seq = end;
            }
        }
        Ok(entry)
    }

    fn max_seq(&self, ticket: i64, high: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT coalesce(max(seq),0) n FROM events WHERE ticket=? AND seq<=?",
            params![ticket, high],
            |r| r.get(0),
        )?)
    }

    fn has_table(&self, name: &str) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?")?
            .exists([name])?)
    }

    /// Ticket snapshot. `after == 0` returns the latest 180 visible events;
    /// otherwise up to 180 events newer than `after`.
    pub fn snapshot(&mut self, ticket: i64, after: i64) -> Result<Snapshot> {
        id(ticket)?;
        expect((0..=MAX_SAFE).contains(&after), "事件游标无效")?;
        let end = self.max_seq(ticket, MAX_SAFE)?;
        self.ensure(ticket, end)?;
        let conn = &self.conn;

        let ticket_row = conn
            .query_row(
                "SELECT t.id,t.goal,t.objective,t.cwd,t.lead,t.created,t.permission_profile,
      t.agent_routes,t.allow_lead_routing,j.state FROM tickets t JOIN jobs j ON j.ticket=t.id AND j.parent IS NULL WHERE t.id=?",
                [ticket],
                |r| {
                    Ok((
                        TicketDetail {
                            id: r.get(0)?,
                            goal: r.get(1)?,
                            objective: r.get(2)?,
                            cwd: r.get(3)?,
                            lead: r.get(4)?,
                            created: r.get(5)?,
                            permission_profile: r.get(6)?,
                            agent_routes: Value::Null,
                            allow_lead_routing: truthy_int(r, 8)?,
                            state: r.get(9)?,
                        },
                        r.get::<_, String>(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((mut detail, routes)) = ticket_row else {
            bail!("工单不存在");
        };
        detail.agent_routes = json(&routes)?;

        let (total_jobs, running_peers): (i64, i64) = conn.query_row(
            "SELECT count(*) totalJobs,
      count(*) FILTER (WHERE parent IS NOT NULL AND state='running') runningPeers FROM jobs WHERE ticket=?",
            [ticket],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;

        let jobs = conn
            .prepare_cached(&format!(
                "{JOB_DETAILS} WHERE ticket=? ORDER BY (parent IS NULL) DESC,
      (state IN ('running','waiting','queued','interrupted')) DESC,j.id DESC LIMIT 80"
            ))?
            .query_map([ticket], raw_job)?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(job_view)
            .collect::<Result<Vec<_>>>()?;

        // Work only on visible events. Project reply text before crossing IPC.
        let kinds = VISIBLE
            .iter()
            .map(|k| format!("'{k}'"))
            .collect::<Vec<_>>()
            .join(",");
        let order = if after != 0 { "ASC" } else { "DESC" };
        let sql = format!(
            "SELECT seq,kind,ts,job,turn,CASE
      WHEN kind='reply' THEN json_object('text',substr(json_extract(payload,'$.text'),1,64000))
      WHEN kind='mcp.result' THEN json_object('id',json_extract(payload,'$.id'),'server',json_extract(payload,'$.server'),
        'tool',json_extract(payload,'$.tool'),'state',json_extract(payload,'$.state'))
      WHEN kind IN ('user','user.follow','delta','failed','cancelled','interrupted','auto.recover') THEN
        CASE WHEN json_type(payload)='text' THEN json_quote(substr(json_extract(payload,'$'),1,64000)) ELSE payload END
      ELSE payload END payload FROM events WHERE ticket=? AND seq>? AND seq<=? AND kind IN ({kinds})
      ORDER BY seq {order} LIMIT 180"
        );
        let mut merged = conn
            .prepare_cached(&sql)?
            .query_map(params![ticket, after, end], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, f64>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?
            .map(|row| {
                let (seq, kind, ts, job, turn, payload) = row?;
                Ok(EventView {
                    seq,
                    kind,
                    ts,
                    job,
                    turn,
                    agent: None,
                    payload: json(&payload)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let entry = &self.progress[&ticket];
        merged.extend(entry.read(after, end, after == 0, 180, true));
        merged.sort_by_key(|e| e.seq);
        let events: Vec<EventView> = if after != 0 {
            merged.truncate(180);
            merged
        } else {
            let skip = merged.len().saturating_sub(180);
            merged.split_off(skip)
        };
        let cursor = match events.last() {
            Some(last) if after != 0 && events.len() == 180 => last.seq,
            _ => end,
        };

        let mut owners: HashMap<i64, Option<(String, i64)>> = jobs
            .iter()
            .map(|j| (j.id, Some((j.state.clone(), j.turn))))
            .collect();
        let mut native = entry.native.view();
        for task in &mut native.tasks {
            if task.state != "running" {
                continue;
            }
            let owner = match owners.entry(task.job) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => e.insert(
                    conn.prepare_cached("SELECT state,turn FROM jobs WHERE id=? AND ticket=?")?
                        .query_row(params![task.job, ticket], |r| Ok((r.get(0)?, r.get(1)?)))
                        .optional()?,
                ),
            };
            let live =
                matches!(owner, Some((state, turn)) if state == "running" && *turn == task.turn);
            if !live {
                task.state = "unknown".to_owned();
            }
        }

        let approvals = conn
            .prepare_cached("SELECT id,job,action FROM approvals WHERE ticket=? AND decision='pending' ORDER BY id")?
            .query_map([ticket], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))?
            .map(|row| {
                let (id, job, action) = row?;
                Ok(Approval { id, job, action: json(&action)? })
            })
            .collect::<Result<Vec<_>>>()?;

        let run = conn
            .prepare_cached("SELECT phase,milestone,learning FROM continuous_runs WHERE ticket=?")?
            .query_row([ticket], |r| {
                Ok(ContinuousRun {
                    phase: r.get(0)?,
                    milestone: r.get(1)?,
                    learning: r.get(2)?,
                })
            })
            .optional()?;

        // The table is created by the writer; an observer may predate it.
        let plan = if self.has_table("studio_plan")? {
            conn.prepare_cached("SELECT id,body,done FROM studio_plan WHERE ticket=? ORDER BY id")?
                .query_map([ticket], |r| {
                    Ok(PlanItem {
                        id: r.get(0)?,
                        body: r.get(1)?,
                        done: truthy_int(r, 2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            Vec::new()
        };

        Ok(Snapshot {
            ticket: detail,
            jobs,
            total_jobs,
            running_peers,
            native_activity: native,
            events,
            cursor,
            approvals,
            run,
            plan,
        })
    }

    /// Collaboration jobs, newest first, 80 per page.
    pub fn jobs_page(&self, ticket: i64, before: Option<i64>) -> Result<JobsPage> {
        let cursor = match before {
            None => MAX_SAFE,
            Some(b) => id(b)?,
        };
        let rows = self
            .conn
            .prepare_cached(&format!(
                "{JOB_DETAILS} WHERE ticket=? AND j.id<? ORDER BY j.id DESC LIMIT 81"
            ))?
            .query_map(params![id(ticket)?, cursor], raw_job)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let more = rows.len() > 80;
        let jobs = rows
            .into_iter()
            .take(80)
            .map(job_view)
            .collect::<Result<Vec<_>>>()?;
        let next = jobs.last().map(|j| j.id);
        Ok(JobsPage { jobs, more, next })
    }

    /// Derived progress rows, newest first, 80 per page.
    pub fn activity_page(&mut self, ticket: i64, before: Option<i64>) -> Result<ActivityPage> {
        id(ticket)?;
        let high = match before {
            None => MAX_SAFE,
            Some(b) => id(b)? - 1,
        };
        let end = self.max_seq(ticket, high)?;
        let entry = self.ensure(ticket, end)?;
        let mut events = entry.read(0, end, true, 81, false);
        let more = events.len() > 80;
        events.truncate(80);
        let next = events.last().map(|e| e.seq);
        Ok(ActivityPage { events, more, next })
    }
}
