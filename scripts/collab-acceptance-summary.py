#!/usr/bin/env python3
"""Read-only journal evidence summary. Never runs the scenario or project.

Usage: python scripts/collab-acceptance-summary.py RUN_DIR_OR_DB [--json]
Token semantics follow crates/sa-store/src/usage.rs. Native IDs and event
locations are evidence, while agent reports and job states are claims/status.
Only allowlisted metadata is emitted, never input bodies or raw native payloads.
"""

import argparse
from collections import Counter, defaultdict
import json
from pathlib import Path
import re
import sqlite3
import sys


CONTROL = re.compile(r"^SA_([A-Z_]+)\s*(.*)$")
FIELDS = ("total_tokens", "input_tokens", "output_tokens",
          "cache_read_input_tokens", "cache_creation_input_tokens",
          "reasoning_output_tokens")


def number(value):
    return value if type(value) is int and 0 <= value <= 9007199254740991 else None


def identity(value):
    if isinstance(value, str) and 0 < len(value) <= 256:
        return value
    return None


def add_known(values):
    values = list(values)
    return sum(values) if values and all(v is not None for v in values) else None


def tokens(usage, agent):
    usage = usage if isinstance(usage, dict) else {}
    result = dict.fromkeys(FIELDS)
    if agent == "claude":
        read = number(usage.get("cache_read_input_tokens"))
        write = number(usage.get("cache_creation_input_tokens"))
        result.update(input_tokens=add_known([number(usage.get("input_tokens")), read, write]),
                      output_tokens=number(usage.get("output_tokens")),
                      cache_read_input_tokens=read, cache_creation_input_tokens=write)
        result["total_tokens"] = add_known([result["input_tokens"], result["output_tokens"]])
    else:
        result.update(total_tokens=number(usage.get("total_tokens")),
                      output_tokens=number(usage.get("output_tokens")),
                      cache_read_input_tokens=number(usage.get("cache_read_tokens")),
                      reasoning_output_tokens=number(usage.get("thinking_tokens")))
    return result


def aggregate(samples):
    return {field: add_known(s[field] for s in samples) for field in FIELDS}


def control_metadata(tag, value):
    result = {"tag": tag, "valid_json": isinstance(value, dict)}
    if not isinstance(value, dict):
        return result
    for key in ("after", "continue", "to", "write", "status"):
        if key in value:
            result[key] = value[key]
    for key in ("options", "verified", "unverified"):
        if isinstance(value.get(key), list):
            result[key + "_count"] = len(value[key])
    return result


def location(event):
    return {key: event[key] for key in ("seq", "job", "turn", "kind")}


def resolve(source):
    source = source.expanduser().resolve()
    if source.is_file():
        return source, None
    candidates = [p for p in (source / "hub.sqlite3", source / "ledger/hub.sqlite3") if p.is_file()]
    if len(candidates) != 1:
        raise ValueError("目录必须包含唯一的 hub.sqlite3 或 ledger/hub.sqlite3")
    return candidates[0], source


def summarize(source):
    database, run_dir = resolve(source)
    # URI escaping handles spaces, # and non-ASCII paths without creating a DB.
    with sqlite3.connect(database.as_uri() + "?mode=ro", uri=True) as connection:
        connection.row_factory = sqlite3.Row
        connection.execute("PRAGMA query_only=ON")
        connection.execute("BEGIN")
        jobs = [dict(r) for r in connection.execute("SELECT * FROM jobs ORDER BY id")]
        events = [dict(r) for r in connection.execute("SELECT * FROM events ORDER BY seq")]
        tables = {r[0] for r in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        deps = defaultdict(list)
        if "job_deps" in tables:
            for row in connection.execute("SELECT job,needs FROM job_deps"):
                deps[row[0]].append(row[1])

    malformed = []
    by_job = defaultdict(list)
    for event in events:
        try:
            payload = json.loads(event["payload"])
        except (ValueError, TypeError):
            malformed.append(event["seq"])
            payload = {}
        event["data"] = payload if isinstance(payload, dict) else {}
        by_job[event["job"]].append(event)

    output_jobs = []
    global_agy = {}
    global_claude = {}
    for job in jobs:
        rows = by_job[job["id"]]
        sessions = set()
        evidence = []
        controls = []
        claude = {}
        agy = {}
        duplicates = 0
        native_types = Counter()
        for event in rows:
            p = event["data"]
            if event["kind"] == "reply":
                for line in str(p.get("text", "")).splitlines():
                    match = CONTROL.match(line)
                    if match:
                        try:
                            value = json.loads(match[2])
                        except ValueError:
                            value = None
                        controls.append(dict(location(event), **control_metadata(match[1], value)))
            if event["kind"] != "native":
                continue
            native_type = p.get("type") or p.get("event") or "unknown"
            native_types[str(native_type)] += 1
            result = p.get("result") if p.get("event") == "result" else None
            result = result if isinstance(result, dict) else {}
            sid = (identity(p.get("session_id")) or identity(p.get("conversation_id"))
                   or identity(result.get("conversation_id")))
            step = p.get("step_update")
            if isinstance(step, dict):
                sid = sid or identity(step.get("conversation_id"))
            first = sid is not None and sid not in sessions
            if sid:
                sessions.add(sid)
            if first or native_type == "result":
                evidence.append(dict(location(event), native_type=native_type,
                                     session_id=sid, subtype=p.get("subtype"),
                                     is_error=p.get("is_error"),
                                     result_status=result.get("status")))
            if job.get("agent") == "claude" and native_type == "result" and p.get("parent_tool_use_id") is None:
                uuid = identity(p.get("uuid"))
                # Repeated terminal frames within one attempt cannot add usage twice.
                key = (sid, uuid) if uuid else (job["id"], event["turn"])
                duplicates += int(key in claude)
                sample = dict(location(event), uuid=uuid, session_id=sid,
                              tokens=tokens(p.get("usage"), "claude"))
                claude[key] = sample
                global_claude[key] = sample
            if job.get("agent") == "antigravity" and p.get("event") == "result" and sid:
                sample = dict(location(event), session_id=sid,
                              tokens=tokens(result.get("usage"), "antigravity"))
                for mapping in (agy, global_agy):
                    previous = mapping.get(sid)
                    old = previous["tokens"]["total_tokens"] if previous else None
                    new = sample["tokens"]["total_tokens"]
                    # Preserve high water if an older cumulative frame arrives late.
                    if previous is None or old is None or (new is not None and new >= old):
                        mapping[sid] = sample

        kinds = Counter(e["kind"] for e in rows)
        input_turns = {e["turn"] for e in rows if e["kind"] == "input"}
        result_turns = {e["turn"] for e in rows if e["kind"] == "native"
                        and (job.get("agent") != "claude" or e["data"].get("parent_tool_use_id") is None)
                        and (e["data"].get("type") == "result" or e["data"].get("event") == "result")}
        output_jobs.append({
            "id": job["id"], "parent": job.get("parent"), "agent": job.get("agent"),
            "state": job.get("state"), "turn": job.get("turn"), "after": deps[job["id"]],
            "ledger_session_id": job.get("session"), "native_session_ids": sorted(sessions),
            "native_session_count": len(sessions), "native_evidence": evidence,
            "native_event_types": dict(native_types), "control_lines": controls,
            "input_count": kinds["input"], "reply_count": kinds["reply"],
            "native_result_count": native_types["result"],
            "input_turns_without_native_result": sorted(input_turns - result_turns),
            "completion_claim_count": sum(e["data"].get("status") == "done" for e in rows if e["kind"] == "report"),
            "report_statuses": [e["data"].get("status") for e in rows if e["kind"] == "report"],
            "claude_main_loop_results": list(claude.values()), "claude_duplicate_results": duplicates,
            "agy_session_high_water": list(agy.values()),
        })

    checks = []
    keepalive = []
    for index, event in enumerate(events):
        if event["kind"] in ("check.result", "check.bounced"):
            p = event["data"]
            checks.append(dict(location(event), metadata={k: p[k] for k in
                               ("ok", "success", "status", "exit", "exit_code", "code", "attempt", "passed", "seconds") if k in p}))
        if "keepalive" in event["kind"]:
            p = event["data"]
            neighbors = [dict(location(e), native_type=e["data"].get("type") or e["data"].get("event"))
                         for e in events[max(0, index - 3):index + 4] if e["kind"] == "native"]
            keepalive.append(dict(location(event), metadata={k: p[k] for k in
                                  ("ok", "cache_read", "cache_write", "seconds") if k in p},
                                  nearby_native_events=neighbors))

    bad = any(j["state"] in ("failed", "cancelled", "partial", "interrupted") for j in output_jobs)
    result = {
        "database": str(database), "read_only": True,
        "assessment": "未通过" if bad else "功能验收未知，需独立核验",
        "event_counts": dict(Counter(e["kind"] for e in events)),
        "jobs": output_jobs, "platform_checks": checks, "keepalive": keepalive,
        "usage": {"claude_main_loop": aggregate([v["tokens"] for v in global_claude.values()]),
                  "agy_cumulative_session_high_water": aggregate([v["tokens"] for v in global_agy.values()]),
                  "scope": "仅已观察到的原生结果，缺结果任务未计入，不代表全工单用量",
                  "jobs_without_usage_result": [j["id"] for j in output_jobs
                                                if not j["claude_main_loop_results"] and not j["agy_session_high_water"]],
                  "cost_usd": None},
        "malformed_payload_sequences": malformed,
        "limitations": [
            "done、SA_REPORT 和模型自述均不能单独证明功能通过，未执行项目或验证产物。",
            "缺失用量保持 null，计数 0 仅表示该账本未观察到此类事件。",
            "Claude 仅计原生 result.usage 的每轮主循环，UUID 去重，缺 UUID 时按 job/turn 去重，内部子任务不计。",
            "AGY result.usage 是会话累计值，按 conversation_id 保留最新高水位，不跨轮累加，可能包含账本之外的历史。",
            "AGY 缺终态 result 的步骤未计，其他执行端用量未投影，故用量不能视为完整运行总量。",
            "保温依据专用事件单列，邻近事件仅供定位，无法由时间邻近证明原生用量属于保温。当前实现保温 native 流不入主账本。",
            "未计算成本，原生 total_cost_usd 为执行端累计标价估算，不是订阅账单。",
            "检查回执仅输出元数据，缺少可识别结果字段时保持未知。",
        ],
    }
    if run_dir is not None and (run_dir / "run.log").is_file():
        log = (run_dir / "run.log").read_text(encoding="utf-8", errors="replace")
        result["run_log"] = {"contains_failed_marker": bool(re.search(r"\bFAILED\b|exit=[1-9]", log)),
                             "contains_test_ok_marker": "test result: ok." in log,
                             "association_with_ledger": "未知，未证明 run.log 与账本属于同一次执行"}
        result["limitations"].append("run.log 与账本独立列示，日志成功或失败标记不能自动归属于账本。")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("--json", action="store_true", help="输出完整 JSON 证据")
    args = parser.parse_args()
    try:
        result = summarize(args.source)
    except (OSError, ValueError, sqlite3.Error) as error:
        parser.exit(2, f"无法分析指定账本 ({type(error).__name__})\n")
    if args.json:
        print(json.dumps(result, ensure_ascii=False, indent=2))
        return
    print(result["assessment"])
    for job in result["jobs"]:
        print(f"job={job['id']} parent={job['parent']} turn={job['turn']} state={job['state']} "
              f"inputs={job['input_count']} replies={job['reply_count']} "
              f"native_results={job['native_result_count']} done_claims={job['completion_claim_count']} "
              f"sessions={job['native_session_ids']} after={job['after']}")
        for line in job["control_lines"]:
            print("  " + json.dumps(line, ensure_ascii=False))
    print("platform_checks=" + json.dumps(result["platform_checks"], ensure_ascii=False))
    print("keepalive=" + json.dumps(result["keepalive"], ensure_ascii=False))
    print("usage=" + json.dumps(result["usage"], ensure_ascii=False))
    if "run_log" in result:
        print("run_log=" + json.dumps(result["run_log"], ensure_ascii=False))
    for limitation in result["limitations"]:
        print("- " + limitation)


if __name__ == "__main__":
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8")
    main()
