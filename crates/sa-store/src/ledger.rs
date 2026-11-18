//! Spending ledger of one ticket (architecture §7, P0A).
//!
//! Every attempt that may have consumed model usage is listed once with its
//! role (lead, collaborator, advisor), why it ran (first attempt, continuation,
//! platform retry, check or integration repair, consultation) and how it
//! ended. Usage comes from the same versioned projection as the ticket totals
//! (`usage.rs`), so the ledger never counts a sample the totals do not count.
//! Usage the journal cannot attribute is listed as a coverage gap with its
//! reason instead of being shown as zero.
//!
//! Read-only: no schema change, no model call, no price table. Money stays
//! unknown until a versioned price table exists.

use crate::usage::{Entry, RawAttempt, TokenCounts};
use anyhow::{Result, ensure};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// Classification rules of this ledger. Bump when a category changes.
pub const LEDGER_RULES_VERSION: u32 = 2;
/// Normalization rules of the underlying usage projection (v2 includes
/// advisor calls; v3 keeps each physical execution of a retried turn apart,
/// so an interrupted run's usage is no longer replaced by the retry's), the
/// same value evaluation reports carry.
pub const USAGE_RULES_VERSION: u32 = 3;
const MAX_JOBS: usize = 20_000;

/// Native harnesses whose usage frames the projection understands.
const METERED: &[&str] = &["codex", "claude", "deepseek", "antigravity"];

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LedgerAttempt {
    pub job: i64,
    /// `None` when conversation-cumulative usage cannot be split by attempt.
    pub turn: Option<i64>,
    /// Physical execution of `turn`: 0 for the first run, then one per retry
    /// that reused the turn number (operator retry of an interrupted or
    /// failed task).
    pub execution: u32,
    /// Advisor consultations carry their durable `advice:<seq>` identity.
    pub attempt_id: Option<String>,
    /// `lead`, `collaborator` or `advisor`.
    pub role: String,
    /// `initial`, `continuation`, `retry`, `repair`, `consult` or `unallocated`.
    pub kind: String,
    /// `replied`, `retried`, `failed`, `abandoned`, `interrupted`, `running`
    /// or `unknown`.
    pub outcome: String,
    pub agent: String,
    pub tokens: TokenCounts,
    pub reported: bool,
    pub partial: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LedgerBucket {
    pub role: String,
    pub kind: String,
    pub outcome: String,
    pub attempts: u64,
    pub reported_attempts: u64,
    /// Sum of reported attempts; a field is known only if every reported
    /// attempt in the bucket supplied it.
    pub tokens: TokenCounts,
    pub partial: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CoverageGap {
    pub job: Option<i64>,
    pub turn: Option<i64>,
    pub attempt_id: Option<String>,
    /// Stable reason code, see `docs/dev/usage-ledger.md`.
    pub reason: String,
    pub detail: String,
}

/// Two separate coverage ratios (§7.3): how many attempts reported usage at
/// all, and of those, how many supplied each field.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Coverage {
    pub attempts: u64,
    pub reported_attempts: u64,
    pub total_tokens_known: u64,
    pub input_tokens_known: u64,
    pub output_tokens_known: u64,
    pub cache_read_known: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Ledger {
    pub schema_version: u32,
    pub rules_version: u32,
    pub usage_rules_version: u32,
    pub ticket: i64,
    pub through_event: i64,
    /// Same totals as the ticket usage projection.
    pub totals: TokenCounts,
    pub partial: bool,
    pub coverage: Coverage,
    pub buckets: Vec<LedgerBucket>,
    pub attempts: Vec<LedgerAttempt>,
    pub gaps: Vec<CoverageGap>,
    /// Always null: no versioned price table is available, so no amount
    /// or currency is claimed.
    pub cost: Option<String>,
}

#[derive(Default)]
struct Marks {
    reply: bool,
    retry: bool,
    bounced: bool,
    failed: bool,
    gave_up: bool,
    cancelled: bool,
    interrupted: bool,
}

fn add(target: &mut Option<TokenCounts>, tokens: &TokenCounts) {
    if tokens == &TokenCounts::default() {
        return;
    }
    match target {
        Some(sum) => {
            let mut next = TokenCounts::default();
            for (out, a, b) in [
                (&mut next.total_tokens, sum.total_tokens, tokens.total_tokens),
                (&mut next.input_tokens, sum.input_tokens, tokens.input_tokens),
                (&mut next.output_tokens, sum.output_tokens, tokens.output_tokens),
                (
                    &mut next.cache_read_input_tokens,
                    sum.cache_read_input_tokens,
                    tokens.cache_read_input_tokens,
                ),
                (
                    &mut next.cache_creation_input_tokens,
                    sum.cache_creation_input_tokens,
                    tokens.cache_creation_input_tokens,
                ),
                (
                    &mut next.reasoning_output_tokens,
                    sum.reasoning_output_tokens,
                    tokens.reasoning_output_tokens,
                ),
            ] {
                *out = a.zip(b).and_then(|(a, b)| a.checked_add(b));
            }
            *sum = next;
        }
        None => *target = Some(tokens.clone()),
    }
}

/// Build the ledger of `ticket` through the newest event. Call inside a read
/// transaction so jobs, events and usage describe one journal state.
pub fn ticket_ledger(conn: &Connection, ticket: i64) -> Result<Ledger> {
    let end: i64 = conn.query_row(
        "SELECT coalesce(max(seq),0) FROM events WHERE ticket=?",
        [ticket],
        |r| r.get(0),
    )?;
    let mut stmt = conn.prepare("SELECT id,parent,agent,state FROM jobs WHERE ticket=? ORDER BY id")?;
    let jobs: Vec<(i64, Option<i64>, String, String)> = stmt
        .query_map([ticket], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    ensure!(jobs.len() <= MAX_JOBS, "工单任务数超限");
    // Marks per physical execution, counted like the usage projection: each
    // `turn.admitted` after the first of a turn starts a new execution.
    let mut marks: HashMap<(i64, i64, u32), Marks> = HashMap::new();
    let mut admissions: HashMap<(i64, i64), u32> = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT job,turn,kind FROM events WHERE ticket=? AND seq<=? AND job IS NOT NULL AND turn IS NOT NULL
         AND kind IN ('turn.admitted','reply','failed','cancelled','interrupted','turn.retry_scheduled','turn.gave_up',
                      'route.rotated','turn.route_wait','auto.retry','check.bounced','integration.bounced')
         ORDER BY seq",
    )?;
    let mut rows = stmt.query([ticket, end])?;
    while let Some(row) = rows.next()? {
        let key = (row.get::<_, i64>(0)?, row.get::<_, i64>(1)?);
        let kind: String = row.get(2)?;
        let seen = admissions.entry(key).or_default();
        if kind == "turn.admitted" {
            *seen += 1;
            continue;
        }
        let execution = seen.saturating_sub(1);
        let mark = marks.entry((key.0, key.1, execution)).or_default();
        match kind.as_str() {
            "reply" => mark.reply = true,
            "failed" => mark.failed = true,
            "turn.gave_up" => mark.gave_up = true,
            "cancelled" => mark.cancelled = true,
            "interrupted" => mark.interrupted = true,
            "check.bounced" | "integration.bounced" => mark.bounced = true,
            _ => mark.retry = true,
        }
    }
    let mut projection = Entry::default();
    let visible: Vec<(i64, String)> = vec![];
    let mut usage = projection.view(end, &visible, jobs.len() as i64);
    while usage.pending {
        projection.scan(conn, ticket, end)?;
        usage = projection.view(end, &visible, jobs.len() as i64);
    }
    let parents: HashMap<i64, Option<i64>> = jobs.iter().map(|j| (j.0, j.1)).collect();
    let mut ledger = Ledger {
        schema_version: 1,
        rules_version: LEDGER_RULES_VERSION,
        usage_rules_version: USAGE_RULES_VERSION,
        ticket,
        through_event: projection.position().min(end),
        totals: usage.totals.clone(),
        partial: usage.partial,
        ..Default::default()
    };
    let raw = projection.attempts();
    for attempt in &raw {
        ledger.attempts.push(classify(attempt, &parents, &marks, &admissions));
    }
    // Jobs that never produced a usage-relevant event still ran (or were
    // admitted); list them so the denominator is complete.
    for (id, _parent, agent, state) in &jobs {
        if !raw.iter().any(|a| a.job == *id && a.advisor.is_none()) {
            let role = if parents.get(id).copied().flatten().is_none() { "lead" } else { "collaborator" };
            ledger.gaps.push(CoverageGap {
                job: Some(*id),
                reason: "no_usage_events".into(),
                detail: format!("{agent} 任务（状态 {state}）在日志中没有任何用量事件"),
                ..Default::default()
            });
            ledger.attempts.push(LedgerAttempt {
                job: *id,
                role: role.into(),
                kind: "initial".into(),
                outcome: "unknown".into(),
                agent: agent.clone(),
                partial: true,
                ..Default::default()
            });
        }
    }
    gaps(&mut ledger, &raw);
    for (agent, _session) in projection.shared_sessions() {
        ledger.gaps.push(CoverageGap {
            reason: "shared_native_session".into(),
            detail: format!("多个任务报告了同一个 {agent} 原生会话，累计用量无法在任务之间分摊"),
            ..Default::default()
        });
    }
    if projection.advisor_partial() {
        ledger.gaps.push(CoverageGap {
            reason: "advisor_records_incomplete".into(),
            detail: "顾问调用记录缺失或无法对应到调用者".into(),
            ..Default::default()
        });
    }
    let mut buckets: BTreeMap<(String, String, String), (LedgerBucket, Option<TokenCounts>)> =
        BTreeMap::new();
    for attempt in &ledger.attempts {
        let coverage = &mut ledger.coverage;
        if attempt.kind != "unallocated" {
            coverage.attempts += 1;
            coverage.reported_attempts += u64::from(attempt.reported);
        }
        if attempt.reported {
            coverage.total_tokens_known += u64::from(attempt.tokens.total_tokens.is_some());
            coverage.input_tokens_known += u64::from(attempt.tokens.input_tokens.is_some());
            coverage.output_tokens_known += u64::from(attempt.tokens.output_tokens.is_some());
            coverage.cache_read_known += u64::from(attempt.tokens.cache_read_input_tokens.is_some());
        }
        let key = (attempt.role.clone(), attempt.kind.clone(), attempt.outcome.clone());
        let (bucket, sum) = buckets.entry(key.clone()).or_insert_with(|| {
            (
                LedgerBucket { role: key.0, kind: key.1, outcome: key.2, ..Default::default() },
                None,
            )
        });
        bucket.attempts += 1;
        bucket.partial |= attempt.partial || !attempt.reported;
        if attempt.reported {
            bucket.reported_attempts += 1;
            add(sum, &attempt.tokens);
        }
    }
    ledger.buckets = buckets
        .into_values()
        .map(|(mut bucket, sum)| {
            bucket.tokens = sum.unwrap_or_default();
            bucket
        })
        .collect();
    ledger.partial |= !ledger.gaps.is_empty();
    Ok(ledger)
}

fn classify(
    attempt: &RawAttempt,
    parents: &HashMap<i64, Option<i64>>,
    marks: &HashMap<(i64, i64, u32), Marks>,
    admissions: &HashMap<(i64, i64), u32>,
) -> LedgerAttempt {
    let last = |job: i64, turn: i64| admissions.get(&(job, turn)).copied().unwrap_or(0).saturating_sub(1);
    let role = if attempt.advisor.is_some() {
        "advisor"
    } else if parents.get(&attempt.job).copied().flatten().is_none() {
        "lead"
    } else {
        "collaborator"
    };
    let kind = match (attempt.turn, &attempt.advisor) {
        (_, Some(_)) => "consult",
        (None, None) => "unallocated",
        (Some(_), None) if attempt.execution > 0 => "retry",
        (Some(turn), None) if turn <= 1 => "initial",
        (Some(turn), None) => match marks.get(&(attempt.job, turn - 1, last(attempt.job, turn - 1))) {
            Some(previous) if previous.retry => "retry",
            Some(previous) if previous.bounced => "repair",
            _ => "continuation",
        },
    };
    let outcome = if attempt.advisor.is_some() {
        if !attempt.ended { "running" } else if attempt.failed { "failed" } else { "replied" }
    } else if let Some(turn) = attempt.turn {
        match marks.get(&(attempt.job, turn, attempt.execution)) {
            Some(m) if m.cancelled => "abandoned",
            Some(m) if m.interrupted => "interrupted",
            Some(m) if m.retry => "retried",
            Some(m) if m.failed || m.gave_up => "failed",
            Some(m) if m.reply => "replied",
            _ if attempt.ended => if attempt.failed { "failed" } else { "replied" },
            _ => "running",
        }
    } else {
        "unknown"
    };
    LedgerAttempt {
        job: attempt.job,
        turn: attempt.turn,
        execution: attempt.execution,
        attempt_id: attempt.advisor.clone(),
        role: role.into(),
        kind: kind.into(),
        outcome: outcome.into(),
        agent: attempt.agent.clone(),
        tokens: attempt.tokens.clone(),
        reported: attempt.reported,
        // A running attempt may still report more usage.
        partial: attempt.partial || (attempt.turn.is_some() && !attempt.ended),
    }
}

fn gaps(ledger: &mut Ledger, raw: &[RawAttempt]) {
    let mut seen_jobs = std::collections::HashSet::new();
    for attempt in raw {
        let gap = |reason: &str, detail: String| CoverageGap {
            job: Some(attempt.job),
            turn: attempt.turn,
            attempt_id: attempt.advisor.clone(),
            reason: reason.into(),
            detail,
        };
        if attempt.advisor.is_none() && seen_jobs.insert(attempt.job) {
            if !METERED.contains(&attempt.agent.as_str()) {
                ledger.gaps.push(gap(
                    "no_usage_protocol",
                    format!("执行端 {} 不报告可识别的用量", attempt.agent),
                ));
            }
            if attempt.agent == "claude" {
                ledger.gaps.push(gap(
                    "native_subagents_excluded",
                    "Claude 只统计主循环；原生子代理用量未单独报告".into(),
                ));
            }
            if attempt.partial {
                ledger.gaps.push(gap(
                    "partial_samples",
                    "部分用量样本缺失、回退或重复，已报告部分不含它们".into(),
                ));
            }
        }
        if attempt.turn.is_none() {
            ledger.gaps.push(gap(
                "cumulative_unallocated",
                "会话累计用量无法分到具体轮次，单列为未分配".into(),
            ));
        } else if attempt.advisor.is_some() {
            if !attempt.reported {
                ledger.gaps.push(gap("advisor_unreported", "顾问调用没有报告用量".into()));
            } else if attempt.partial {
                ledger.gaps.push(gap("advisor_partial", "顾问调用用量不完整".into()));
            }
        } else if !attempt.reported && METERED.contains(&attempt.agent.as_str()) {
            ledger.gaps.push(gap("unreported_attempt", "该次执行没有报告用量".into()));
        }
    }
}

/// Per-ticket line of a status report: totals, coverage and gap counts only,
/// without goals, prompts or replies.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct TicketLine {
    pub ticket: i64,
    pub through_event: i64,
    pub totals: TokenCounts,
    pub partial: bool,
    pub coverage: Coverage,
    pub gaps: BTreeMap<String, u64>,
}

/// Current-state spending report across tickets of one journal (P0A).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct StatusReport {
    pub schema_version: u32,
    pub rules_version: u32,
    pub usage_rules_version: u32,
    pub tickets: Vec<TicketLine>,
    /// Buckets summed over every ticket; a field is known only if every
    /// contributing bucket knew it.
    pub buckets: Vec<LedgerBucket>,
    pub coverage: Coverage,
    pub gaps: BTreeMap<String, u64>,
    pub cost: Option<String>,
    pub privacy: String,
}

/// Read the journal in `data_dir` read-only and build the status report of
/// `ticket`, or of every ticket when `None`. Never writes the journal.
pub fn status_report(data_dir: &std::path::Path, ticket: Option<i64>) -> Result<StatusReport> {
    use rusqlite::OpenFlags;
    let conn = Connection::open_with_flags(
        data_dir.join(crate::DB_FILE),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA query_only=ON; BEGIN DEFERRED;")?;
    let ids: Vec<i64> = match ticket {
        Some(id) => vec![id],
        None => {
            let mut stmt = conn.prepare("SELECT id FROM tickets ORDER BY id")?;
            stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        }
    };
    ensure!(!ids.is_empty(), "没有可汇总的工单");
    let mut report = StatusReport {
        schema_version: 1,
        rules_version: LEDGER_RULES_VERSION,
        usage_rules_version: USAGE_RULES_VERSION,
        privacy: "只含计数、执行端名称和原因代码；不含目标、提示词、回复或原生会话标识".into(),
        ..Default::default()
    };
    let mut buckets: BTreeMap<(String, String, String), (LedgerBucket, Option<TokenCounts>)> =
        BTreeMap::new();
    for id in ids {
        let ledger = ticket_ledger(&conn, id)?;
        let mut gaps = BTreeMap::new();
        for gap in &ledger.gaps {
            *gaps.entry(gap.reason.clone()).or_insert(0) += 1;
            *report.gaps.entry(gap.reason.clone()).or_insert(0) += 1;
        }
        let c = &ledger.coverage;
        let total = &mut report.coverage;
        total.attempts += c.attempts;
        total.reported_attempts += c.reported_attempts;
        total.total_tokens_known += c.total_tokens_known;
        total.input_tokens_known += c.input_tokens_known;
        total.output_tokens_known += c.output_tokens_known;
        total.cache_read_known += c.cache_read_known;
        for bucket in &ledger.buckets {
            let key = (bucket.role.clone(), bucket.kind.clone(), bucket.outcome.clone());
            let (sum, tokens) = buckets.entry(key.clone()).or_insert_with(|| {
                (
                    LedgerBucket { role: key.0, kind: key.1, outcome: key.2, ..Default::default() },
                    None,
                )
            });
            sum.attempts += bucket.attempts;
            sum.reported_attempts += bucket.reported_attempts;
            sum.partial |= bucket.partial;
            if bucket.reported_attempts > 0 {
                add(tokens, &bucket.tokens);
            }
        }
        report.tickets.push(TicketLine {
            ticket: id,
            through_event: ledger.through_event,
            totals: ledger.totals,
            partial: ledger.partial,
            coverage: ledger.coverage,
            gaps,
        });
    }
    report.buckets = buckets
        .into_values()
        .map(|(mut bucket, tokens)| {
            bucket.tokens = tokens.unwrap_or_default();
            bucket
        })
        .collect();
    conn.execute_batch("COMMIT")?;
    Ok(report)
}
