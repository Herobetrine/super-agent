"""Opt-in paid check of full-access autonomous turns in a disposable project."""
import json
from pathlib import Path
import secrets
import sys
import tempfile
import time

sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
from engine import Engine, Store, TERMINAL


def main():
    base=Path(tempfile.mkdtemp(prefix="superagent-autonomy-"))
    project=base/"project"
    project.mkdir()
    nonce=secrets.token_hex(10)
    (project/"source.txt").write_bytes(nonce.encode())
    store=Store(base/"state")
    engine=Engine(store,timeout=180)
    goal=("在本临时项目内把 source.txt 完整复制为 delivery.txt，并取得另一名 agent 的独立文件校验。"
          "这是多轮验收。第一轮必须各输出一行 SA_GOAL 和 SA_NEXT，有效 JSON 分别包含 objective 和 body。"
          "向平台提交新的阶段目标并结束该轮，不能仅在文字中描述阶段目标。"
          "后续自行完成实施与委派，收齐实际验证结果再结束。"
          "最终回复中含有文件的确切内容。仅操作这个测试项目，无需外部发布。")
    tid=store.create(goal,project,"deepseek",permission_profile="auto")
    engine.start()
    print("isolated project",base,flush=True)
    try:
        last=None
        deadline=time.monotonic()+420
        while time.monotonic()<deadline:
            jobs=store.rows("SELECT id,agent,turn,state,result FROM jobs WHERE ticket=? ORDER BY id",(tid,))
            states=[(j["agent"],j["turn"],j["state"]) for j in jobs]
            if states!=last:
                print(states,flush=True)
                last=states
            if jobs[0]["state"] in TERMINAL:
                break
            time.sleep(.3)
        counts={r["kind"]:r["n"] for r in store.rows("SELECT kind,count(*) AS n FROM events GROUP BY kind")}
        exact=(project/"delivery.txt").is_file() and (project/"delivery.txt").read_bytes()==nonce.encode()
        ok=(jobs[0]["state"]=="done" and len(jobs)>1 and exact and nonce in (jobs[0]["result"] or "") and
            all(counts.get(k,0)>0 for k in ("auto.goal","auto.next","auto.finish")) and not counts.get("approval.request"))
        report=dict(passed=ok,exact_file=exact,states=states,events=counts)
        (base/"receipt.json").write_text(json.dumps(report,indent=2),encoding="utf-8")
        store.export(tid,base/"events.jsonl")
        print(json.dumps(report,ensure_ascii=False),flush=True)
        return 0 if ok else 1
    finally:
        if engine.active:
            engine.cancel(tid)
        engine.close()
        store.db.close()


if __name__=="__main__":
    raise SystemExit(main())
