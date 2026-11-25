//! Read the main conversation feed. Indexed journals select the page by
//! sequence before projecting payloads. Older journals use one ordered query.

use crate::model::{DECISION_KINDS, EventView, OPEN_JOB_STATES, folded_from_feed};
use crate::progress::VISIBLE;
use anyhow::Result;
use rusqlite::{Connection, Row, ToSql, params};
use std::collections::{HashMap, HashSet};

pub(super) const PAGE: usize = 180;

/// Candidates are sorted chronologically for delivery in either direction.
pub(super) fn page_slice<T>(sorted: &[T], after: i64) -> &[T] {
    if after == 0 {
        &sorted[sorted.len().saturating_sub(PAGE)..]
    } else {
        &sorted[..sorted.len().min(PAGE)]
    }
}

const INCLUDED: &str = "kind NOT IN ('delta','native')";

fn sql_literals(values: &[&str]) -> String {
    values
        .iter()
        .map(|v| format!("'{v}'"))
        .collect::<Vec<_>>()
        .join(",")
}

const COLUMNS: &str = "seq,kind,ts,job,turn,CASE
      WHEN kind='reply' THEN json_object('text',substr(json_extract(payload,'$.text'),1,64000))
      WHEN kind='mcp.result' THEN json_object('id',json_extract(payload,'$.id'),'server',json_extract(payload,'$.server'),
        'tool',json_extract(payload,'$.tool'),'state',json_extract(payload,'$.state'))
      WHEN kind IN ('user','user.follow','delta','failed','cancelled','interrupted','auto.recover') THEN
        CASE WHEN json_type(payload)='text' THEN json_quote(substr(json_extract(payload,'$'),1,64000)) ELSE payload END
      ELSE payload END payload";

#[derive(Clone, Copy)]
pub(super) struct Indexes {
    kind: &'static str,
    owner: Option<&'static str>,
    open_jobs: &'static str,
}

impl Indexes {
    pub fn detect(conn: &Connection) -> rusqlite::Result<Self> {
        let names = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index'
             AND name IN ('events_ticket_job_kind','events_ticket_kind','jobs_queue','jobs_ticket_id')",
            )?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        let kind = if names.contains("events_ticket_kind") {
            "INDEXED BY events_ticket_kind"
        } else {
            // A journal predating even the per-kind index still opens read-only.
            "NOT INDEXED"
        };
        let owner = names
            .contains("events_ticket_job_kind")
            .then_some("INDEXED BY events_ticket_job_kind");
        Ok(Self {
            kind,
            owner,
            open_jobs: if owner.is_some() && names.contains("jobs_queue") {
                "INDEXED BY jobs_queue"
            } else if owner.is_none() && names.contains("jobs_ticket_id") {
                "INDEXED BY jobs_ticket_id"
            } else {
                "NOT INDEXED"
            },
        })
    }
}

pub(super) struct Fold {
    pub root: Option<i64>,
    pub resumed: i64,
    pub open: HashMap<i64, OpenRound>,
}

pub(super) struct OpenRound {
    state: String,
    turn: i64,
    pub on_ticket: bool,
}

impl Fold {
    pub fn shown(&self, kind: &str, seq: i64, job: Option<i64>, turn: Option<i64>) -> bool {
        let round = job
            .and_then(|j| self.open.get(&j))
            .filter(|r| kind != "activity" || r.on_ticket)
            .map(|r| (r.state.as_str(), r.turn));
        !folded_from_feed(kind, seq, job, turn, self.root, self.resumed, round)
    }
}

pub(super) struct Reader<'a> {
    conn: &'a Connection,
    indexes: Indexes,
    ticket: i64,
    after: i64,
    end: i64,
    #[cfg(test)]
    trace: std::cell::RefCell<Vec<tests::StatementSample>>,
}

impl<'a> Reader<'a> {
    pub fn new(conn: &'a Connection, indexes: Indexes, ticket: i64, after: i64, end: i64) -> Self {
        Self {
            conn,
            indexes,
            ticket,
            after,
            end,
            #[cfg(test)]
            trace: Default::default(),
        }
    }

    fn read<T>(
        &self,
        sql: &str,
        args: &[&dyn ToSql],
        mut map: impl FnMut(&Row<'_>) -> Result<T>,
    ) -> Result<Vec<T>> {
        let mut stmt = self.conn.prepare_cached(sql)?;
        #[cfg(test)]
        stmt.reset_status(rusqlite::StatementStatus::VmStep);
        let mut result = Vec::new();
        {
            let mut rows = stmt.query(args)?;
            while let Some(row) = rows.next()? {
                result.push(map(row)?);
            }
        }
        #[cfg(test)]
        self.record(&stmt, sql, args)?;
        Ok(result)
    }

    pub fn fold(&self, root: Option<i64>) -> Result<Fold> {
        let mut resumed = 0;
        if let Some(root) = root {
            let owner = self.indexes.owner.unwrap_or(self.indexes.kind);
            let sql = format!(
                "SELECT seq FROM events {owner} WHERE ticket=? AND job=? AND kind='turn.admitted'
                               AND {INCLUDED} AND seq<=? ORDER BY seq DESC LIMIT 1"
            );
            resumed = self
                .read(&sql, params![self.ticket, root, self.end], seq)?
                .first()
                .copied()
                .unwrap_or(0);
        }
        let mut open = HashMap::new();
        if self.after < resumed {
            let states = sql_literals(OPEN_JOB_STATES);
            let index = self.indexes.open_jobs;
            // The old raw-event query resolved an owner by job id alone.
            // Activity used this ticket's rounds. Preserve both behaviours
            // even for an imported event referring to another ticket's job.
            let referenced = if let Some(owner) = self.indexes.owner {
                format!(
                    "AND (j.ticket=?1 OR EXISTS (SELECT 1 FROM events {owner}
                    WHERE ticket=?1 AND job=j.id AND {INCLUDED}))"
                )
            } else {
                // The legacy SQL resolves raw owners itself. Only activity
                // uses this map, and it uses this ticket's rounds.
                "AND j.ticket=?1".to_owned()
            };
            let sql = format!(
                "SELECT id,state,turn,j.ticket=?1 FROM jobs AS j {index}
                WHERE state IN ({states}) {referenced}"
            );
            open.extend(self.read(&sql, params![self.ticket], |r| {
                Ok((
                    r.get(0)?,
                    OpenRound {
                        state: r.get(1)?,
                        turn: r.get(2)?,
                        on_ticket: r.get(3)?,
                    },
                ))
            })?);
        }
        Ok(Fold {
            root,
            resumed,
            open,
        })
    }

    pub fn page(&self, fold: &Fold) -> Result<Vec<EventView>> {
        if self.after >= self.end {
            return Ok(Vec::new());
        }
        let kind = self.indexes.kind;
        let order = if self.after == 0 { "DESC" } else { "ASC" };
        let Some(owner) = self.indexes.owner else {
            return self.legacy_page(fold, order);
        };
        let slice = |index: &str, extra: &str| {
            format!(
                "SELECT seq FROM events {index} WHERE ticket=?1 AND kind=?2 AND seq>?3 AND seq<=?4
             {extra} ORDER BY seq {order} LIMIT {PAGE}"
            )
        };
        let by_kind = slice(kind, "");
        let visible = || VISIBLE.iter().copied().filter(|k| *k != "delta");
        let before = fold.resumed.saturating_sub(1);
        let mut ids = Vec::new();
        for k in visible() {
            ids.extend(self.read(
                &by_kind,
                params![self.ticket, k, self.after.max(before), self.end],
                seq,
            )?);
        }
        // Only a latest-first page can be filled entirely by the newer range.
        if self.after < before && (self.after != 0 || ids.len() < PAGE) {
            let owned = format!("AND {INCLUDED} AND job IS ?5");
            let by_owner = slice(owner, &owned);
            let by_round = slice(owner, &format!("{owned} AND (turn IS NULL OR turn>=?6)"));
            for k in visible() {
                if DECISION_KINDS.contains(&k) {
                    ids.extend(self.read(
                        &by_kind,
                        params![self.ticket, k, self.after, before],
                        seq,
                    )?);
                    continue;
                }
                for job in [None, fold.root] {
                    ids.extend(self.read(
                        &by_owner,
                        params![self.ticket, k, self.after, before, job],
                        seq,
                    )?);
                }
                for (&job, round) in &fold.open {
                    if Some(job) != fold.root {
                        ids.extend(self.read(
                            &by_round,
                            params![self.ticket, k, self.after, before, job, round.turn],
                            seq,
                        )?);
                    }
                }
            }
        }
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        ids.sort_unstable();
        let ids = page_slice(&ids, self.after);
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql =
            format!("SELECT {COLUMNS} FROM events WHERE seq IN ({placeholders}) ORDER BY seq");
        let args = ids.iter().map(|id| id as &dyn ToSql).collect::<Vec<_>>();
        let mut events = self.events(&sql, &args)?;
        events.retain(|e| fold.shown(&e.kind, e.seq, e.job, e.turn));
        Ok(events)
    }

    fn legacy_page(&self, fold: &Fold, order: &str) -> Result<Vec<EventView>> {
        let visible = VISIBLE
            .iter()
            .copied()
            .filter(|k| *k != "delta")
            .collect::<Vec<_>>();
        let kinds = sql_literals(&visible);
        let decisions = sql_literals(DECISION_KINDS);
        let open = sql_literals(OPEN_JOB_STATES);
        // The pre-index fold from 2fd0831. Resolve an event's owner by id,
        // including owners outside this ticket, before applying the limit.
        let sql = format!("SELECT page.*,owner.state,owner.turn FROM (
            SELECT {COLUMNS} FROM events WHERE ticket=?1 AND seq>?2 AND seq<=?3 AND kind IN ({kinds})
            AND NOT (job IS NOT NULL AND job IS NOT ?4 AND ?4 IS NOT NULL AND kind NOT IN ({decisions}) AND seq<?5
              AND coalesce((SELECT pj.state NOT IN ({open}) OR coalesce(events.turn<pj.turn,0) FROM jobs pj WHERE pj.id=events.job),1))
            ORDER BY seq {order} LIMIT {PAGE}) AS page LEFT JOIN jobs AS owner ON owner.id=page.job");
        // Recheck the bounded page with the shared rule, using the actual
        // owner even when the event references another ticket's job.
        let rows = self.read(
            &sql,
            params![self.ticket, self.after, self.end, fold.root, fold.resumed],
            |r| {
                let event = event(r)?;
                let state: Option<String> = r.get(6)?;
                let round = state.as_deref().zip(r.get::<_, Option<i64>>(7)?);
                let folded = folded_from_feed(
                    &event.kind,
                    event.seq,
                    event.job,
                    event.turn,
                    fold.root,
                    fold.resumed,
                    round,
                );
                Ok((!folded).then_some(event))
            },
        )?;
        Ok(rows.into_iter().flatten().collect())
    }

    pub fn deltas(&self, job: i64, turn: i64, cursor: i64) -> Result<Vec<EventView>> {
        // Admission commits before the adapter starts. A same-turn retry
        // keeps the first attempt's deltas; old journals may lack admission.
        let kind = self.indexes.kind;
        let admission = if let Some(owner) = self.indexes.owner {
            // A job's turn advances monotonically; retries retain its turn.
            // Seek backwards over the retries, then forwards from the preceding
            // turn's admission to the first admission of this turn.
            let range = format!(
                "FROM events {owner} WHERE ticket=?1 AND job=?2 AND kind='turn.admitted'
                AND {INCLUDED} AND seq<=?5"
            );
            format!(
                "SELECT min(seq) {range} AND turn=?3 AND seq>=coalesce(
                (SELECT seq {range} AND turn<>?3 ORDER BY seq DESC LIMIT 1),{first_seq})",
                first_seq = i64::MIN
            )
        } else {
            format!(
                "SELECT min(seq) FROM events {kind}
                WHERE ticket=?1 AND kind='turn.admitted' AND job=?2 AND turn=?3 AND seq<=?5"
            )
        };
        let sql = format!(
            "SELECT {COLUMNS} FROM events {kind}
            WHERE ticket=?1 AND kind='delta' AND job=?2 AND turn=?3
              AND seq>max(?4,coalesce(({admission}),?4))
              AND seq<=?5 ORDER BY seq"
        );
        self.events(&sql, params![self.ticket, job, turn, self.after, cursor])
    }

    fn events(&self, sql: &str, args: &[&dyn ToSql]) -> Result<Vec<EventView>> {
        self.read(sql, args, event)
    }
}

fn event(r: &Row<'_>) -> Result<EventView> {
    Ok(EventView {
        seq: r.get(0)?,
        kind: r.get(1)?,
        ts: r.get(2)?,
        job: r.get(3)?,
        turn: r.get(4)?,
        agent: None,
        payload: super::store::json(r.get_ref(5)?.as_str()?)?,
    })
}

fn seq(r: &Row<'_>) -> Result<i64> {
    Ok(r.get(0)?)
}

#[cfg(test)]
mod tests;
