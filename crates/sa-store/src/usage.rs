//! Read-only token projection from the native journal, without schema changes.
//!
//! Contracts checked on 2026-11-05: Codex 0.166.0 generated v2 schema
//! (`thread/tokenUsage/updated`), Claude Code 2.1.317 stream-json, DSH
//! 0.2.1-rc.5 headless `json-stream` and `dsh-llm` TokenUsage, agy 1.5.0.
//! Sources: https://learn.chatgpt.com/docs/app-server,
//! https://code.claude.com/docs/en/agent-sdk/cost-tracking,
//! https://antigravity.google/docs/cli/headless/,
//! https://github.com/deepseek-ai/deepseek-harness.
//!
//! Claude's result.usage is per input turn and excludes native subagents.
//! Its assistant output_tokens is a placeholder, so it is never counted.
//! DSH step usage includes native retries and uses disjoint input/cache
//! buckets. Codex totals are cumulative per thread. agy step usage is
//! per step, while result usage is cumulative across the conversation.

use anyhow::Result;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Maximum journal rows and native payload bytes examined per refresh.
pub const USAGE_SCAN_BATCH: usize = 256;
pub const USAGE_PAYLOAD_LIMIT: usize = 262_144;

/// Reported counts. An aggregate field is known only when every reported
/// contribution supplies it. Entirely unreported attempts use `partial`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct TokenCounts {
    pub total_tokens: Option<u64>,
    /// All input, including caches, only when its meaning can be normalized.
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    /// A subset of output when reported; never added to the total again.
    pub reasoning_output_tokens: Option<u64>,
}

impl TokenCounts {
    fn add(&mut self, other: &Self) {
        fn sum(a: &mut Option<u64>, b: Option<u64>) {
            *a = a.zip(b).and_then(|(a, b)| a.checked_add(b));
        }
        sum(&mut self.total_tokens, other.total_tokens);
        sum(&mut self.input_tokens, other.input_tokens);
        sum(&mut self.output_tokens, other.output_tokens);
        sum(
            &mut self.cache_read_input_tokens,
            other.cache_read_input_tokens,
        );
        sum(
            &mut self.cache_creation_input_tokens,
            other.cache_creation_input_tokens,
        );
        sum(
            &mut self.reasoning_output_tokens,
            other.reasoning_output_tokens,
        );
    }

    fn any(&self) -> bool {
        self != &Self::default()
    }

    fn positive(&self) -> bool {
        [
            self.total_tokens,
            self.input_tokens,
            self.output_tokens,
            self.cache_read_input_tokens,
            self.cache_creation_input_tokens,
        ]
        .into_iter()
        .flatten()
        .any(|v| v > 0)
    }

    fn delta(&self, old: &Self) -> Self {
        fn difference(new: Option<u64>, old: Option<u64>) -> Option<u64> {
            new.zip(old).map(|(n, o)| n.saturating_sub(o))
        }
        Self {
            total_tokens: difference(self.total_tokens, old.total_tokens),
            input_tokens: difference(self.input_tokens, old.input_tokens),
            output_tokens: difference(self.output_tokens, old.output_tokens),
            cache_read_input_tokens: difference(
                self.cache_read_input_tokens,
                old.cache_read_input_tokens,
            ),
            cache_creation_input_tokens: difference(
                self.cache_creation_input_tokens,
                old.cache_creation_input_tokens,
            ),
            reasoning_output_tokens: difference(
                self.reasoning_output_tokens,
                old.reasoning_output_tokens,
            ),
        }
    }
}

#[derive(Clone, Default)]
struct Sum(Option<TokenCounts>);

impl Sum {
    fn add(&mut self, tokens: &TokenCounts) {
        if !tokens.any() {
            return;
        }
        match &mut self.0 {
            Some(total) => total.add(tokens),
            None => self.0 = Some(tokens.clone()),
        }
    }

    fn finish(self) -> TokenCounts {
        self.0.unwrap_or_default()
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UsageScope {
    #[default]
    Harness,
    MainLoop,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct JobUsage {
    pub job: i64,
    pub agent: String,
    pub tokens: TokenCounts,
    pub reported_attempts: u64,
    pub unreported_attempts: u64,
    pub partial: bool,
    pub scope: UsageScope,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct TicketUsage {
    pub totals: TokenCounts,
    /// The snapshot's visible jobs; totals cover every scanned job.
    pub jobs: Vec<JobUsage>,
    pub unreported_jobs: u64,
    pub partial: bool,
    /// More journal rows remain. Refreshing continues the bounded scan.
    pub pending: bool,
    pub cursor: i64,
}

#[derive(Default)]
struct Attempt {
    reported: bool,
    ended: bool,
    failed: bool,
    claude_result: Option<TokenCounts>,
    claude_messages: HashMap<String, TokenCounts>,
}

#[derive(Default)]
struct AgySession {
    total: Option<TokenCounts>,
    pending: HashMap<String, TokenCounts>,
    accounted: HashSet<String>,
}

#[derive(Default)]
struct JobState {
    agent: String,
    attempts: BTreeMap<i64, Attempt>,
    partial: bool,
    units: HashMap<String, TokenCounts>,
    codex_baselines: HashMap<String, TokenCounts>,
    codex_total: Sum,
    claude_message_owners: HashMap<String, i64>,
    agy: HashMap<String, AgySession>,
    session: String,
}

fn number(v: &Value, key: &str) -> Option<u64> {
    // Match the JS exact integer range and reject strings, negatives and floats.
    v.get(key)?.as_u64().filter(|n| *n <= 9_007_199_254_740_991)
}

fn identity(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() && s.len() <= 256 => Some(s.clone()),
        Value::Number(n) if n.as_u64().is_some() => Some(n.to_string()),
        _ => None,
    }
}

fn full_input(input: Option<u64>, read: Option<u64>, write: Option<u64>) -> Option<u64> {
    input?.checked_add(read?)?.checked_add(write?)
}

fn codex(v: &Value) -> TokenCounts {
    TokenCounts {
        total_tokens: number(v, "totalTokens"),
        input_tokens: number(v, "inputTokens"),
        output_tokens: number(v, "outputTokens"),
        cache_read_input_tokens: number(v, "cachedInputTokens"),
        cache_creation_input_tokens: number(v, "cacheWriteInputTokens"),
        reasoning_output_tokens: number(v, "reasoningOutputTokens"),
    }
}

fn claude(v: &Value, output: bool) -> TokenCounts {
    let read = number(v, "cache_read_input_tokens");
    let write = number(v, "cache_creation_input_tokens");
    let input = full_input(number(v, "input_tokens"), read, write);
    let output = output.then(|| number(v, "output_tokens")).flatten();
    TokenCounts {
        total_tokens: input.zip(output).and_then(|(i, o)| i.checked_add(o)),
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: read,
        cache_creation_input_tokens: write,
        reasoning_output_tokens: None,
    }
}

fn deepseek(v: &Value) -> TokenCounts {
    let output = number(v, "outputTokens");
    let total = number(v, "totalTokens");
    let read = number(v, "cacheReadTokens");
    let write = number(v, "cacheWriteTokens");
    let input = total
        .zip(output)
        .and_then(|(t, o)| t.checked_sub(o))
        .or_else(|| full_input(number(v, "inputTokens"), read, write));
    TokenCounts {
        total_tokens: total.or_else(|| input.zip(output).and_then(|(i, o)| i.checked_add(o))),
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: read,
        cache_creation_input_tokens: write,
        reasoning_output_tokens: number(v, "reasoningTokens"),
    }
}

fn agy(v: &Value) -> TokenCounts {
    TokenCounts {
        total_tokens: number(v, "total_tokens"),
        // agy's public protocol does not promise whether input includes
        // cache reads across every backend. Preserve its total, not a guess.
        input_tokens: None,
        output_tokens: number(v, "output_tokens"),
        cache_read_input_tokens: number(v, "cache_read_tokens"),
        cache_creation_input_tokens: None,
        reasoning_output_tokens: number(v, "thinking_tokens"),
    }
}

impl JobState {
    fn native(&mut self, turn: i64, v: &Value) {
        match self.agent.as_str() {
            "codex" => self.codex(turn, v),
            "claude" => self.claude(turn, v),
            "deepseek" => self.deepseek(turn, v),
            "antigravity" => self.agy(turn, v),
            _ => {}
        }
    }

    fn codex(&mut self, turn: i64, v: &Value) {
        if v["method"] != "thread/tokenUsage/updated" {
            return;
        }
        let p = &v["params"];
        let Some(thread) = identity(&p["threadId"]) else {
            self.partial = true;
            return;
        };
        let next = codex(&p["tokenUsage"]["total"]);
        let last = codex(&p["tokenUsage"]["last"]);
        if next.total_tokens.is_none() || !last.any() {
            self.partial = true;
            return;
        }
        let delta = match self.codex_baselines.get(&thread) {
            Some(previous) => {
                if next.total_tokens < previous.total_tokens {
                    // A delayed older frame or rollback has no trustworthy
                    // delta. Keep the high-water mark so its replay cannot
                    // bill the same consumption again.
                    self.partial = true;
                    return;
                }
                if next.total_tokens == previous.total_tokens {
                    self.attempts.entry(turn).or_default().reported = true;
                    return;
                }
                next.delta(previous)
            }
            None => {
                // Existing upstream history may predate this journal. Count
                // only the last observed request, and expose missing coverage.
                if next.total_tokens != last.total_tokens {
                    self.partial = true;
                }
                last
            }
        };
        self.codex_baselines.insert(thread, next);
        self.codex_total.add(&delta);
        self.attempts.entry(turn).or_default().reported = true;
    }

    fn claude(&mut self, turn: i64, v: &Value) {
        // result.usage and assistant messages cover the main loop only.
        // modelUsage includes native children, but changed resume semantics
        // in 2.1.277 and cannot safely be inferred for unversioned old logs.
        if !v["parent_tool_use_id"].is_null() {
            return;
        }
        if v["type"] == "assistant" {
            let Some(id) = identity(&v["message"]["id"]) else {
                return;
            };
            let tokens = claude(&v["message"]["usage"], false);
            if tokens.any() {
                let owner = *self.claude_message_owners.entry(id.clone()).or_insert(turn);
                let attempt = self.attempts.entry(owner).or_default();
                attempt.claude_messages.insert(id, tokens);
                attempt.reported = true;
            }
        } else if v["type"] == "result" {
            let tokens = claude(&v["usage"], true);
            let attempt = self.attempts.entry(turn).or_default();
            attempt.ended = true;
            if v["is_error"] == true {
                attempt.failed = true;
            }
            if tokens.any() && !(v["subtype"] == "error_during_execution" && !tokens.positive()) {
                attempt.reported = true;
                attempt.claude_result = Some(tokens);
            }
        }
    }

    fn deepseek(&mut self, turn: i64, v: &Value) {
        if v["type"] == "session" {
            if let Some(session) = identity(&v["sessionId"]) {
                self.session = session;
            }
            return;
        }
        if v["type"] != "status" || v["phase"] != "step_end" {
            return;
        }
        let (Some(native_turn), Some(step)) = (identity(&v["turn"]), identity(&v["step"])) else {
            self.partial = true;
            return;
        };
        let tokens = deepseek(&v["usage"]);
        if !tokens.any() {
            // The headless projector deliberately omits usage if any native
            // retry omitted a sample. This is unknown, even after success.
            self.partial = true;
            return;
        }
        self.partial |= tokens.total_tokens.is_none();
        let session = if self.session.is_empty() {
            format!("attempt-{turn}")
        } else {
            self.session.clone()
        };
        self.units
            .insert(format!("{session}/{native_turn}/{step}"), tokens);
        self.attempts.entry(turn).or_default().reported = true;
    }

    fn agy(&mut self, turn: i64, v: &Value) {
        let (value, final_result) = match v["event"].as_str() {
            Some("step_update") => (&v["step_update"], false),
            Some("result") => (&v["result"], true),
            _ => return,
        };
        let Some(session_id) = identity(&value["conversation_id"]) else {
            return;
        };
        let tokens = agy(&value["usage"]);
        if !tokens.any() {
            return;
        }
        self.partial |= tokens.total_tokens.is_none();
        let session = self.agy.entry(session_id).or_default();
        if final_result {
            let older = session.total.as_ref();
            if older.is_some_and(|old| old.total_tokens > tokens.total_tokens) {
                self.partial = true;
                return;
            }
            if older != Some(&tokens) {
                if older.is_none() && session.pending.is_empty() && tokens.positive() {
                    // Terminal-only history cannot establish its initial baseline.
                    self.partial = true;
                }
                session.total = Some(tokens);
                session
                    .accounted
                    .extend(session.pending.drain().map(|(id, _)| id));
            }
        } else if let Some(step) = identity(&value["step_index"])
            && !session.accounted.contains(&step)
        {
            session.pending.insert(step, tokens);
        }
        self.attempts.entry(turn).or_default().reported = true;
    }

    fn view(&self, job: i64) -> JobUsage {
        let mut tokens = self.codex_total.clone();
        for unit in self.units.values() {
            tokens.add(unit);
        }
        for attempt in self.attempts.values() {
            if let Some(result) = &attempt.claude_result {
                tokens.add(result);
            } else {
                for message in attempt.claude_messages.values() {
                    tokens.add(message);
                }
            }
        }
        for session in self.agy.values() {
            if let Some(total) = &session.total {
                tokens.add(total);
            }
            for step in session.pending.values() {
                tokens.add(step);
            }
        }
        let reported = self.attempts.values().filter(|a| a.reported).count() as u64;
        let unreported = self.attempts.len() as u64 - reported;
        JobUsage {
            job,
            agent: self.agent.clone(),
            tokens: tokens.finish(),
            reported_attempts: reported,
            unreported_attempts: unreported,
            partial: self.partial
                || unreported > 0
                || self.agent == "claude"
                || self.attempts.values().any(|a| !a.ended || a.failed),
            scope: if self.agent == "claude" {
                UsageScope::MainLoop
            } else {
                UsageScope::Harness
            },
        }
    }
}

/// Retains compact counters and deduplication identities, never raw payloads.
#[derive(Default)]
pub(crate) struct Entry {
    seq: i64,
    jobs: BTreeMap<i64, JobState>,
}

impl Entry {
    pub(crate) fn scan(&mut self, conn: &Connection, ticket: i64, end: i64) -> Result<()> {
        // events_ticket(ticket,seq) supplies the forward-only cursor. Size
        // guard runs in SQLite before Rust allocates or parses a payload.
        let mut stmt = conn.prepare_cached(
            "SELECT e.seq,e.job,e.turn,j.agent,e.kind,
             CASE WHEN e.kind='native' AND length(CAST(e.payload AS BLOB))<=?4 THEN e.payload END
             FROM events e LEFT JOIN jobs j ON j.id=e.job
             WHERE e.ticket=?1 AND e.seq>?2 AND e.seq<=?3
             AND e.kind IN ('native','turn.admitted','reply','failed','cancelled','interrupted','turn.retry_scheduled','turn.gave_up')
             ORDER BY e.seq LIMIT ?5",
        )?;
        let mut rows = stmt.query(params![
            ticket,
            self.seq,
            end,
            USAGE_PAYLOAD_LIMIT as i64,
            USAGE_SCAN_BATCH as i64
        ])?;
        let mut count = 0;
        while let Some(row) = rows.next()? {
            count += 1;
            self.seq = row.get(0)?;
            let (Some(job), Some(turn), Some(agent)) = (
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ) else {
                continue;
            };
            let kind: String = row.get(4)?;
            let state = self.jobs.entry(job).or_insert_with(|| JobState {
                agent,
                ..Default::default()
            });
            let attempt = state.attempts.entry(turn).or_default();
            match kind.as_str() {
                "reply" => attempt.ended = true,
                "failed"
                | "cancelled"
                | "interrupted"
                | "turn.retry_scheduled"
                | "turn.gave_up" => {
                    attempt.ended = true;
                    attempt.failed = true;
                }
                "native" => match row.get::<_, Option<String>>(5)? {
                    Some(text) => match serde_json::from_str(&text) {
                        Ok(value) => state.native(turn, &value),
                        Err(_) => state.partial = true,
                    },
                    None => state.partial = true,
                },
                _ => {}
            }
        }
        if count < USAGE_SCAN_BATCH {
            self.seq = end;
        }
        Ok(())
    }

    pub(crate) fn view(&self, end: i64, visible: &[(i64, String)], total_jobs: i64) -> TicketUsage {
        let mut totals = Sum::default();
        let mut partial = self.seq < end;
        let mut reported_jobs = 0;
        let mut jobs = Vec::new();
        for (id, state) in &self.jobs {
            let view = state.view(*id);
            totals.add(&view.tokens);
            reported_jobs += u64::from(view.reported_attempts > 0);
            partial |= view.partial;
            if visible.iter().any(|(visible_id, _)| id == visible_id) {
                jobs.push(view);
            }
        }
        for (id, agent) in visible {
            if !self.jobs.contains_key(id) {
                jobs.push(JobUsage {
                    job: *id,
                    agent: agent.clone(),
                    partial: true,
                    ..Default::default()
                });
            }
        }
        jobs.sort_by_key(|j| j.job);
        let unreported_jobs = (total_jobs.max(0) as u64).saturating_sub(reported_jobs);
        TicketUsage {
            totals: totals.finish(),
            jobs,
            unreported_jobs,
            partial: partial || unreported_jobs > 0,
            pending: self.seq < end,
            cursor: self.seq,
        }
    }
}
