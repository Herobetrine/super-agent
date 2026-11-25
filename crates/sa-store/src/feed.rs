//! Read the main conversation feed. Indexed journals select the page by
//! sequence before projecting payloads. Older journals use one ordered query.

use crate::model::{DECISION_KINDS, EventView, OPEN_JOB_STATES, folded_from_feed};
use crate::progress::{Entry, VISIBLE};
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
    ticket_jobs: &'static str,
}

impl Indexes {
    pub fn detect(conn: &Connection) -> rusqlite::Result<Self> {
        let names = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index'
             AND name IN ('events_ticket_job_kind','events_ticket_kind','events_ticket','jobs_ticket_id')",
            )?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        let kind = if names.contains("events_ticket_kind") {
            "INDEXED BY events_ticket_kind"
        } else if names.contains("events_ticket") {
            "INDEXED BY events_ticket"
        } else {
            ""
        };
        let owner = names
            .contains("events_ticket_job_kind")
            .then_some("INDEXED BY events_ticket_job_kind");
        Ok(Self {
            kind,
            owner,
            ticket_jobs: if names.contains("jobs_ticket_id") {
                "INDEXED BY jobs_ticket_id"
            } else {
                ""
            },
        })
    }
}

pub(super) struct Fold {
    pub root: Option<i64>,
    pub resumed: i64,
}

struct OpenRound {
    state: String,
    turn: i64,
    on_ticket: bool,
}

impl Fold {
    fn needs_history(&self, after: i64, newer_rows: usize) -> bool {
        after < self.resumed.saturating_sub(1) && (after != 0 || newer_rows < PAGE)
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
        Ok(Fold { root, resumed })
    }

    fn open_rounds(&self, owner: &str) -> Result<HashMap<i64, OpenRound>> {
        let tickets = self.indexes.ticket_jobs;
        let states = sql_literals(OPEN_JOB_STATES);
        // Each recursive seek jumps to the next distinct non-NULL job in this
        // ticket's partial index. The anchor includes even the smallest job id.
        let sql = format!(
            "WITH RECURSIVE referenced(job) AS (
            SELECT (SELECT job FROM events {owner} WHERE ticket=?1 AND {INCLUDED}
                    AND job IS NOT NULL ORDER BY job LIMIT 1)
            UNION ALL
            SELECT (SELECT job FROM events {owner} WHERE ticket=?1 AND {INCLUDED}
                    AND job>r.job ORDER BY job LIMIT 1) FROM referenced r WHERE r.job IS NOT NULL
        ), candidates(id) AS (
            SELECT id FROM jobs {tickets} WHERE ticket=?1
            UNION SELECT job FROM referenced WHERE job IS NOT NULL
        ) SELECT j.id,j.state,j.turn,j.ticket=?1 FROM candidates c
          CROSS JOIN jobs j ON j.id=c.id WHERE j.state IN ({states})"
        );
        Ok(self
            .read(&sql, params![self.ticket], |r| {
                Ok((
                    r.get(0)?,
                    OpenRound {
                        state: r.get(1)?,
                        turn: r.get(2)?,
                        on_ticket: r.get(3)?,
                    },
                ))
            })?
            .into_iter()
            .collect())
    }

    pub fn page(&self, fold: &Fold, activity: Option<&Entry>) -> Result<Vec<EventView>> {
        if self.after >= self.end {
            return Ok(Vec::new());
        }
        let order = if self.after == 0 { "DESC" } else { "ASC" };
        let mut activity_ids = activity.map_or_else(Vec::new, |e| {
            e.newer_feed(self.after, self.end, fold.resumed).to_vec()
        });
        let mut open = HashMap::new();
        let mut events = if let Some(owner) = self.indexes.owner {
            self.indexed_page(fold, owner, order, activity_ids.len(), &mut open)?
        } else {
            self.legacy_page(fold, order)?
        };

        if let Some(entry) = activity {
            let newer =
                events.iter().filter(|e| e.seq >= fold.resumed).count() + activity_ids.len();
            if fold.needs_history(self.after, newer) {
                entry.append_history(&mut activity_ids, self.after, fold, None, None);
                let mut append = |job, state: &str, turn| {
                    if Some(job) != fold.root {
                        entry.append_history(
                            &mut activity_ids,
                            self.after,
                            fold,
                            Some(job),
                            Some((state, turn)),
                        );
                    }
                };
                if self.indexes.owner.is_some() {
                    for (job, round) in open {
                        if round.on_ticket {
                            append(job, &round.state, round.turn);
                        }
                    }
                } else {
                    // Raw ownership was resolved by legacy_page. Stream local
                    // owners directly to the supplemental activity projection.
                    let tickets = self.indexes.ticket_jobs;
                    let states = sql_literals(OPEN_JOB_STATES);
                    let sql = format!(
                        "SELECT id,state,turn FROM jobs {tickets} WHERE ticket=? AND state IN ({states})"
                    );
                    self.read(&sql, params![self.ticket], |r| {
                        append(r.get(0)?, r.get_ref(1)?.as_str()?, r.get(2)?);
                        Ok(())
                    })?;
                }
            }
            activity_ids.sort_unstable();
            events.extend(
                page_slice(&activity_ids, self.after)
                    .iter()
                    .map(|s| entry.rows[s].view(*s)),
            );
        }
        Ok(events)
    }

    fn indexed_page(
        &self,
        fold: &Fold,
        owner: &str,
        order: &str,
        newer_activity: usize,
        open: &mut HashMap<i64, OpenRound>,
    ) -> Result<Vec<EventView>> {
        let kind = self.indexes.kind;
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
        if fold.needs_history(self.after, ids.len() + newer_activity) {
            *open = self.open_rounds(owner)?;
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
                for (&job, round) in open.iter() {
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
        events.retain(|e| {
            let round = e
                .job
                .and_then(|j| open.get(&j))
                .map(|r| (r.state.as_str(), r.turn));
            !folded_from_feed(
                &e.kind,
                e.seq,
                e.job,
                e.turn,
                fold.root,
                fold.resumed,
                round,
            )
        });
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
        let kind = self.indexes.kind;
        // A legacy first attempt can precede this turn's first recorded
        // admission. Only a lower turn's admission is a safe initial bound.
        let lower = if self.after == 0 {
            let owner = self.indexes.owner.unwrap_or(kind);
            format!(
                "max(?4,coalesce((SELECT seq FROM events {owner} WHERE ticket=?1 AND job=?2
                AND kind='turn.admitted' AND {INCLUDED} AND turn<?3 AND seq<=?5
                ORDER BY seq DESC LIMIT 1),?4))"
            )
        } else {
            "?4".to_owned()
        };
        let sql = format!(
            "SELECT {COLUMNS} FROM events {kind}
            WHERE ticket=?1 AND kind='delta' AND job=?2 AND turn=?3
              AND seq>{lower}
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
