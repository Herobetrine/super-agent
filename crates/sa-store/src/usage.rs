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
use serde_json::{Value, json};
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
    /// Cache reads divided by all input, including cache reads and writes.
    /// Unknown or inconsistent buckets are not interpreted as a zero hit rate.
    pub fn cache_hit_rate(&self) -> Option<f64> {
        let (read, input) = self.cache_read_input_tokens.zip(self.input_tokens)?;
        (input > 0 && read <= input).then(|| read as f64 / input as f64)
    }

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

/// Output from execution intervals whose usage and journal times can be paired.
/// Durations include startup, tools and waits inside an execution. Summing
/// overlapping workers' durations yields a duration-weighted execution rate,
/// not aggregate wall-clock throughput or the model's decoder speed.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UsagePerformance {
    pub output_tokens: u64,
    pub elapsed_ms: u64,
    pub samples: u64,
    pub partial: bool,
}

impl UsagePerformance {
    pub fn output_tokens_per_second(&self) -> Option<f64> {
        (self.samples > 0 && self.elapsed_ms > 0)
            .then(|| self.output_tokens as f64 * 1000.0 / self.elapsed_ms as f64)
    }

    fn add(&mut self, other: &Self) {
        self.partial |= other.partial;
        match (
            self.output_tokens.checked_add(other.output_tokens),
            self.elapsed_ms.checked_add(other.elapsed_ms),
            self.samples.checked_add(other.samples),
        ) {
            (Some(output), Some(elapsed), Some(samples)) => {
                self.output_tokens = output;
                self.elapsed_ms = elapsed;
                self.samples = samples;
            }
            _ => self.partial = true,
        }
    }
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
    pub performance: UsagePerformance,
    /// The journal shows native subagents ran; their usage is not counted.
    pub native_subagents: bool,
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
    pub performance: UsagePerformance,
    /// Independent read-only consultations, attributed to their calling job.
    pub advisor_calls: Vec<AdvisorUsage>,
    pub reported_advisor_calls: u64,
    pub unreported_advisor_calls: u64,
    /// Native sessions reported by more than one job. Their cumulative
    /// counters cannot be split between the jobs, so totals stay partial.
    pub shared_native_sessions: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AdvisorUsage {
    pub attempt_id: String,
    pub job: i64,
    pub turn: i64,
    pub agent: String,
    pub tokens: TokenCounts,
    pub reported: bool,
    pub partial: bool,
    pub scope: UsageScope,
    pub performance: UsagePerformance,
    pub native_subagents: bool,
}

/// Retain only protocol fields required by the existing native projector.
/// Text, tool arguments, prompts and unrelated metadata never leave memory.
pub fn advisor_usage_sample(agent: &str, value: &Value) -> Option<Value> {
    fn counts(value: &Value, keys: &[&str]) -> Value {
        keys.iter()
            .map(|key| ((*key).to_string(), json!(number(value, key))))
            .collect()
    }
    let camel = [
        "totalTokens",
        "inputTokens",
        "outputTokens",
        "cachedInputTokens",
        "cacheWriteInputTokens",
        "reasoningOutputTokens",
    ];
    let snake = [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ];
    match agent {
        "codex" if value["method"] == "thread/tokenUsage/updated" => {
            let p = &value["params"];
            Some(json!({"method":"thread/tokenUsage/updated", "params":{
                "threadId":identity(&p["threadId"]), "tokenUsage":{
                    "total":counts(&p["tokenUsage"]["total"], &camel),
                    "last":counts(&p["tokenUsage"]["last"], &camel)}}}))
        }
        "claude" if value["type"] == "system" && value["subtype"] == "init" => {
            Some(json!({"type":"system", "subtype":"init", "model":model_id(&value["model"])}))
        }
        "claude" if value["type"] == "assistant" || value["type"] == "result" => {
            if !value["parent_tool_use_id"].is_null() {
                // Only the fact that a native subagent ran, never its id.
                return (value["type"] == "assistant")
                    .then(|| json!({"type":"subagent", "parent_tool_use_id":true}));
            }
            let usage = |usage: &Value| {
                let mut sample = counts(usage, &snake);
                sample["cache_creation"] = counts(
                    &usage["cache_creation"],
                    &["ephemeral_5m_input_tokens", "ephemeral_1h_input_tokens"],
                );
                for key in ["speed", "inference_geo"] {
                    if let Some(mode) = usage[key].as_str().filter(|s| s.len() <= 32) {
                        sample[key] = json!(mode);
                    }
                }
                sample
            };
            if value["type"] == "assistant" {
                Some(json!({"type":"assistant", "subagent_tool":spawns_subagent(&value["message"]["content"]),
                    "message":{"id":identity(&value["message"]["id"]),
                    "model":model_id(&value["message"]["model"]),
                    "usage":usage(&value["message"]["usage"])}}))
            } else {
                Some(
                    json!({"type":"result", "is_error":value["is_error"] == true,
                    "subtype":if value["subtype"] == "error_during_execution" { "error_during_execution" } else { "result" },
                    "usage":usage(&value["usage"])}),
                )
            }
        }
        "deepseek" if value["type"] == "session" => {
            Some(json!({"type":"session", "sessionId":identity(&value["sessionId"])}))
        }
        "deepseek" if value["type"] == "status" && value["phase"] == "step_end" => Some(
            json!({"type":"status", "phase":"step_end", "turn":identity(&value["turn"]),
                "step":identity(&value["step"]), "usage":counts(&value["usage"],
                    &["totalTokens", "inputTokens", "outputTokens", "cacheReadTokens", "cacheWriteTokens", "reasoningTokens"])}),
        ),
        "antigravity" => {
            let kind = match value["event"].as_str()? {
                "step_update" => "step_update",
                "result" => "result",
                _ => return None,
            };
            let sample = &value[kind];
            Some(
                json!({"event":kind, (kind):{"conversation_id":identity(&sample["conversation_id"]),
                "step_index":identity(&sample["step_index"]), "usage":counts(&sample["usage"],
                    &["total_tokens", "output_tokens", "cache_read_tokens", "thinking_tokens"])}}),
            )
        }
        _ => None,
    }
}

#[derive(Default)]
struct Attempt {
    reported: bool,
    ended: bool,
    failed: bool,
    claude_result: Option<TokenCounts>,
    claude_messages: HashMap<String, TokenCounts>,
    started_ms: Option<u64>,
    ended_ms: Option<u64>,
    sampled_ms: Option<u64>,
    output_tokens: Option<u64>,
    timing_invalid: bool,
    /// Model reported by this execution's own native frames, with its source
    /// (`native_init` or `native_message`).
    model: Option<(String, &'static str)>,
    /// Lifetime split of the Claude result's cache writes.
    claude_result_ttl: Option<(u64, u64)>,
    /// Largest single request prompt seen (Codex), and whether any frame
    /// left it unknown.
    request_input_bound: Option<u64>,
    request_input_unknown: bool,
    nonstandard_mode: bool,
}

impl Attempt {
    fn output_change(&mut self, previous: Option<u64>, next: Option<u64>, at: Option<u64>) {
        let (Some(previous), Some(next)) = (previous, next) else {
            self.timing_invalid = true;
            return;
        };
        if previous == next && self.output_tokens.is_some() {
            return;
        }
        let output = self.output_tokens.unwrap_or(0);
        self.output_tokens = if next >= previous {
            output.checked_add(next - previous)
        } else {
            output.checked_sub(previous - next)
        };
        if self.output_tokens.is_none()
            || at.is_none()
            || self.started_ms.is_none()
            || at < self.started_ms
            || at < self.sampled_ms
            || self.ended_ms.is_some()
        {
            self.timing_invalid = true;
        }
        self.sampled_ms = at;
    }

    fn finish(&mut self, at: Option<u64>) {
        if self.ended_ms.is_none() {
            self.ended_ms = at;
            self.timing_invalid |= at.is_none() || at < self.started_ms || at < self.sampled_ms;
        }
    }

    fn performance(&self) -> UsagePerformance {
        let sample = self
            .output_tokens
            .zip(self.started_ms)
            .and_then(|(output, start)| {
                self.ended_ms
                    .or(self.sampled_ms)
                    .and_then(|end| end.checked_sub(start))
                    .filter(|duration| *duration > 0)
                    .map(|duration| (output, duration))
            });
        match sample {
            Some((output_tokens, elapsed_ms)) if !self.timing_invalid && !self.failed => {
                UsagePerformance {
                    output_tokens,
                    elapsed_ms,
                    samples: 1,
                    // A running interval is sampled only through its latest report.
                    partial: self.ended_ms.is_none(),
                }
            }
            _ => UsagePerformance {
                partial: true,
                ..Default::default()
            },
        }
    }
}

#[derive(Default)]
struct AgySession {
    total: Option<TokenCounts>,
    pending: HashMap<String, (i64, TokenCounts)>,
    accounted: HashSet<String>,
}

#[derive(Default)]
struct JobState {
    agent: String,
    attempts: BTreeMap<i64, Attempt>,
    partial: bool,
    units: HashMap<String, (i64, TokenCounts)>,
    codex_baselines: HashMap<String, TokenCounts>,
    codex_total: Sum,
    /// The same Codex deltas, keyed by the attempt that observed them.
    codex_turns: BTreeMap<i64, Sum>,
    claude_message_owners: HashMap<String, i64>,
    agy: HashMap<String, AgySession>,
    session: String,
    /// Job attempts are keyed by physical execution ("slot"), not by logical
    /// turn: an operator retry reuses the turn number but starts a new native
    /// run, whose usage and outcome must not merge with the earlier run.
    /// slot -> (logical turn, execution index within the turn).
    slots: BTreeMap<i64, (i64, u32)>,
    /// Logical turn -> (current slot, admissions seen).
    current: HashMap<i64, (i64, u32)>,
    /// The journal shows native subagent activity (a frame with a
    /// `parent_tool_use_id`, or a `Task`/`Agent` tool use) whose usage the
    /// main-loop counters exclude.
    subagents: bool,
    /// Route evidence per slot: the slot's own `route.requested`, else the
    /// route in force when the slot began (`route_carried`/`route_rotated`).
    routes: HashMap<i64, (RouteRef, &'static str)>,
    in_force: Option<(RouteRef, &'static str)>,
    /// A `route.rotated` changed the job's route, so the job-level route
    /// no longer describes earlier executions.
    rotated: bool,
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

/// A model id as reported by a native frame or a route. Synthetic or
/// malformed values are not evidence.
fn model_id(v: &Value) -> Option<String> {
    v.as_str()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-._:/@[]".contains(&c))
        })
        .map(str::to_string)
}

/// Claude Code starts a native subagent through the `Task` (older) or
/// `Agent` tool.
fn spawns_subagent(content: &Value) -> bool {
    content.as_array().is_some_and(|blocks| {
        blocks.iter().any(|block| {
            block["type"] == "tool_use" && matches!(block["name"].as_str(), Some("Task" | "Agent"))
        })
    })
}

/// Anthropic cache writes split by lifetime, when the frame reports both.
fn cache_ttl(usage: &Value) -> Option<(u64, u64)> {
    let split = &usage["cache_creation"];
    number(split, "ephemeral_5m_input_tokens").zip(number(split, "ephemeral_1h_input_tokens"))
}

/// Fast mode or US-only inference changes the list price.
fn nonstandard_mode(usage: &Value) -> bool {
    usage["speed"].as_str().is_some_and(|speed| speed != "standard") || usage["inference_geo"] == "us"
}

/// Provider and model of a route payload (`{"provider","model","mode"}`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RouteRef {
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl RouteRef {
    fn parse(v: &Value) -> Option<Self> {
        let route = Self {
            provider: v["provider"].as_str().filter(|s| !s.is_empty() && s.len() <= 128).map(str::to_string),
            model: model_id(&v["model"]),
        };
        route.model.is_some().then_some(route)
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
    /// Slot of the execution an event of `turn` belongs to. Each
    /// `turn.admitted` after the first starts a new execution; events before
    /// any admission (older journals) belong to execution 0. `ledger.rs`
    /// applies the same rule through [`execution_index`].
    fn slot(&mut self, turn: i64, admitted: bool) -> i64 {
        let current = self.current.get(&turn).copied();
        let (exec, reuse) = match current {
            None => (0, None),
            Some((_, admissions)) if admitted && admissions > 0 => (admissions, None),
            Some((slot, _)) => (0, Some(slot)),
        };
        let slot = reuse.unwrap_or_else(|| {
            let id = self.slots.len() as i64;
            self.slots.insert(id, (turn, exec));
            if let Some((route, source)) = &self.in_force {
                let source = if *source == "route" { "route_carried" } else { source };
                self.routes.insert(id, (route.clone(), source));
            }
            id
        });
        let admissions = current.map_or(0, |(_, n)| n) + u32::from(admitted);
        self.current.insert(turn, (slot, admissions));
        slot
    }

    fn native(&mut self, turn: i64, v: &Value, at: Option<u64>) {
        match self.agent.as_str() {
            "codex" => self.codex(turn, v, at),
            "claude" => self.claude(turn, v, at),
            "deepseek" => self.deepseek(turn, v, at),
            "antigravity" => self.agy(turn, v, at),
            _ => {}
        }
    }

    fn codex(&mut self, turn: i64, v: &Value, at: Option<u64>) {
        if v["method"] != "thread/tokenUsage/updated" {
            return;
        }
        let p = &v["params"];
        let Some(thread) = identity(&p["threadId"]) else {
            self.partial = true;
            self.attempts.entry(turn).or_default().timing_invalid = true;
            return;
        };
        let next = codex(&p["tokenUsage"]["total"]);
        let last = codex(&p["tokenUsage"]["last"]);
        if next.total_tokens.is_none() || !last.any() {
            self.partial = true;
            self.attempts.entry(turn).or_default().timing_invalid = true;
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
                    self.attempts.entry(turn).or_default().timing_invalid = true;
                }
                last.clone()
            }
        };
        self.codex_baselines.insert(thread, next);
        self.codex_total.add(&delta);
        self.codex_turns.entry(turn).or_default().add(&delta);
        let attempt = self.attempts.entry(turn).or_default();
        // A delta equal to the last request is exactly one request; a larger
        // delta merges several, and its summed input still bounds each one.
        let bound = if delta == last { last.input_tokens } else { delta.input_tokens };
        match bound {
            Some(bound) => {
                attempt.request_input_bound =
                    Some(attempt.request_input_bound.map_or(bound, |max| max.max(bound)));
            }
            None => attempt.request_input_unknown = true,
        }
        attempt.reported = true;
        attempt.output_change(Some(0), delta.output_tokens, at);
    }

    fn claude(&mut self, turn: i64, v: &Value, at: Option<u64>) {
        // result.usage and assistant messages cover the main loop only.
        // modelUsage includes native children, but changed resume semantics
        // in 2.1.277 and cannot safely be inferred for unversioned old logs.
        if !v["parent_tool_use_id"].is_null() {
            self.subagents = true;
            return;
        }
        if v["type"] == "system" && v["subtype"] == "init" {
            if let Some(model) = model_id(&v["model"]) {
                self.attempts.entry(turn).or_default().model = Some((model, "native_init"));
            }
            return;
        }
        if v["type"] == "assistant" {
            if spawns_subagent(&v["message"]["content"]) || v["subagent_tool"] == true {
                self.subagents = true;
            }
            let attempt = self.attempts.entry(turn).or_default();
            attempt.nonstandard_mode |= nonstandard_mode(&v["message"]["usage"]);
            if attempt.model.is_none()
                && let Some(model) = model_id(&v["message"]["model"])
            {
                attempt.model = Some((model, "native_message"));
            }
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
            attempt.nonstandard_mode |= nonstandard_mode(&v["usage"]);
            attempt.ended = true;
            if v["is_error"] == true {
                attempt.failed = true;
            }
            if tokens.any() && !(v["subtype"] == "error_during_execution" && !tokens.positive()) {
                attempt.reported = true;
                let previous = attempt
                    .claude_result
                    .as_ref()
                    .map_or(Some(0), |t| t.output_tokens);
                // Identical terminal replays must not extend the measured interval.
                if attempt.claude_result.as_ref() != Some(&tokens) {
                    attempt.output_change(previous, tokens.output_tokens, at);
                }
                attempt.claude_result = Some(tokens);
                attempt.claude_result_ttl = cache_ttl(&v["usage"]);
            }
            attempt.finish(at);
        }
    }

    fn deepseek(&mut self, turn: i64, v: &Value, at: Option<u64>) {
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
            self.attempts.entry(turn).or_default().timing_invalid = true;
            return;
        };
        let tokens = deepseek(&v["usage"]);
        if !tokens.any() {
            // The headless projector deliberately omits usage if any native
            // retry omitted a sample. This is unknown, even after success.
            self.partial = true;
            self.attempts.entry(turn).or_default().timing_invalid = true;
            return;
        }
        self.partial |= tokens.total_tokens.is_none();
        let session = if self.session.is_empty() {
            format!("attempt-{turn}")
        } else {
            self.session.clone()
        };
        let key = format!("{session}/{native_turn}/{step}");
        let previous = self.units.get(&key);
        if previous.is_none_or(|(_, old)| old != &tokens) {
            let owner = previous.map_or(turn, |(owner, _)| *owner);
            let output = previous.map_or(Some(0), |(_, old)| old.output_tokens);
            self.attempts
                .entry(owner)
                .or_default()
                .output_change(output, tokens.output_tokens, at);
            self.units.insert(key, (owner, tokens));
        }
        self.attempts.entry(turn).or_default().reported = true;
    }

    fn agy(&mut self, turn: i64, v: &Value, at: Option<u64>) {
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
        let session_output = |session: &AgySession| {
            session.pending.values().try_fold(
                session
                    .total
                    .as_ref()
                    .map_or(Some(0), |t| t.output_tokens)?,
                |sum, (_, t)| sum.checked_add(t.output_tokens?),
            )
        };
        let previous = session_output(session);
        let mut changed = false;
        let mut correction_from_prior_attempt = false;
        if final_result {
            let older = session.total.as_ref();
            if older.is_some_and(|old| old.total_tokens > tokens.total_tokens) {
                self.partial = true;
                return;
            }
            if older != Some(&tokens) {
                if older.is_none() && previous != tokens.output_tokens {
                    // Even with observed steps, a first cumulative result can
                    // include resumed history outside this timed execution.
                    self.attempts.entry(turn).or_default().timing_invalid = true;
                }
                if older.is_none() && session.pending.is_empty() && tokens.positive() {
                    // Terminal-only history cannot establish its initial baseline.
                    self.partial = true;
                    self.attempts.entry(turn).or_default().timing_invalid = true;
                }
                changed = true;
                session.total = Some(tokens);
                session
                    .accounted
                    .extend(session.pending.drain().map(|(id, _)| id));
            }
        } else if let Some(step) = identity(&value["step_index"])
            && !session.accounted.contains(&step)
        {
            let previous = session.pending.get(&step);
            changed = previous.is_none_or(|(_, old)| old != &tokens);
            correction_from_prior_attempt =
                changed && previous.is_some_and(|(owner, _)| *owner != turn);
            let owner = previous.map_or(turn, |(owner, _)| *owner);
            session.pending.insert(step, (owner, tokens));
        }
        let next = session_output(session);
        let attempt = self.attempts.entry(turn).or_default();
        attempt.reported = true;
        if changed {
            attempt.output_change(previous, next, at);
        }
        if final_result && changed {
            attempt.finish(at);
        }
        if correction_from_prior_attempt || previous.zip(next).is_some_and(|(old, new)| new < old) {
            // A cumulative correction may alter an earlier attempt. Its
            // temporal allocation is unknown even though usage is corrected.
            for attempt in self.attempts.values_mut() {
                attempt.timing_invalid = true;
            }
        }
    }

    fn view(&self, job: i64) -> JobUsage {
        let mut tokens = self.codex_total.clone();
        for (_, unit) in self.units.values() {
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
            for (_, step) in session.pending.values() {
                tokens.add(step);
            }
        }
        let reported = self.attempts.values().filter(|a| a.reported).count() as u64;
        let unreported = self.attempts.len() as u64 - reported;
        let mut performance = UsagePerformance::default();
        for attempt in self.attempts.values() {
            performance.add(&attempt.performance());
        }
        performance.partial |= self.partial;
        JobUsage {
            job,
            agent: self.agent.clone(),
            tokens: tokens.finish(),
            reported_attempts: reported,
            unreported_attempts: unreported,
            // Claude counters cover the main loop only. They are incomplete
            // when the journal shows native subagents ran, not by default.
            partial: self.partial
                || unreported > 0
                || self.subagents
                || self.attempts.values().any(|a| !a.ended || a.failed),
            scope: if self.agent == "claude" {
                UsageScope::MainLoop
            } else {
                UsageScope::Harness
            },
            performance,
            native_subagents: self.subagents,
        }
    }

    /// List-price evidence of one execution slot.
    fn evidence(&self, slot: i64) -> crate::prices::PriceEvidence {
        let Some(attempt) = self.attempts.get(&slot) else {
            return Default::default();
        };
        let ttl = attempt.claude_result.as_ref().and(attempt.claude_result_ttl);
        crate::prices::PriceEvidence {
            cache_write_5m: ttl.map(|t| t.0),
            cache_write_1h: ttl.map(|t| t.1),
            request_input_bound: attempt.request_input_bound.filter(|_| !attempt.request_input_unknown),
            nonstandard_mode: attempt.nonstandard_mode,
            started_ms: attempt.started_ms,
            ended_ms: attempt.ended_ms,
        }
    }

    /// Reported tokens per attempt (turn). Conversation-cumulative usage that
    /// cannot be assigned to one attempt is returned separately.
    fn allocation(&self) -> (BTreeMap<i64, TokenCounts>, TokenCounts) {
        let mut turns: BTreeMap<i64, Sum> = BTreeMap::new();
        for (turn, sum) in &self.codex_turns {
            if let Some(tokens) = &sum.0 {
                turns.entry(*turn).or_default().add(tokens);
            }
        }
        for (owner, unit) in self.units.values() {
            turns.entry(*owner).or_default().add(unit);
        }
        for (turn, attempt) in &self.attempts {
            let entry = turns.entry(*turn).or_default();
            if let Some(result) = &attempt.claude_result {
                entry.add(result);
            } else {
                for message in attempt.claude_messages.values() {
                    entry.add(message);
                }
            }
        }
        let mut unallocated = Sum::default();
        for session in self.agy.values() {
            if let Some(total) = &session.total {
                unallocated.add(total);
            }
            for (_, step) in session.pending.values() {
                unallocated.add(step);
            }
        }
        (
            turns.into_iter().map(|(turn, sum)| (turn, sum.finish())).collect(),
            unallocated.finish(),
        )
    }
}

/// One attempt's reported usage, before the ledger classifies it.
pub(crate) struct RawAttempt {
    pub job: i64,
    /// `None` for usage that cannot be assigned to one attempt.
    pub turn: Option<i64>,
    /// Physical execution of `turn`: 0 for the first, then one per retry
    /// that reused the turn number.
    pub execution: u32,
    pub advisor: Option<String>,
    pub agent: String,
    pub tokens: TokenCounts,
    pub reported: bool,
    pub ended: bool,
    pub failed: bool,
    /// The job's or call's counters are incomplete (replays, regressions,
    /// missing fields, identity conflicts).
    pub partial: bool,
    /// Model named by this execution's own native frames, with its source.
    pub native_model: Option<(String, &'static str)>,
    /// Route in force for this execution, with its source (`route`,
    /// `route_carried`, `route_rotated` or `advice_route`).
    pub route: Option<(RouteRef, &'static str)>,
    /// The job's route changed by rotation at some point.
    pub rotated: bool,
    /// The job or call shows native subagent activity.
    pub subagents: bool,
    pub evidence: crate::prices::PriceEvidence,
}

/// Retains compact counters and deduplication identities, never raw payloads.
#[derive(Default)]
pub(crate) struct Entry {
    seq: i64,
    jobs: BTreeMap<i64, JobState>,
    advisors: BTreeMap<String, AdvisorState>,
    advisor_partial: bool,
    native_owners: HashMap<(String, String), HashSet<UsageOwner>>,
    shared_sessions: HashSet<(String, String)>,
}

struct AdvisorState {
    job: i64,
    turn: i64,
    started_seq: i64,
    state: JobState,
    identity_conflict: bool,
    /// Route recorded by `advice.started`.
    route: Option<RouteRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum UsageOwner {
    Job(i64),
    Advisor(String),
}

fn native_session(agent: &str, value: &Value) -> Option<String> {
    match agent {
        "codex" if value["method"] == "thread/tokenUsage/updated" => {
            identity(&value["params"]["threadId"])
        }
        "deepseek" if value["type"] == "session" => identity(&value["sessionId"]),
        "antigravity" => value["event"]
            .as_str()
            .filter(|kind| matches!(*kind, "step_update" | "result"))
            .and_then(|kind| identity(&value[kind]["conversation_id"])),
        _ => None,
    }
}

impl Entry {
    /// Whether every row up to `end` has been scanned.
    pub(crate) fn caught_up(&self, end: i64) -> bool {
        self.seq >= end
    }

    pub(crate) fn scan(&mut self, conn: &Connection, ticket: i64, end: i64) -> Result<()> {
        // events_ticket(ticket,seq) supplies the forward-only cursor. Size
        // guard runs in SQLite before Rust allocates or parses a payload.
        let mut stmt = conn.prepare_cached(
            "SELECT e.seq,e.job,e.turn,j.agent,e.kind,
             CASE WHEN e.kind IN ('advice.started','advice.result') AND json_valid(e.payload)
                  THEN json_object('attempt_id',json_extract(e.payload,'$.attempt_id'),
                                   'agent',json_extract(e.payload,'$.advisor.agent'),
                                   'provider',json_extract(e.payload,'$.advisor.provider'),
                                   'model',json_extract(e.payload,'$.advisor.model'),
                                   'state',json_extract(e.payload,'$.state'),
                                   'usage_complete',json_extract(e.payload,'$.usage_complete'))
                  WHEN e.kind IN ('route.requested','route.rotated') AND json_valid(e.payload)
                  THEN json_object('provider',coalesce(json_extract(e.payload,'$.to.provider'),json_extract(e.payload,'$.provider')),
                                   'model',coalesce(json_extract(e.payload,'$.to.model'),json_extract(e.payload,'$.model')))
                  WHEN e.kind IN ('native','usage.observed') AND length(CAST(e.payload AS BLOB))<=?4 THEN e.payload END,e.ts,
             -- An oversized Claude frame is not parsed, but whether it came
             -- from a native subagent still decides a coverage gap.
             CASE WHEN e.kind='native' AND j.agent='claude' AND length(CAST(e.payload AS BLOB))>?4 AND json_valid(e.payload)
                  THEN json_type(e.payload,'$.parent_tool_use_id') NOT IN ('null') END
             FROM events e LEFT JOIN jobs j ON j.id=e.job
             WHERE e.ticket=?1 AND e.seq>?2 AND e.seq<=?3
             AND e.kind IN ('native','turn.admitted','reply','failed','cancelled','interrupted','turn.retry_scheduled','turn.gave_up','route.rotated','turn.route_wait','auto.retry','advice.started','advice.result','usage.observed','route.requested')
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
            let at = row
                .get::<_, f64>(6)
                .ok()
                .map(|seconds| seconds * 1000.0)
                .filter(|ms| ms.is_finite() && *ms >= 0.0 && *ms < u64::MAX as f64)
                .map(|ms| ms.round() as u64);
            if matches!(
                kind.as_str(),
                "advice.started" | "advice.result" | "usage.observed"
            ) {
                let payload = row
                    .get::<_, Option<String>>(5)?
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok());
                match payload {
                    Some(value) => self.advisor(job, turn, &kind, &value, at),
                    None if kind == "advice.started" => {
                        self.advisor_partial = true;
                        self.advisor(job, turn, &kind, &Value::Null, at);
                    }
                    None => self.advisor_partial = true,
                }
                continue;
            }
            let payload = if matches!(kind.as_str(), "native" | "route.requested" | "route.rotated") {
                row.get::<_, Option<String>>(5)?
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            } else {
                None
            };
            let (native, route) = if kind == "native" {
                (payload, None)
            } else {
                (None, payload.as_ref().and_then(RouteRef::parse))
            };
            if let Some(value) = &native {
                self.register_identity(&agent, value, UsageOwner::Job(job));
            }
            let state = self.jobs.entry(job).or_insert_with(|| JobState {
                agent,
                ..Default::default()
            });
            if row.get::<_, Option<bool>>(7)? == Some(true) {
                state.subagents = true;
            }
            if kind == "route.requested" {
                // Route evidence never creates an execution of its own.
                let slot = state.slot(turn, false);
                if let Some(route) = route {
                    state.routes.insert(slot, (route.clone(), "route"));
                    state.in_force = Some((route, "route"));
                }
                continue;
            }
            if kind == "route.rotated" {
                state.rotated = true;
                // The next execution runs on the new route; unknown if the
                // payload does not name it.
                state.in_force = route.map(|route| (route, "route_rotated"));
            }
            // From here on, `turn` names the physical execution slot.
            let turn = state.slot(turn, kind == "turn.admitted");
            let attempt = state.attempts.entry(turn).or_default();
            match kind.as_str() {
                "turn.admitted" => {
                    if attempt.started_ms.is_none() {
                        attempt.started_ms = at;
                    }
                    attempt.timing_invalid |= at.is_none();
                }
                "reply" => {
                    attempt.ended = true;
                    attempt.finish(at);
                }
                "failed"
                | "cancelled"
                | "interrupted"
                | "turn.retry_scheduled"
                | "turn.gave_up"
                | "route.rotated"
                | "turn.route_wait"
                | "auto.retry" => {
                    attempt.ended = true;
                    attempt.failed = true;
                    attempt.finish(at);
                }
                "native" => match native {
                    Some(value) => state.native(turn, &value, at),
                    None => {
                        state.partial = true;
                        state.attempts.entry(turn).or_default().timing_invalid = true;
                    }
                },
                _ => {}
            }
        }
        if count < USAGE_SCAN_BATCH {
            self.seq = end;
        }
        Ok(())
    }

    fn register_identity(&mut self, agent: &str, value: &Value, owner: UsageOwner) {
        let Some(session) = native_session(agent, value) else {
            return;
        };
        let key = (agent.to_string(), session);
        let owners = self.native_owners.entry(key.clone()).or_default();
        owners.insert(owner);
        if owners
            .iter()
            .filter(|owner| matches!(owner, UsageOwner::Job(_)))
            .count()
            > 1
        {
            self.shared_sessions.insert(key);
        }
        if owners.len() > 1 {
            for owner in owners.iter() {
                if let UsageOwner::Advisor(id) = owner
                    && let Some(call) = self.advisors.get_mut(id)
                {
                    // Shared cumulative history cannot be allocated to a call.
                    call.identity_conflict = true;
                }
            }
        }
    }

    fn advisor(&mut self, job: i64, turn: i64, kind: &str, value: &Value, at: Option<u64>) {
        if kind == "advice.started" {
            let id =
                identity(&value["attempt_id"]).unwrap_or_else(|| format!("advice:{}", self.seq));
            self.advisors.entry(id).or_insert_with(|| {
                let mut state = JobState {
                    agent: value["agent"].as_str().unwrap_or_default().to_string(),
                    ..Default::default()
                };
                state.attempts.insert(
                    1,
                    Attempt {
                        started_ms: at,
                        timing_invalid: at.is_none(),
                        ..Default::default()
                    },
                );
                AdvisorState {
                    job,
                    turn,
                    started_seq: self.seq,
                    state,
                    identity_conflict: false,
                    route: RouteRef::parse(value),
                }
            });
            return;
        }
        let id = identity(&value["attempt_id"]).or_else(|| {
            // Legacy consultations were sequential and had no explicit ID.
            (kind == "advice.result")
                .then(|| {
                    self.advisors
                        .iter()
                        .filter(|(_, call)| {
                            call.job == job
                                && call.turn == turn
                                && !call
                                    .state
                                    .attempts
                                    .get(&1)
                                    .is_some_and(|attempt| attempt.ended)
                        })
                        .max_by_key(|(_, call)| call.started_seq)
                        .map(|(id, _)| id.clone())
                })
                .flatten()
        });
        let Some(id) = id else {
            self.advisor_partial = true;
            return;
        };
        if kind == "usage.observed"
            && let Some(agent) = self.advisors.get(&id).map(|call| call.state.agent.clone())
        {
            self.register_identity(&agent, &value["sample"], UsageOwner::Advisor(id.clone()));
        }
        let Some(call) = self.advisors.get_mut(&id) else {
            self.advisor_partial = true;
            return;
        };
        if call.job != job || call.turn != turn {
            self.advisor_partial = true;
            return;
        }
        if kind == "usage.observed" {
            if value["schema_version"] != 1
                || value["role"] != "advisor"
                || value["agent"].as_str() != Some(call.state.agent.as_str())
            {
                call.state.partial = true;
                return;
            }
            call.state.native(1, &value["sample"], at);
        } else {
            call.state.partial |= value["usage_complete"] == false || value["usage_complete"] == 0;
            let attempt = call.state.attempts.entry(1).or_default();
            attempt.ended = true;
            attempt.failed |= value["state"] != "done";
            attempt.finish(at);
        }
    }

    pub(crate) fn view(&self, end: i64, visible: &[(i64, String)], total_jobs: i64) -> TicketUsage {
        let mut totals = Sum::default();
        let mut performance = UsagePerformance::default();
        let mut partial =
            self.seq < end || self.advisor_partial || !self.shared_sessions.is_empty();
        let mut reported_jobs = 0;
        let mut jobs = Vec::new();
        for (id, state) in &self.jobs {
            let view = state.view(*id);
            totals.add(&view.tokens);
            performance.add(&view.performance);
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
        let mut advisor_calls = Vec::new();
        let mut reported_advisor_calls = 0;
        for (attempt_id, call) in &self.advisors {
            let mut view = call.state.view(call.job);
            if call.identity_conflict {
                view.tokens = TokenCounts::default();
                view.reported_attempts = 0;
                view.partial = true;
                view.performance = UsagePerformance {
                    partial: true,
                    ..Default::default()
                };
            }
            totals.add(&view.tokens);
            performance.add(&view.performance);
            partial |= view.partial;
            let reported = view.reported_attempts > 0;
            reported_advisor_calls += u64::from(reported);
            advisor_calls.push(AdvisorUsage {
                attempt_id: attempt_id.clone(),
                job: call.job,
                turn: call.turn,
                agent: view.agent,
                tokens: view.tokens,
                reported,
                partial: view.partial,
                scope: view.scope,
                performance: view.performance,
                native_subagents: view.native_subagents,
            });
        }
        let unreported_advisor_calls = advisor_calls.len() as u64 - reported_advisor_calls;
        performance.partial |= self.seq < end || unreported_jobs > 0 || self.advisor_partial;
        TicketUsage {
            totals: totals.finish(),
            jobs,
            unreported_jobs,
            partial: partial || unreported_jobs > 0,
            pending: self.seq < end,
            cursor: self.seq,
            performance,
            advisor_calls,
            reported_advisor_calls,
            unreported_advisor_calls,
            shared_native_sessions: self.shared_sessions.len() as u64,
        }
    }

    /// Every scanned attempt with its own reported usage, for the ledger.
    pub(crate) fn attempts(&self) -> Vec<RawAttempt> {
        let mut out = vec![];
        for (id, state) in &self.jobs {
            let (turns, unallocated) = state.allocation();
            for (slot, attempt) in &state.attempts {
                let (turn, execution) = state.slots.get(slot).copied().unwrap_or((*slot, 0));
                out.push(RawAttempt {
                    job: *id,
                    turn: Some(turn),
                    execution,
                    advisor: None,
                    agent: state.agent.clone(),
                    tokens: turns.get(slot).cloned().unwrap_or_default(),
                    reported: attempt.reported,
                    ended: attempt.ended,
                    failed: attempt.failed,
                    partial: state.partial,
                    native_model: attempt.model.clone(),
                    route: state.routes.get(slot).cloned(),
                    rotated: state.rotated,
                    subagents: state.subagents,
                    evidence: state.evidence(*slot),
                });
            }
            if unallocated.any() {
                out.push(RawAttempt {
                    job: *id,
                    turn: None,
                    execution: 0,
                    advisor: None,
                    agent: state.agent.clone(),
                    tokens: unallocated,
                    reported: true,
                    ended: true,
                    failed: false,
                    partial: state.partial,
                    native_model: None,
                    // Session-cumulative usage spans executions; only a route
                    // that never rotated describes all of it.
                    route: state.in_force.clone().filter(|_| !state.rotated),
                    rotated: state.rotated,
                    subagents: state.subagents,
                    evidence: Default::default(),
                });
            }
        }
        for (attempt_id, call) in &self.advisors {
            let view = call.state.view(call.job);
            let attempt = call.state.attempts.get(&1);
            out.push(RawAttempt {
                job: call.job,
                turn: Some(call.turn),
                execution: 0,
                advisor: Some(attempt_id.clone()),
                agent: call.state.agent.clone(),
                tokens: if call.identity_conflict { TokenCounts::default() } else { view.tokens },
                reported: !call.identity_conflict && view.reported_attempts > 0,
                ended: attempt.is_some_and(|a| a.ended),
                failed: attempt.is_some_and(|a| a.failed),
                partial: call.state.partial || call.identity_conflict,
                native_model: attempt.and_then(|a| a.model.clone()),
                route: call.route.clone().map(|route| (route, "advice_route")),
                rotated: false,
                subagents: call.state.subagents,
                evidence: call.state.evidence(1),
            });
        }
        out
    }

    /// Native sessions reported by more than one job, as (agent, session).
    pub(crate) fn shared_sessions(&self) -> Vec<(String, String)> {
        let mut shared: Vec<_> = self.shared_sessions.iter().cloned().collect();
        shared.sort();
        shared
    }

    pub(crate) fn advisor_partial(&self) -> bool {
        self.advisor_partial
    }

    pub(crate) fn position(&self) -> i64 {
        self.seq
    }
}
