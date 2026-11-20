//! Journal access for the engine (port of `runtime/engine-store.cjs`).
//!
//! One SQLite connection behind a mutex, like the single synchronous
//! `DatabaseSync` handle in Node. Every write group runs as `BEGIN IMMEDIATE …
//! COMMIT` (SC:69-79). The mutex is never held across an `.await`.

use crate::protocol::{self as p, END_SQL, digest, stringify};
use anyhow::{Result, anyhow};
use rusqlite::{Connection, ToSql, Transaction, TransactionBehavior, types::ValueRef};
use serde_json::{Map, Value, json};
use std::{
    path::Path,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

/// `Date.now()/1000`.
pub fn now() -> f64 {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_millis() as f64) / 1000.0
}

/// Additive Rust-port side tables (`CREATE … IF NOT EXISTS`). The Node
/// engine never reads them, so the journal stays usable by both engines.
/// - `cancel_receipts`: SA_CANCEL idempotency per `(job, logical turn, id)`.
/// - `ticket_failover`: tickets with automatic failover of read-only
///   children enabled (`mode='readers'`, spec §9.2 F).
/// - `job_failovers`: original child → the sibling that replaced it.
pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS cancel_receipts(
  job INTEGER NOT NULL,source_turn INTEGER NOT NULL,msgkey TEXT NOT NULL,target INTEGER,
  seq INTEGER NOT NULL,PRIMARY KEY(job,source_turn,msgkey)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS ticket_failover(ticket INTEGER PRIMARY KEY,mode TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS quota_windows(agent TEXT NOT NULL,provider TEXT NOT NULL,kind TEXT NOT NULL,
  used REAL,resets_at REAL,exhausted INTEGER NOT NULL DEFAULT 0,minutes INTEGER,observed_at REAL NOT NULL,
  PRIMARY KEY(agent,provider,kind));
CREATE TABLE IF NOT EXISTS job_failovers(original INTEGER PRIMARY KEY,replacement INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS job_failovers_replacement ON job_failovers(replacement);
CREATE TABLE IF NOT EXISTS job_outcomes(job INTEGER PRIMARY KEY,turn INTEGER NOT NULL,payload TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS questions(id INTEGER PRIMARY KEY,ticket INTEGER NOT NULL,job INTEGER NOT NULL,
  turn INTEGER NOT NULL,msgkey TEXT NOT NULL,question TEXT NOT NULL,options TEXT NOT NULL DEFAULT '[]',
  fallback TEXT,origin TEXT NOT NULL DEFAULT 'agent',state TEXT NOT NULL DEFAULT 'pending',
  parked INTEGER NOT NULL DEFAULT 0,answer TEXT,asked REAL NOT NULL,closed REAL);
CREATE UNIQUE INDEX IF NOT EXISTS questions_pending_job ON questions(job) WHERE state='pending';
CREATE INDEX IF NOT EXISTS questions_ticket_state ON questions(ticket,state);
CREATE TABLE IF NOT EXISTS job_deps(job INTEGER NOT NULL,needs INTEGER NOT NULL,PRIMARY KEY(job,needs));
CREATE TABLE IF NOT EXISTS project_checks(project TEXT PRIMARY KEY,checks TEXT NOT NULL,updated REAL NOT NULL);
CREATE TABLE IF NOT EXISTS check_passes(project TEXT NOT NULL,directory TEXT NOT NULL,fingerprint TEXT NOT NULL,
  checks_sha TEXT NOT NULL,at REAL NOT NULL,PRIMARY KEY(project,directory));
CREATE TABLE IF NOT EXISTS job_checks(job INTEGER PRIMARY KEY,turn INTEGER NOT NULL,attempt INTEGER NOT NULL,payload TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS check_bounces(job INTEGER PRIMARY KEY,attempt INTEGER NOT NULL);";

pub const INTERRUPT_REASON: &str = "上次执行中断，可能已经产生副作用，继续前请核对实际状态。";

/// Proof that BEGIN IMMEDIATE failed with BUSY before its closure ran.
/// Transactions that began successfully never receive this marker.
#[derive(Debug)]
pub(crate) struct BeginBusy {
    pub extended_code: i32,
}

impl std::fmt::Display for BeginBusy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "SQLite BEGIN busy (code {})", self.extended_code)
    }
}

impl std::error::Error for BeginBusy {}

pub(crate) fn sqlite_error_codes(error: &anyhow::Error) -> Option<(i32, i32)> {
    match error.downcast_ref::<rusqlite::Error>()? {
        rusqlite::Error::SqliteFailure(failure, _) => Some((failure.extended_code & 0xff, failure.extended_code)),
        _ => None,
    }
}

pub struct Db {
    conn: Mutex<Option<Connection>>,
}

impl Db {
    pub fn open(dir: &Path) -> Result<Self> {
        let conn = sa_store::journal::open_journal(dir)?;
        // Additive Rust-port table; the Node engine ignores it.
        conn.execute_batch(sa_store::knowledge::SCHEMA)?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(crate::pause::SCHEMA)?;
        conn.execute_batch(crate::advice::SCHEMA)?;
        conn.execute_batch(crate::workspaces::SCHEMA)?;
        conn.execute_batch(crate::workspaces::hydrate::SCHEMA)?;
        conn.execute_batch(crate::scm::SCHEMA)?;
        conn.execute_batch(crate::checkpoints::SCHEMA)?;
        conn.execute_batch(crate::execution_policy::SCHEMA)?;
        conn.execute_batch(sa_store::delivery::SCHEMA)?;
        conn.execute_batch(crate::operations::SCHEMA)?;
        conn.set_prepared_statement_cache_capacity(256);
        Ok(Self {
            conn: Mutex::new(Some(conn)),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Connection>> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run `f` without an explicit transaction (single statements autocommit).
    pub fn with<T>(&self, f: impl FnOnce(&Q) -> Result<T>) -> Result<T> {
        let guard = self.lock();
        let conn = guard.as_ref().ok_or_else(|| anyhow!("调度台已关闭"))?;
        f(&Q(conn))
    }

    /// Read immediately when the connection is available. UI and observation
    /// callers can omit their snapshot while an engine transaction owns it.
    pub fn try_with<T>(&self, f: impl FnOnce(&Q) -> Result<T>) -> Result<Option<T>> {
        let guard = match self.conn.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
        };
        let conn = guard.as_ref().ok_or_else(|| anyhow!("调度台已关闭"))?;
        f(&Q(conn)).map(Some)
    }

    /// `transaction(db, fn)`: BEGIN IMMEDIATE, COMMIT, ROLLBACK on error.
    pub fn tx<T>(&self, f: impl FnOnce(&Q) -> Result<T>) -> Result<T> {
        self.transaction(f, false, false)
    }

    /// Admission alone can retry a BUSY begin: its transaction body has not
    /// run and therefore cannot have claimed an in-memory breaker probe.
    pub(crate) fn admission_tx<T>(&self, f: impl FnOnce(&Q) -> Result<T>) -> Result<T> {
        self.transaction(f, true, false)
    }

    /// A transaction for stream telemetry only: model text deltas and raw
    /// adapter frames, which record what a harness already did and decide
    /// nothing. It commits with `synchronous=NORMAL`, so the WAL is not forced
    /// to disk (about 2 ms per commit on the machines measured, against 0.02 ms).
    ///
    /// Every other transaction stays `FULL` and syncs the whole WAL, including
    /// the telemetry before it, so durability is still a prefix of commit
    /// order: a power loss can drop only the newest telemetry, exactly as if it
    /// had happened a moment earlier. A process crash loses nothing in either
    /// mode. Never use this for state transitions, receipts or anything a
    /// later decision reads.
    pub(crate) fn telemetry_tx<T>(&self, f: impl FnOnce(&Q) -> Result<T>) -> Result<T> {
        self.transaction(f, false, true)
    }

    fn transaction<T>(&self, f: impl FnOnce(&Q) -> Result<T>, classify_begin_busy: bool, relaxed: bool) -> Result<T> {
        let guard = self.lock();
        let conn = guard.as_ref().ok_or_else(|| anyhow!("调度台已关闭"))?;
        // `synchronous` cannot change inside a transaction: relax it before
        // BEGIN and restore it after the transaction ends, on every path.
        // Declared first so it is dropped after the transaction below.
        struct Strict<'a>(&'a Connection);
        impl Drop for Strict<'_> {
            fn drop(&mut self) {
                let _ = self.0.execute_batch("PRAGMA synchronous=FULL");
            }
        }
        let _strict = if relaxed && conn.is_autocommit() {
            conn.execute_batch("PRAGMA synchronous=NORMAL")?;
            Some(Strict(conn))
        } else {
            None
        };
        // The transaction rolls back on ordinary errors, commit errors and
        // unwinding. Its lifetime ends before the connection mutex is released.
        let was_autocommit = conn.is_autocommit();
        let transaction = match Transaction::new_unchecked(conn, TransactionBehavior::Immediate) {
            Ok(transaction) => transaction,
            Err(error) => {
                let error = anyhow::Error::from(error);
                if classify_begin_busy && was_autocommit && conn.is_autocommit()
                    && let Some((rusqlite::ffi::SQLITE_BUSY, extended_code)) = sqlite_error_codes(&error)
                {
                    return Err(error.context(BeginBusy { extended_code }));
                }
                return Err(error);
            }
        };
        let value = f(&Q(&transaction))?;
        transaction.commit()?;
        Ok(value)
    }

    pub fn is_closed(&self) -> bool {
        self.lock().is_none()
    }

    pub fn close(&self) {
        if let Some(conn) = self.lock().take() {
            conn.flush_prepared_statement_cache();
            let _ = conn.close();
        }
    }
}

/// Identity used for event rows: `job.ticket`, `job.id` and the in-memory
/// `job.turn` (the admission snapshot, S:35-49).
#[derive(Clone, Copy, Debug)]
pub struct Who {
    pub ticket: i64,
    pub id: i64,
    pub turn: i64,
}

fn to_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => p::seconds_json(f),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::Array(b.iter().map(|x| json!(x)).collect()),
    }
}

/// Query helper bound to a connection (inside or outside a transaction).
pub struct Q<'a>(pub &'a Connection);

impl Q<'_> {
    pub fn conn(&self) -> &Connection {
        self.0
    }

    pub fn run(&self, sql: &str, params: &[&dyn ToSql]) -> Result<usize> {
        Ok(self.0.prepare_cached(sql)?.execute(params)?)
    }

    pub fn insert(&self, sql: &str, params: &[&dyn ToSql]) -> Result<i64> {
        self.0.prepare_cached(sql)?.execute(params)?;
        Ok(self.0.last_insert_rowid())
    }

    pub fn all(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Vec<Map<String, Value>>> {
        let mut stmt = self.0.prepare_cached(sql)?;
        let names: Vec<String> = stmt.column_names().map(|s| s.to_string()).collect();
        let mut rows = stmt.query(params)?;
        let mut out = vec![];
        while let Some(row) = rows.next()? {
            let mut m = Map::new();
            for (i, n) in names.iter().enumerate() {
                m.insert(n.clone(), to_json(row.get_ref(i)?));
            }
            out.push(m);
        }
        Ok(out)
    }

    pub fn one(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Option<Map<String, Value>>> {
        Ok(self.all(sql, params)?.into_iter().next())
    }

    pub fn exists(&self, sql: &str, params: &[&dyn ToSql]) -> Result<bool> {
        let mut stmt = self.0.prepare_cached(sql)?;
        Ok(stmt.exists(params)?)
    }

    pub fn int(&self, sql: &str, params: &[&dyn ToSql]) -> Result<i64> {
        let mut stmt = self.0.prepare_cached(sql)?;
        Ok(stmt.query_row(params, |r| r.get::<_, Option<i64>>(0))?.unwrap_or(0))
    }

    /// `log(job, kind, data)`: payload is `JSON.stringify(data ?? null)`.
    pub fn log(&self, who: Who, kind: &str, data: &Value) -> Result<i64> {
        let payload = stringify(data);
        let sha = digest(&payload);
        self.insert(
            "INSERT INTO events(ts,ticket,job,turn,kind,payload,sha256) VALUES(?,?,?,?,?,?,?)",
            &[&now(), &who.ticket, &who.id, &who.turn, &kind, &payload, &sha],
        )
    }

    /// `live(job)` (S:50-57).
    pub fn live(&self, who: Who) -> Result<bool> {
        self.exists(
            "SELECT 1 FROM jobs WHERE id=? AND ticket=? AND turn=? AND state='running'",
            &[&who.id, &who.ticket, &who.turn],
        )
    }

    /// `root(ticket)`; throws `工单不存在`.
    pub fn root(&self, ticket: i64) -> Result<Map<String, Value>> {
        self.one("SELECT * FROM jobs WHERE ticket=? AND parent IS NULL", &[&ticket])?
            .ok_or_else(|| anyhow!("工单不存在"))
    }

    /// Caller supplies a transaction so policy and manual routes can be
    /// saved atomically. Applies to subsequent collaborator dispatches.
    pub fn set_lead_routing(&self, ticket: i64, enabled: bool) -> Result<()> {
        let root = self.root(ticket)?;
        let current = self.int("SELECT allow_lead_routing FROM tickets WHERE id=?", &[&ticket])? != 0;
        if current != enabled {
            self.run("UPDATE tickets SET allow_lead_routing=? WHERE id=?", &[&(enabled as i64), &ticket])?;
            self.log(who_of(&root), "lead_routing.changed", &json!({"enabled": enabled}))?;
        }
        Ok(())
    }

    /// A `user.follow` exists in the ticket after `admission`.
    pub fn followed_after(&self, ticket: i64, admission: i64) -> Result<bool> {
        self.exists(
            "SELECT 1 FROM events WHERE ticket=? AND kind='user.follow' AND seq>? LIMIT 1",
            &[&ticket, &admission],
        )
    }

    pub fn pending_children(&self, job: i64) -> Result<bool> {
        self.exists(
            &format!("SELECT 1 FROM jobs WHERE parent=? AND state NOT IN {END_SQL} LIMIT 1"),
            &[&job],
        )
    }

    /// `interrupt(job)` (S:63-76).
    pub fn interrupt(&self, who: Who, ticket_profile: &str) -> Result<()> {
        if ticket_profile == "auto" {
            self.run(
                "UPDATE jobs SET state='queued',dispatch_turn=coalesce(dispatch_turn,turn),turn=turn+1,result=? WHERE id=?",
                &[&INTERRUPT_REASON, &who.id],
            )?;
            self.log(who, "auto.recover", &json!(INTERRUPT_REASON))?;
        } else {
            self.run(
                "UPDATE jobs SET state='interrupted',result=? WHERE id=?",
                &[&INTERRUPT_REASON, &who.id],
            )?;
            self.log(who, "interrupted", &json!(INTERRUPT_REASON))?;
        }
        Ok(())
    }

    /// Cancel `root` and every non-terminal descendant the way the
    /// operator's cancel treats a ticket (E:63-73): expire their pending
    /// approvals, set `cancelled` and log `cancelled` with `reason` on each.
    /// Returns the cancelled job ids; the caller aborts their running turns
    /// after the transaction commits.
    pub fn cancel_subtree(&self, root: i64, reason: &str) -> Result<Vec<i64>> {
        let rows = self.all(
            &format!(
                "WITH RECURSIVE sub(id) AS (SELECT ? UNION ALL SELECT j.id FROM jobs j JOIN sub ON j.parent=sub.id)
        SELECT j.id,j.ticket,j.turn FROM jobs j JOIN sub ON j.id=sub.id WHERE j.state NOT IN {END_SQL} ORDER BY j.id"
            ),
            &[&root],
        )?;
        let mut ids = vec![];
        for job in rows {
            let id = int_of(&job, "id");
            self.run(
                "UPDATE approvals SET decision='expired' WHERE job=? AND decision='pending'",
                &[&id],
            )?;
            self.run("UPDATE jobs SET state='cancelled' WHERE id=?", &[&id])?;
            self.withdraw_question(who_of(&job))?;
            self.log(who_of(&job), "cancelled", &json!(reason))?;
            ids.push(id);
        }
        Ok(ids)
    }

    /// `recover()` body (S:77-84); call inside a transaction.
    pub fn recover(&self) -> Result<()> {
        self.run("UPDATE approvals SET decision='expired' WHERE decision='pending'", &[])?;
        let rows = self.all(
            "SELECT j.id,j.ticket,j.turn,t.permission_profile AS ticket_profile FROM jobs j
        JOIN tickets t ON j.ticket=t.id WHERE j.state='running'",
            &[],
        )?;
        for r in rows {
            self.interrupt(who_of(&r), str_of(&r, "ticket_profile"))?;
        }
        Ok(())
    }

    /// `notes(id, pending)` (S:276-283): `{seq, ...payload}`.
    pub fn notes(&self, id: i64, pending: bool) -> Result<Vec<Value>> {
        let sql = format!(
            "SELECT m.seq,e.payload FROM mailbox m JOIN events e ON m.seq=e.seq WHERE m.target=?{} ORDER BY m.seq",
            if pending { " AND consumed_turn IS NULL" } else { "" }
        );
        let mut out = vec![];
        for r in self.all(&sql, &[&id])? {
            let mut m = Map::new();
            m.insert("seq".into(), r["seq"].clone());
            if let Value::Object(payload) = serde_json::from_str(str_of(&r, "payload"))? {
                for (k, v) in payload {
                    m.insert(k, v);
                }
            }
            out.push(Value::Object(m));
        }
        Ok(out)
    }

    /// `accept(job, seqs)` body (S:284-301); call inside a transaction.
    pub fn accept(&self, who: Who, seqs: &[i64]) -> Result<()> {
        self.accept_via(who, seqs, "native")
    }

    /// [`accept`](Self::accept) recording how the notes reached the turn
    /// (`native` steering or the collaboration bus `inbox`).
    pub fn accept_via(&self, who: Who, seqs: &[i64], via: &str) -> Result<()> {
        if !self.live(who)? {
            return Ok(());
        }
        let mut accepted = vec![];
        for &seq in seqs {
            let changed = self.run(
                "UPDATE mailbox SET consumed_turn=? WHERE seq=? AND target=? AND consumed_turn IS NULL",
                &[&who.turn, &seq, &who.id],
            )?;
            if changed > 0 {
                accepted.push(seq);
            }
        }
        if !accepted.is_empty() {
            self.log(who, "note.accepted", &json!({"seqs": accepted, "via": via}))?;
        }
        Ok(())
    }

    /// `recordNote(ticket, dest, body, source, key)` (S:220-272).
    pub fn record_note(
        &self,
        ticket: i64,
        dest: Option<&Value>,
        body: Option<&Value>,
        source: Option<&NoteSource>,
        key: Option<&str>,
    ) -> Result<i64> {
        self.record_note_within(ticket, dest, body, source, key, 128)
    }

    /// [`record_note`](Self::record_note) with a caller-chosen automated
    /// mailbox budget (the collaboration bus uses its own, larger budget).
    pub fn record_note_within(
        &self,
        ticket: i64,
        dest: Option<&Value>,
        body: Option<&Value>,
        source: Option<&NoteSource>,
        key: Option<&str>,
        budget: i64,
    ) -> Result<i64> {
        self.record_note_checked(ticket, dest, body, source, key, NoteDelivery { budget, answering: false })
    }

    /// A pending question can outlive an interrupted or draining turn. Its
    /// answer remains deliverable while that job is non-terminal.
    pub fn record_answer_note(&self, ticket: i64, job: i64, body: &str) -> Result<i64> {
        self.record_note_checked(
            ticket, Some(&json!(job)), Some(&json!(body)), None, None,
            NoteDelivery { budget: 128, answering: true },
        )
    }

    fn record_note_checked(
        &self,
        ticket: i64,
        dest: Option<&Value>,
        body: Option<&Value>,
        source: Option<&NoteSource>,
        key: Option<&str>,
        delivery: NoteDelivery,
    ) -> Result<i64> {
        let dest = match p::safe_int(dest) {
            Some(d) if d > 0 && p::valid_text(body, usize::MAX) => d,
            _ => return Err(crate::dispatch::rejected("收件任务或正文无效")),
        };
        let body = body.and_then(Value::as_str).unwrap_or_default();
        let turn = source.map(|s| s.logical);
        if let Some(s) = source {
            let old = self.one(
                "SELECT m.seq,e.payload FROM mailbox m JOIN events e ON e.seq=m.seq
        WHERE m.source=? AND m.source_turn=? AND m.msgkey=?",
                &[&s.who.id, &turn, &key],
            )?;
            if let Some(old) = old {
                let payload: Value = serde_json::from_str(str_of(&old, "payload"))?;
                if p::safe_int(payload.get("job")) != Some(dest)
                    || payload.get("body").and_then(Value::as_str) != Some(body)
                {
                    return Err(crate::dispatch::rejected("同一 SA_NOTE ID 的内容不一致"));
                }
                return Ok(old["seq"].as_i64().unwrap_or_default());
            }
        }
        let receiver = self.one("SELECT * FROM jobs WHERE id=? AND ticket=?", &[&dest, &ticket])?;
        let mut rejection: Option<&str> = None;
        let inbox = receiver.as_ref().is_some_and(|r| {
            if delivery.answering { !p::is_terminal(str_of(r, "state")) }
            else { matches!(str_of(r, "state"), "queued" | "running" | "waiting") }
        });
        if !inbox {
            rejection = Some("目标不属于本工单或已停止收信");
        } else if source.is_some() {
            let n = self.int(
                &format!(
                    "SELECT count(*) AS n FROM mailbox m JOIN jobs j ON j.id=m.target
      WHERE j.ticket=? AND m.source IS NOT NULL AND (NOT EXISTS(SELECT 1 FROM continuous_runs WHERE ticket=?)
      OR (m.consumed_turn IS NULL AND j.state NOT IN {END_SQL}))"
                ),
                &[&ticket, &ticket],
            )?;
            if n >= delivery.budget {
                rejection = Some("自动信箱达到预算");
            }
        }
        if source.is_none()
            && let Some(r) = rejection
        {
            return Err(anyhow!(r));
        }
        let mut payload = Map::new();
        payload.insert("job".into(), json!(dest));
        payload.insert(
            "sender".into(),
            json!(match source {
                Some(s) => format!("{} (#{})", s.agent, s.who.id),
                None => "user".into(),
            }),
        );
        payload.insert("body".into(), json!(body));
        payload.insert("sha256".into(), json!(digest(body)));
        if let Some(k) = key {
            payload.insert("id".into(), json!(k));
        }
        if let Some(r) = rejection {
            payload.insert("reason".into(), json!(r));
        }
        let who = match (source, &receiver) {
            (Some(s), _) => s.who,
            (None, Some(r)) => who_of(r),
            (None, None) => return Err(anyhow!("目标不属于本工单或已停止收信")),
        };
        let kind = if rejection.is_some() { "note.rejected" } else { "note" };
        let seq = self.log(who, kind, &Value::Object(payload))?;
        let target = if rejection.is_some() { None } else { Some(dest) };
        let source_id = source.map(|s| s.who.id);
        self.run(
            "INSERT INTO mailbox(seq,target,source,source_turn,msgkey) VALUES(?,?,?,?,?)",
            &[&seq, &target, &source_id, &turn, &key],
        )?;
        Ok(seq)
    }

    /// A question survives unrelated turns only when the agent repeats it.
    /// Call in the same transaction as the state change that closes it.
    pub fn withdraw_question(&self, who: Who) -> Result<()> {
        if let Some(row) = self.one("SELECT id FROM questions WHERE job=? AND state='pending'", &[&who.id])? {
            let id = int_of(&row, "id");
            self.run("UPDATE questions SET state='withdrawn',parked=0,closed=? WHERE id=?", &[&now(), &id])?;
            self.log(who, "ask.withdrawn", &json!({"id": id}))?;
        }
        Ok(())
    }

    /// Platform pauses share the durable question/wake contract with agent asks.
    pub(crate) fn platform_question(&self, who: Who, key: &str, question: &str, options: &[&str]) -> Result<()> {
        self.withdraw_question(who)?;
        let options = json!(options);
        let id = self.insert(
            "INSERT INTO questions(ticket,job,turn,msgkey,question,options,origin,parked,asked)
             VALUES(?,?,?,?,?,?,'platform',1,?)",
            &[&who.ticket, &who.id, &who.turn, &key, &question, &stringify(&options), &now()],
        )?;
        self.log(who, "ask", &json!({"id":id,"key":key,"question":question,"options":options,"origin":"platform"}))?;
        self.run("UPDATE jobs SET state='waiting',ready_after=0 WHERE id=?", &[&who.id])?;
        Ok(())
    }

    pub fn commit_question(&self, who: Who, ask: Option<&Value>, parked: bool) -> Result<()> {
        let pending = self.one("SELECT * FROM questions WHERE job=? AND state='pending'", &[&who.id])?;
        if let (Some(row), Some(ask)) = (&pending, ask) {
            let options: Value = serde_json::from_str(str_of(row, "options"))?;
            if row.get("msgkey") == ask.get("id") && row.get("question") == ask.get("question")
                && options == ask["options"]
            {
                self.run("UPDATE questions SET parked=? WHERE id=?", &[&i64::from(parked), &int_of(row, "id")])?;
                return Ok(());
            }
        }
        self.withdraw_question(who)?;
        if let Some(ask) = ask {
            let id = self.insert(
                "INSERT INTO questions(ticket,job,turn,msgkey,question,options,fallback,parked,asked) VALUES(?,?,?,?,?,?,?,?,?)",
                &[&who.ticket, &who.id, &who.turn, &ask["id"].as_str(), &ask["question"].as_str(),
                    &stringify(&ask["options"]), &ask.get("default").and_then(Value::as_str), &i64::from(parked), &now()],
            )?;
            self.log(who, "ask", &json!({"id":id, "key":ask["id"], "question":ask["question"],
                "options":ask["options"], "default":ask.get("default")}))?;
        }
        Ok(())
    }

    /// Failed or cancelled prerequisites, and partial isolated writers, cancel
    /// queued work before admission. A later pass propagates the cancellation
    /// along the dependency chain.
    pub fn skip_dependents(&self) -> Result<()> {
        let rows = self.all(&format!("{} SELECT j.*,n.id AS needs,n.state AS needs_state
            FROM jobs j JOIN effective_deps d ON d.job=j.id JOIN jobs n ON n.id=d.needs
            WHERE j.state='queued' AND n.state IN ('partial','failed','cancelled') AND NOT {} ORDER BY j.id,n.id",
            crate::job::EFFECTIVE_DEPS_SQL, crate::job::PREREQUISITE_SETTLED_SQL), &[])?;
        for row in rows {
            let who = who_of(&row);
            let needs = int_of(&row, "needs");
            let state = str_of(&row, "needs_state");
            let result = format!("前置任务 #{needs} 以「{state}」结束，本任务未执行。");
            if self.run("UPDATE jobs SET state='cancelled',result=? WHERE id=? AND state='queued'", &[&result, &who.id])? > 0 {
                self.withdraw_question(who)?;
                self.log(who, "skipped", &json!({"needs":needs,"state":state}))?;
            }
        }
        Ok(())
    }

    /// `wake()` (E:126-137).
    pub fn wake(&self) -> Result<()> {
        let settled =
            format!("NOT EXISTS(SELECT 1 FROM jobs c WHERE c.parent=p.id AND c.state NOT IN {END_SQL})");
        self.run(
            &format!(
                "UPDATE jobs AS p SET state='queued',turn=turn+1 WHERE state='waiting' AND (({settled} AND NOT EXISTS(SELECT 1 FROM questions a WHERE a.job=p.id AND a.state='pending' AND a.parked=1)) OR EXISTS(SELECT 1 FROM mailbox m JOIN events e ON e.seq=m.seq WHERE m.target=p.id AND m.consumed_turn IS NULL
            AND (NOT EXISTS(SELECT 1 FROM questions a WHERE a.job=p.id AND a.state='pending' AND a.origin='platform' AND a.parked=1)
              OR (m.source IS NULL AND json_extract(e.payload,'$.sender')='user'))))"
            ),
            &[],
        )?;
        for j in self.all(&format!("SELECT p.* FROM jobs p WHERE state='draining' AND {settled}"), &[])? {
            self.run("UPDATE jobs SET state='failed' WHERE id=?", &[&j["id"].as_i64()])?;
            self.withdraw_question(who_of(&j))?;
            self.log(who_of(&j), "failed", &j["result"])?;
        }
        Ok(())
    }
}

struct NoteDelivery {
    budget: i64,
    answering: bool,
}

/// Source of an agent note: the sending job and its logical turn.
pub struct NoteSource {
    pub who: Who,
    pub agent: String,
    pub logical: i64,
}

pub fn str_of<'a>(m: &'a Map<String, Value>, k: &str) -> &'a str {
    m.get(k).and_then(Value::as_str).unwrap_or_default()
}

pub fn int_of(m: &Map<String, Value>, k: &str) -> i64 {
    m.get(k).and_then(Value::as_i64).unwrap_or_default()
}

pub fn opt_int(m: &Map<String, Value>, k: &str) -> Option<i64> {
    m.get(k).and_then(Value::as_i64)
}

pub fn opt_str(m: &Map<String, Value>, k: &str) -> Option<String> {
    m.get(k).and_then(Value::as_str).map(str::to_string)
}

pub fn who_of(m: &Map<String, Value>) -> Who {
    Who {
        ticket: int_of(m, "ticket"),
        id: int_of(m, "id"),
        turn: int_of(m, "turn"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synchronous(db: &Db) -> i64 {
        db.with(|q| q.int("PRAGMA synchronous", &[])).unwrap()
    }

    #[test]
    fn telemetry_commits_are_relaxed_only_for_their_own_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        assert_eq!(synchronous(&db), 2);
        // A table of its own: events need an existing ticket and job.
        db.tx(|q| q.run("CREATE TABLE telemetry_probe(text TEXT NOT NULL)", &[])).unwrap();
        let rows = |db: &Db| db.with(|q| q.int("SELECT count(*) FROM telemetry_probe", &[])).unwrap();

        // Inside the transaction the connection does not force the WAL.
        let inside = db
            .telemetry_tx(|q| {
                q.run("INSERT INTO telemetry_probe VALUES(?)", &[&"片段"])?;
                q.int("PRAGMA synchronous", &[])
            })
            .unwrap();
        assert_eq!(inside, 1);
        assert_eq!(synchronous(&db), 2, "strict durability restored after commit");
        assert_eq!(rows(&db), 1);

        // A failed telemetry transaction rolls back and still restores it.
        let failed: Result<()> = db.telemetry_tx(|q| {
            q.run("INSERT INTO telemetry_probe VALUES(?)", &[&"丢弃"])?;
            Err(anyhow!("stop"))
        });
        assert!(failed.is_err());
        assert_eq!(synchronous(&db), 2, "strict durability restored after rollback");
        assert_eq!(rows(&db), 1);

        // Ordinary transactions never run relaxed.
        assert_eq!(db.tx(|q| q.int("PRAGMA synchronous", &[])).unwrap(), 2);
    }
}
