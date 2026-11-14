"""Per-request prompt-cache audit of a journal copy (numbers only, no content).

Usage: python scripts/cache-hit-audit.py <hub.sqlite3 copy> [days]
Codex: per-request `tokenUsage.last`; Claude: main-loop `message_start.usage`.
Requests are bucketed by the gap since the same thread/job's previous request.
"""
import collections
import json
import sqlite3
import sys

db = sqlite3.connect(sys.argv[1])
days = float(sys.argv[2]) if len(sys.argv) > 2 else 3


def bucket_gap(gap):
    if gap is None:
        return "first"
    for limit, name in [(60, "<1m"), (300, "1-5m"), (3600, "5-60m")]:
        if gap < limit:
            return name
    return ">60m"


# ---- Codex: one tokenUsage/updated per model request; `last` is that request.
rows = db.execute(
    """select e.seq,e.ts,e.job,e.turn,e.payload from events e join jobs j on j.id=e.job
       where j.agent='codex' and e.kind='native' and e.ts > strftime('%s','now',?)
       and e.payload like '%tokenUsage/updated%' order by e.seq""", (f"-{days} day",)).fetchall()
last_ts = {}
seen_turn = set()
codex = collections.defaultdict(lambda: [0, 0, 0])  # requests, input, cached
by_gap = collections.defaultdict(lambda: [0, 0, 0])
by_pos = collections.defaultdict(lambda: [0, 0, 0])
worst = []
for seq, ts, job, turn, payload in rows:
    p = json.loads(payload)["params"]
    last = p["tokenUsage"]["last"]
    inp, cached = last["inputTokens"], last["cachedInputTokens"]
    thread = p["threadId"]
    gap = None if thread not in last_ts else ts - last_ts[thread]
    last_ts[thread] = ts
    pos = "turn-first" if (job, turn) not in seen_turn else "turn-later"
    seen_turn.add((job, turn))
    for agg in (codex["all"], by_gap[bucket_gap(gap)], by_pos[pos]):
        agg[0] += 1; agg[1] += inp; agg[2] += cached
    worst.append((inp - cached, bucket_gap(gap), pos, job, turn, inp))


def show(name, agg):
    n, inp, cached = agg
    rate = cached / inp if inp else 0
    print(f"  {name:<12} requests={n:<6} input={inp:>12,} cached={cached:>12,} uncached={inp - cached:>11,} hit={rate:.4f}")


print(f"== Codex (last {days} days)")
show("all", codex["all"])
for k in ["first", "<1m", "1-5m", "5-60m", ">60m"]:
    if k in by_gap:
        show("gap " + k, by_gap[k])
for k, v in by_pos.items():
    show(k, v)
worst.sort(reverse=True)
uncached_total = sum(w[0] for w in worst) or 1
top = collections.Counter()
for w in worst:
    top[(w[1], w[2])] += w[0]
print("  uncached share by (gap, position):")
for k, v in top.most_common(8):
    print(f"    {k}: {v:,} ({v / uncached_total:.1%})")

# ---- Claude: message_start usage per request (main loop only).
rows = db.execute(
    """select e.seq,e.ts,e.job,e.turn,e.payload from events e join jobs j on j.id=e.job
       where j.agent='claude' and e.kind='native' and e.ts > strftime('%s','now',?)
       and e.payload like '%message_start%' order by e.seq""", (f"-{days} day",)).fetchall()
claude = collections.defaultdict(lambda: [0, 0, 0, 0])  # n, input, read, write
last_ts, seen = {}, set()
for seq, ts, job, turn, payload in rows:
    d = json.loads(payload)
    if d.get("parent_tool_use_id"):
        key_scope = "subagent"
    else:
        key_scope = "main"
    u = d["event"]["message"]["usage"]
    inp, read, write = u.get("input_tokens", 0), u.get("cache_read_input_tokens", 0), u.get("cache_creation_input_tokens", 0)
    gap = None if job not in last_ts else ts - last_ts[job]
    last_ts[job] = ts
    pos = "turn-first" if (job, turn) not in seen else "turn-later"
    seen.add((job, turn))
    for k in (key_scope, f"{key_scope} gap {bucket_gap(gap)}", f"{key_scope} {pos}"):
        a = claude[k]; a[0] += 1; a[1] += inp; a[2] += read; a[3] += write
print(f"== Claude (last {days} days)")
for k in sorted(claude):
    n, inp, read, write = claude[k]
    total = inp + read + write
    print(f"  {k:<22} requests={n:<5} read={read:>11,} write={write:>10,} input={inp:>8,} hit={read / total if total else 0:.4f}")
