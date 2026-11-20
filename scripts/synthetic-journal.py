"""Fill an empty Super Agent journal with one large synthetic ticket.

Used to measure the desktop cold read without touching a real journal:

    scripts\\cargo.cmd run --release -p sa-store --example coldread -- --data DIR --init
    python -I scripts/synthetic-journal.py DIR --turns 400
    scripts\\cargo.cmd run --release -p sa-store --example coldread -- --data DIR

Payload text is synthetic. Frame kinds and the ratio of counts and sizes follow
the aggregate shape of a recorded multi-agent ticket (Codex lead with command
output deltas, a Claude collaborator with streamed partial messages).
"""

import argparse
import json
import os
import random
import sqlite3
import time

WORDS = "alpha beta gamma delta render chunk build cargo test module voxel shader item block".split()


def text(rng, size):
    out = []
    n = 0
    while n < size:
        w = rng.choice(WORDS)
        out.append(w)
        n += len(w) + 1
    return " ".join(out)[:size]


def codex_turn(rng, job, turn):
    yield "native", {"method": "turn/started", "params": {"turn": {"id": f"t{job}-{turn}"}}}
    for item in range(rng.randint(10, 30)):
        iid = f"exe-{job}-{turn}-{item}"
        yield "native", {"method": "item/started", "params": {"item": {"type": "commandExecution", "id": iid, "command": text(rng, 120)}}}
        for _ in range(rng.randint(20, 110)):
            yield "native", {"method": "item/commandExecution/outputDelta", "params": {"itemId": iid, "delta": text(rng, 220)}}
        big = 600_000 if rng.random() < 0.01 else rng.randint(1_000, 12_000)
        yield "native", {"method": "item/completed", "params": {"item": {"type": "commandExecution", "id": iid, "aggregatedOutput": text(rng, big), "exitCode": 0}}}
    for _ in range(rng.randint(100, 400)):
        d = text(rng, 40)
        yield "native", {"method": "item/agentMessage/delta", "params": {"delta": d}}
        yield "delta", d
    yield "native", {"method": "thread/tokenUsage/updated", "params": {"threadId": f"th{job}", "tokenUsage": {
        "total": {"totalTokens": turn * 1000, "inputTokens": turn * 900, "cachedInputTokens": turn * 800, "outputTokens": turn * 100, "reasoningOutputTokens": turn * 10},
        "last": {"totalTokens": 1000, "inputTokens": 900, "cachedInputTokens": 800, "outputTokens": 100, "reasoningOutputTokens": 10}}}}
    yield "native", {"method": "turn/completed", "params": {"turn": {"id": f"t{job}-{turn}", "status": "completed"}}}
    yield "reply", {"text": text(rng, 3000)}


def claude_turn(rng, job, turn):
    yield "native", {"type": "system", "subtype": "init", "session_id": f"s{job}", "model": "claude-opus-5-5"}
    for tool in range(rng.randint(5, 20)):
        tid = f"toolu_{job}_{turn}_{tool}"
        for _ in range(rng.randint(20, 80)):
            yield "native", {"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": text(rng, 30)}}, "parent_tool_use_id": None}
        yield "native", {"type": "assistant", "message": {"content": [{"type": "tool_use", "id": tid, "name": "Bash", "input": {"command": text(rng, 100)}}],
                                                              "usage": {"input_tokens": 10, "cache_read_input_tokens": 9000, "cache_creation_input_tokens": 100, "output_tokens": 1}}, "parent_tool_use_id": None}
        yield "native", {"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": tid, "content": text(rng, rng.randint(500, 20_000))}]}, "parent_tool_use_id": None}
    yield "native", {"type": "result", "subtype": "success", "result": text(rng, 400),
                     "usage": {"input_tokens": 50, "cache_read_input_tokens": 90000, "cache_creation_input_tokens": 1000, "output_tokens": 2000}}
    yield "reply", {"text": text(rng, 2000)}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("data")
    ap.add_argument("--turns", type=int, default=400)
    ap.add_argument("--seed", type=int, default=7)
    args = ap.parse_args()
    db = os.path.join(args.data, "hub.sqlite3")
    assert os.path.isfile(db), "run coldread --init first"
    live = os.path.join(os.environ.get("USERPROFILE", ""), ".local", "share", "superagent")
    assert not os.path.abspath(db).lower().startswith(os.path.abspath(live).lower()), "refusing live journal"
    rng = random.Random(args.seed)
    c = sqlite3.connect(db)
    now = time.time()
    c.execute("INSERT INTO tickets(id,goal,cwd,writes,lead,created) VALUES(1,'synthetic cold-read ticket','E:/synthetic',1,'codex',?)", (now,))
    c.execute("INSERT INTO jobs(id,ticket,parent,agent,body,writing,depth,turn,state) VALUES(1,1,NULL,'codex','lead',1,0,?,'done')", (args.turns,))
    c.execute("INSERT INTO jobs(id,ticket,parent,agent,body,writing,depth,turn,state) VALUES(2,1,1,'claude','collaborator',1,1,?,'done')", (args.turns,))
    seq = 0
    batch = []
    for turn in range(1, args.turns + 1):
        for job, gen in ((1, codex_turn), (2, claude_turn)):
            frames = [("turn.admitted", {"turn": turn})] + list(gen(rng, job, turn))
            for kind, payload in frames:
                seq += 1
                body = json.dumps(payload, separators=(",", ":"))
                batch.append((seq, now + seq * 0.001, 1, job, turn, kind, body, ""))
        if len(batch) > 50_000:
            c.executemany("INSERT INTO events(seq,ts,ticket,job,turn,kind,payload,sha256) VALUES(?,?,?,?,?,?,?,?)", batch)
            c.commit()
            batch.clear()
    c.executemany("INSERT INTO events(seq,ts,ticket,job,turn,kind,payload,sha256) VALUES(?,?,?,?,?,?,?,?)", batch)
    c.commit()
    n, size = c.execute("SELECT count(*), sum(length(payload)) FROM events").fetchone()
    print(f"{n} events, {size / 1e6:.0f} MB payload")


if __name__ == "__main__":
    main()
