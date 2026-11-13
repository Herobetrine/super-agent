"""Read-only bounded journal tail. Writes numeric metadata only, never content.

Usage is a partial observation since --after-seq, not whole-ticket billing.
Claude terminal and per-request samples are separate and must not be added.
"""
import argparse
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import time


TOKEN_NAMES = ("input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens", "output_tokens")
AGENTS = {"claude", "codex", "deepseek", "antigravity", "acp"}
STATES = {"queued", "running", "waiting", "done", "failed", "cancelled", "interrupted", "completed", "partial", "draining"}
RETRIES = {"retry", "auto.retry", "turn.retry_scheduled", "route.rotated"}
CONTROLS = {"control", "control.invalid", "control.rejected", "control.accepted", "send", "spawn", "cancel",
            "protocol.progress_invalid", "protocol.preview_invalid", "job.progress", "cancel.rejected", "child.cancelled"}
PAUSE_EVENTS = {"ticket.pause_requested", "ticket.resumed", "ticket.paused", "ticket.handoff_started", "ticket.pause_draining"}
TERMINALS = {"reply", "failed", "cancelled", "interrupted", "partial", "turn.gave_up", "turn.pause_failure"}


def integer(value):
    return value if type(value) is int and value >= 0 else None


def identity(value):
    if isinstance(value, str) and 0 < len(value) <= 256:
        return hashlib.sha256(value.encode()).hexdigest()
    if integer(value) is not None:
        return str(value)
    return None


def counts(usage, agent):
    if not isinstance(usage, dict):
        usage = {}
    if agent == "claude":
        return {name: integer(usage.get(name)) for name in TOKEN_NAMES}
    mapping = ("inputTokens", "cachedInputTokens", "cacheWriteInputTokens", "outputTokens")
    if agent == "deepseek":
        mapping = ("inputTokens", "cacheReadTokens", "cacheWriteTokens", "outputTokens")
    result = dict(zip(TOKEN_NAMES, (integer(usage.get(name)) for name in mapping)))
    if agent == "deepseek":
        total = integer(usage.get("totalTokens"))
        output = result["output_tokens"]
        if total is not None and output is not None and total >= output:
            result["input_tokens"] = total - output
        elif all(result[name] is not None for name in TOKEN_NAMES[:3]):
            result["input_tokens"] = sum(result[name] for name in TOKEN_NAMES[:3])
    return result


def aggregate(samples, agent, outcomes=None):
    samples = list(samples)
    totals = {name: sum(row[name] for row in samples if row[name] is not None)
              if any(row[name] is not None for row in samples) else None for name in TOKEN_NAMES}
    ratio_names = TOKEN_NAMES[:3] if agent == "claude" else TOKEN_NAMES[:2]
    eligible = samples if outcomes is None else [row for row, outcome in zip(samples, outcomes) if outcome == "success"]
    paired = [row for row in eligible if all(row[name] is not None for name in ratio_names)
              and (sum(row[name] for name in ratio_names) if agent == "claude" else row["input_tokens"]) > 0]
    denominator = sum(sum(row[name] for name in TOKEN_NAMES[:3]) if agent == "claude"
                      else row["input_tokens"] for row in paired)
    reads = sum(row["cache_read_input_tokens"] for row in paired)
    return {"samples": len(samples), "complete_samples": sum(all(v is not None for v in row.values()) for row in samples),
            "tokens": totals, "cache_read_fraction": reads / denominator if denominator else None,
            "cache_ratio_samples": len(paired)}


class Sampler:
    def __init__(self, connection, ticket, after_seq, batch_size=2000):
        self.connection = connection
        self.ticket = ticket
        self.start_seq = after_seq
        self.cursor = after_seq
        self.batch_size = batch_size
        self.event_counts = Counter()
        self.delivery = Counter()
        self.turns = {}
        self.messages = {}
        self.results = {}
        self.codex_baselines = {}
        self.codex_units = {}
        self.dsh_sessions = {}
        self.dsh_units = {}
        self.incomplete_codex = 0

    def poll(self):
        # events_ticket(ticket,seq) bounds the scan even in a multi-million-row journal.
        rows = self.connection.execute(
            "SELECT e.seq,e.ts,e.job,e.turn,e.kind,e.payload,j.agent,j.parent FROM events e "
            "LEFT JOIN jobs j ON j.id=e.job WHERE e.ticket=? AND e.seq>? ORDER BY e.seq LIMIT ?",
            (self.ticket, self.cursor, self.batch_size)).fetchall()
        records = []
        for seq, timestamp, job, turn, kind, payload, agent, parent in rows:
            self.cursor = seq
            category = (kind if kind in {"running", "input", "context.compacted"} else
                        "retry" if kind in RETRIES else "control" if kind in CONTROLS else
                        "pause_gate" if kind in PAUSE_EVENTS else "terminal" if kind in TERMINALS else "other")
            self.event_counts[category] += 1
            base = {"seq": seq, "ts": timestamp, "ticket": self.ticket, "job": job, "turn": turn,
                    "agent": agent if agent in AGENTS else "unknown", "parent": parent, "is_root": parent is None}
            if job is not None and turn is not None:
                self.turns.setdefault((job, turn), {"agent": base["agent"], "running": 0, "ended": False,
                                                  "started_at": None, "platform_ended_at": None, "outcome": "pending"})
            try:
                value = json.loads(payload) if kind in {"running", "native"} else {}
            except (ValueError, TypeError):
                self.event_counts["invalid_json"] += 1
                continue
            if not isinstance(value, dict):
                continue
            if kind == "running" and (job, turn) in self.turns:
                self.turns[job, turn]["running"] += 1
                if self.turns[job, turn]["started_at"] is None:
                    self.turns[job, turn]["started_at"] = timestamp
                names = ("full_prompt_bytes", "sent_prompt_bytes", "saved_bytes")
                numbers = {name: integer(value.get(name)) for name in names}
                valid = all(n is not None for n in numbers.values())
                self.delivery["requests"] += 1
                mode = "delta" if value.get("incremental") is True else "full"
                self.delivery[mode + "_requests"] += 1
                if valid:
                    self.delivery["measured_requests"] += 1
                    self.delivery.update(numbers)
                    if numbers["full_prompt_bytes"] - numbers["sent_prompt_bytes"] != numbers["saved_bytes"]:
                        self.delivery["inconsistent_requests"] += 1
                records.append(dict(base, record="delivery", mode=mode, bytes=numbers, complete=valid))
            elif kind == "native":
                record = self.native(base, value)
                if record is not None:
                    records.append(record)
            elif category in {"retry", "control", "context.compacted", "terminal", "pause_gate"}:
                if category == "terminal" and (job, turn) in self.turns:
                    self.turns[job, turn]["ended"] = True
                    self.turns[job, turn]["platform_ended_at"] = timestamp
                    outcome = "success" if kind == "reply" else "failed" if kind in {"turn.gave_up", "turn.pause_failure"} else kind
                    if outcome != "success" or self.turns[job, turn]["outcome"] not in {"failed", "cancelled", "interrupted", "partial"}:
                        self.turns[job, turn]["outcome"] = outcome
                records.append(dict(base, record=category, event=kind))
        return records, len(rows) == self.batch_size

    def native(self, base, value):
        agent = base["agent"]
        key = base["job"], base["turn"]
        usage = None
        source = None
        sample_key = key
        if (value.get("type") == "system" and
                (value.get("subtype") == "compact_boundary" or
                 value.get("subtype") == "status" and value.get("status") == "compacting")) or value.get("method") == "thread/compacted":
            self.event_counts["native_compaction_signals"] += 1
        if key not in self.turns:
            return None
        if agent == "claude" and value.get("parent_tool_use_id") is None:
            if value.get("type") == "result":
                self.turns[key]["ended"] = True
                self.turns[key]["native_ended_at"] = base["ts"]
                self.turns[key]["native_is_error"] = value.get("is_error") if type(value.get("is_error")) is bool else None
                usage = counts(value.get("usage"), agent)
                subtype = value.get("subtype")
                outcome = ("failed" if value.get("is_error") is True or
                           isinstance(subtype, str) and subtype.startswith("error") else
                           "success" if subtype == "success" or value.get("is_error") is False else "unknown")
                self.turns[key]["outcome"] = outcome
                if any(number is not None for number in usage.values()) or key not in self.results:
                    self.results[key] = usage
                source = "claude_terminal_main_loop"
            elif value.get("type") == "assistant" and isinstance(value.get("message"), dict):
                message = value["message"]
                message_id = identity(message.get("id"))
                if message_id is not None:
                    usage = counts(message.get("usage"), agent)
                    message_key = base["job"], message_id
                    sample_key = message_key
                    previous = self.messages.get(message_key, {})
                    usage = {name: usage[name] if usage[name] is not None else previous.get(name) for name in TOKEN_NAMES}
                    self.messages[message_key] = usage
                    source = "claude_request_main_loop"
        elif agent == "codex" and value.get("method") == "thread/tokenUsage/updated":
            params = value.get("params", {})
            token_usage = params.get("tokenUsage", {})
            thread = identity(params.get("threadId"))
            total = counts(token_usage.get("total"), agent)
            total_n = integer(token_usage.get("total", {}).get("totalTokens"))
            baseline_key = base["job"], thread
            previous = self.codex_baselines.get(baseline_key)
            if thread is None or total_n is None:
                self.incomplete_codex += 1
            elif previous is not None and total_n <= previous[0]:
                if total_n < previous[0]:
                    self.incomplete_codex += 1
            else:
                if previous is None:
                    usage = counts(token_usage.get("last"), agent)
                    if total_n != integer(token_usage.get("last", {}).get("totalTokens")):
                        self.incomplete_codex += 1
                else:
                    usage = {name: total[name] - previous[1][name]
                             if total[name] is not None and previous[1][name] is not None and total[name] >= previous[1][name]
                             else None for name in TOKEN_NAMES}
                self.codex_baselines[baseline_key] = total_n, total
                self.codex_units[base["seq"]] = usage
                sample_key = base["seq"]
                source = "codex_observed_delta"
        elif agent == "deepseek":
            if value.get("type") == "session":
                self.dsh_sessions[base["job"]] = identity(value.get("sessionId"))
            elif value.get("type") == "status" and value.get("phase") == "step_end":
                native_turn, step = identity(value.get("turn")), identity(value.get("step"))
                if native_turn is not None and step is not None:
                    session = self.dsh_sessions.get(base["job"]) or ("attempt", base["turn"])
                    usage = counts(value.get("usage"), agent)
                    self.dsh_units[base["job"], session, native_turn, step] = usage
                    sample_key = base["job"], session, native_turn, step
                    source = "deepseek_step_includes_native_retries"
        if usage is not None:
            sample_id = hashlib.sha256(json.dumps([source, sample_key], sort_keys=True).encode()).hexdigest()
            return dict(base, record="usage_update", source=source, tokens=usage,
                        sample_id=sample_id,
                        complete=all(number is not None for number in usage.values()))
        return None

    def summary(self):
        jobs = [{"job": job, "parent": parent, "agent": agent if agent in AGENTS else "unknown",
                 "turn": turn, "state": state if state in STATES else "unknown"}
                for job, parent, agent, turn, state in self.connection.execute(
                    "SELECT id,parent,agent,turn,state FROM jobs WHERE ticket=? ORDER BY id", (self.ticket,))]
        terminal_outcomes = [self.turns[key]["outcome"] for key in self.results]
        claude = aggregate(self.results.values(), "claude", terminal_outcomes)
        claude["outcomes"] = dict(Counter(terminal_outcomes))
        pause_state = None
        if self.connection.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='ticket_pause'").fetchone():
            row = self.connection.execute("SELECT state FROM ticket_pause WHERE ticket=?", (self.ticket,)).fetchone()
            pause_state = row[0] if row and row[0] in {"draining", "handoff", "paused"} else None
        return {"schema_version": 1, "ticket": self.ticket, "after_seq": self.start_seq,
                "through_seq": self.cursor, "updated_at": time.time(), "scope": "partial_tail_main_loop_only",
                "event_counts": dict(self.event_counts), "delivery": dict(self.delivery),
                "claude_terminal": claude,
                "claude_requests_separate_do_not_add": aggregate(self.messages.values(), "claude"),
                "codex_observed_deltas": aggregate(self.codex_units.values(), "codex"),
                "codex_missing_baselines_or_regressions": self.incomplete_codex,
                "deepseek_steps": aggregate(self.dsh_units.values(), "deepseek"),
                "observed_turns": [dict(job=job, turn=turn, **data,
                                        wall_seconds=(data["platform_ended_at"] - data["started_at"])
                                        if data["platform_ended_at"] is not None and data["started_at"] is not None else None,
                                        claude_terminal_tokens=self.results.get((job, turn)),
                                        claude_terminal_usage_complete=all(number is not None for number in self.results[job, turn].values())
                                        if (job, turn) in self.results else None)
                                   for (job, turn), data in sorted(self.turns.items())],
                "jobs": jobs, "pause_state": pause_state,
                "cache_assessment": ("awaiting_terminal" if not terminal_outcomes and
                                     any(data["outcome"] == "pending" for data in self.turns.values()) else
                                     "no_observed_terminal" if not terminal_outcomes else
                                     "no_successful_terminal" if "success" not in terminal_outcomes else
                                     "warm_up_or_missing" if claude["cache_ratio_samples"] < 3 else "observed_partial_sample"),
                "alert_candidates": ["delivery_byte_inconsistency"] if self.delivery["inconsistent_requests"] else [],
                "limitations": ["No automatic cancellation or provider calls.",
                                "Retry event counts are raw events, not distinct attempts. One retry can emit multiple events.",
                                "Missing usage is null, not zero. Replayed usage_update rows replace prior samples.",
                                "Claude internal subagents excluded. Terminal totals and request samples overlap.",
                                "Terminal cache ratios require successful outcome and positive known input denominator.",
                                "Request ratios describe reported requests even if their turn is pending or later fails.",
                                "Tail starts mid-history. Codex first frame uses last request only.",
                                "Unsupported native usage and unreported retries are unknown."]}


def atomic_summary(path, value):
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    os.replace(temporary, path)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--database", type=Path, required=True)
    parser.add_argument("--ticket", type=int, required=True)
    parser.add_argument("--after-seq", type=int, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--summary", type=Path)
    parser.add_argument("--poll", type=float, default=5)
    parser.add_argument("--duration", type=float, default=300)
    parser.add_argument("--batch-size", type=int, default=2000)
    parser.add_argument("--once", action="store_true", help="read one bounded batch and exit")
    args = parser.parse_args()
    if args.after_seq < 0 or args.ticket < 1 or args.poll <= 0 or args.duration <= 0 or not 1 <= args.batch_size <= 10000:
        parser.error("positive ticket/poll/duration, nonnegative seq, batch size 1..10000 required")
    summary = args.summary or args.output.with_suffix(args.output.suffix + ".summary.json")
    if args.output.resolve() == summary.resolve() or args.database.resolve() in {args.output.resolve(), summary.resolve()}:
        parser.error("database, output and summary paths must differ")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    summary.parent.mkdir(parents=True, exist_ok=True)
    # Exclusive creation prevents silently mixing runs with incompatible dedup state.
    with args.output.open("x", encoding="utf-8") as output:
        connection = sqlite3.connect(args.database.resolve().as_uri() + "?mode=ro", uri=True, timeout=2)
        connection.execute("PRAGMA query_only=ON")
        sampler = Sampler(connection, args.ticket, args.after_seq, args.batch_size)
        deadline = time.monotonic() + args.duration
        try:
            while True:
                records, more = sampler.poll()
                for record in records:
                    output.write(json.dumps(record, ensure_ascii=False) + "\n")
                output.flush()
                value = sampler.summary()
                value["batch_limit_reached"] = more
                atomic_summary(summary, value)
                if args.once or time.monotonic() >= deadline:
                    break
                if not more:
                    time.sleep(min(args.poll, max(0, deadline - time.monotonic())))
        except KeyboardInterrupt:
            atomic_summary(summary, sampler.summary())
        finally:
            connection.close()


if __name__ == "__main__":
    main()
