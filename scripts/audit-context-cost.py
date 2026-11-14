"""Read-only, content-free context audit. Does not contact a model/provider.

Byte counts measure platform injection, not provider tokens or billing. Claude
result samples exclude internal subagents and requests with no terminal usage.
"""
import argparse
from collections import defaultdict
import json
from pathlib import Path
import sqlite3


def encoded_size(value):
    return len(json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8"))


def audit(database, recent, native_tail):
    connection = sqlite3.connect(database.resolve().as_uri() + "?mode=ro", uri=True)
    connection.execute("PRAGMA query_only=ON")
    inputs = defaultdict(lambda: {"requests": 0, "json_utf8_bytes": 0, "max_json_utf8_bytes": 0})
    fields = defaultdict(int)
    lead_packets = []
    for agent, lead, payload in connection.execute(
        "SELECT j.agent,j.parent IS NULL,e.payload FROM events e "
        "JOIN jobs j ON j.id=e.job WHERE e.kind='input' ORDER BY e.seq"
    ):
        size = len(payload.encode("utf-8"))
        item = inputs[agent + ("/lead" if lead else "/worker")]
        item["requests"] += 1
        item["json_utf8_bytes"] += size
        item["max_json_utf8_bytes"] = max(item["max_json_utf8_bytes"], size)
        if lead:
            lead_packets.append(payload)
            if len(lead_packets) > recent:
                lead_packets.pop(0)
    for payload in lead_packets:
        for key, value in json.loads(payload).items():
            fields[key] += encoded_size(value)

    delivery = {"requests": 0, "incremental_requests": 0, "full_prompt_bytes": 0, "sent_prompt_bytes": 0, "saved_bytes": 0}
    for (payload,) in connection.execute("SELECT payload FROM events WHERE kind='running'"):
        event = json.loads(payload)
        names = ("full_prompt_bytes", "sent_prompt_bytes", "saved_bytes")
        if not all(isinstance(event.get(name), int) and event[name] >= 0 for name in names):
            continue
        delivery["requests"] += 1
        delivery["incremental_requests"] += int(event.get("incremental") is True)
        for name in names:
            delivery[name] += event[name]

    # One terminal result per platform turn. Repeated stream frames must not
    # inflate usage; this sample deliberately excludes assistant estimates.
    results = {}
    for job, turn, payload in connection.execute(
        "SELECT e.job,e.turn,e.payload FROM events e JOIN jobs j ON j.id=e.job "
        "WHERE e.kind='native' AND j.agent='claude' AND j.parent IS NULL "
        "ORDER BY e.seq DESC LIMIT ?", (native_tail,)
    ):
        if len(payload) > 262144:
            continue
        event = json.loads(payload)
        if event.get("type") != "result" or event.get("parent_tool_use_id") is not None or (job, turn) in results:
            continue
        usage = event.get("usage", {})
        names = ("input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens", "output_tokens")
        if all(isinstance(usage.get(name), int) and usage[name] >= 0 for name in names):
            results[job, turn] = {name: usage[name] for name in names}
    totals = {name: sum(row[name] for row in results.values()) for name in (
        "input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens", "output_tokens"
    )}
    inclusive = sum(totals[name] for name in ("input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"))
    connection.close()
    return {
        "scope": "read-only platform input bytes; no prompts, credentials or model requests",
        "input_groups": dict(inputs),
        "recent_lead_inputs": len(lead_packets),
        "recent_lead_field_utf8_bytes": dict(sorted(fields.items(), key=lambda pair: -pair[1])),
        "delivery_bytes_all_jobs": delivery,
        "claude_lead_terminal_sample": {
            "native_tail_limit": native_tail,
            "complete_usage_turns": len(results),
            "main_loop_only": True,
            "partial_sample": True,
            "tokens": totals,
            "inclusive_input_tokens": inclusive,
            "cache_read_fraction": totals["cache_read_input_tokens"] / inclusive if inclusive else None,
            "note": "Not whole-ticket billing: terminal results only; retries/incomplete turns and internal subagents may be missing.",
        },
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("database", type=Path)
    parser.add_argument("--recent", type=int, default=30)
    parser.add_argument("--native-tail", type=int, default=20000)
    args = parser.parse_args()
    if args.recent < 1 or args.native_tail < 1:
        parser.error("sample limits must be positive")
    print(json.dumps(audit(args.database, args.recent, args.native_tail), ensure_ascii=False, indent=2))
