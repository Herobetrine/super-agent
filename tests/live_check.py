"""Submit a four-backend acceptance ticket through a running desktop scheduler."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import secrets
import sys
import time

sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
from engine import Store, encode

def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--timeout",type=int,default=600)
    parser.add_argument("--claude-route",choices=("user","cc-switch"),help="复验旧工单时才指定旧版路由")
    args=parser.parse_args()
    state=Path(os.environ.get("SUPERAGENT_DATA") or Path.home()/".local/share/superagent/control-room")
    if not (state/"hub.sqlite3").is_file():
        raise SystemExit("请先打开桌面 Super Agent")
    store=Store(state,recover=False)
    project=state.parent/"checks"/secrets.token_hex(6)
    project.mkdir(parents=True)
    workers=("codex","claude","antigravity")
    tokens={name:secrets.token_hex(8) for name in workers}
    for name,token in tokens.items():
        (project/(name+".txt")).write_bytes(token.encode())
    commands=[]
    for name in workers:
        work=f"Read the file {name}.txt in this project using your native read tool. "
        if name=="codex":
            work+="Write the exact same ASCII bytes with no newline to handoff.txt in this project. "
        work+="Return exactly the token you read, without adding any labels, markdown or punctuation. Do not delegate."
        commands.append("SA_SEND "+encode(dict(id=name,to=name,write=name=="codex",body=work)))
    goal=("This is an operator-authorized live integration test. Only the root router follows this paragraph. "
          "Each child follows its own assignment only. On your first turn output exactly these three dispatch lines, "
          "one physical line per message, then end the turn.\n"+"\n".join(commands)+
          "\nOn your second turn, report the three actual returned tokens grouped by sender, preserving every character. "
          "Do not send more work. Report failures explicitly.")
    tid=store.create(goal,project,"deepseek",True,claude_route=args.claude_route)
    print(f"工单 #{tid}  测试项目 {project}",flush=True)
    status=None
    end=time.monotonic()+args.timeout
    while time.monotonic()<end:
        jobs=store.rows("SELECT id,parent,agent,state,result FROM jobs WHERE ticket=? ORDER BY id",(tid,))
        states=[(r["agent"],r["state"]) for r in jobs]
        if status!=states:
            print(states,flush=True)
            status=states
        if jobs[0]["state"] in ("done","partial","failed","interrupted","cancelled"):
            break
        time.sleep(.5)
    root=jobs[0]
    inputs=store.rows("SELECT payload FROM events WHERE job=? AND kind='input' ORDER BY seq DESC LIMIT 1",(root["id"],))
    reports={r["id"]:r["result"] for r in json.loads(inputs[0]["payload"])["reports"]} if inputs else {}
    receipt={}
    for name in workers:
        child=next((j for j in jobs if j["agent"]==name and j["parent"]==root["id"]),{})
        actual=child.get("result") or ""
        native=None
        for row in store.rows("SELECT payload FROM events WHERE job=? AND kind='native' ORDER BY seq",(child.get("id"),)):
            event=json.loads(row["payload"])
            if name=="claude" and event.get("type")=="result":
                native=event.get("result")
            if name=="antigravity" and event.get("event")=="result":
                native=event["result"].get("response")
            if name=="codex" and event.get("method")=="item/completed":
                item=event["params"]["item"]
                if item.get("type")=="agentMessage" and item.get("phase")!="commentary":
                    native=item.get("text")
        receipt[name]=dict(state=child.get("state"),exact_reply=actual==tokens[name],
                           passed_file_challenge=actual in (tokens[name], tokens[name]+"\n", tokens[name]+"\r\n"),
                           native_to_router_exact=isinstance(native,str) and native==actual==reports.get(child.get("id")),
                           reached_router=tokens[name] in (root["result"] or ""),
                           reply_sha256=hashlib.sha256(actual.encode()).hexdigest())
    output=project/"handoff.txt"
    written=output.is_file() and output.read_bytes()==tokens["codex"].encode()
    export=project/"events.jsonl"
    store.export(tid,export)
    ok=(root["state"]=="done" and written and
        all(r["state"]=="done" and r["passed_file_challenge"] and r["native_to_router_exact"]
            and r["reached_router"] for r in receipt.values()))
    record=dict(ticket=tid,router="deepseek",claude_route=args.claude_route,state=root["state"],checks=receipt,
                exact_file=written,verified_export=str(export),passed=ok)
    (project/"receipt.json").write_text(json.dumps(record,ensure_ascii=False,indent=2),encoding="utf-8")
    print(json.dumps(record,ensure_ascii=False,indent=2))
    return 0 if ok else 1

if __name__=="__main__":
    raise SystemExit(main())
