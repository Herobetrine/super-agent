"""Opt-in paid check of full-access autonomous turns in a disposable project."""
import json
import hashlib
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
          "后续自行实施与委派并收齐验证结果。把首项目标的结果写入 SA_FINISH 里程碑；里程碑不关闭工单。"
          "测试程序会在首个里程碑提交后在当前临时项目放入 PROJECT_INBOX.md。届时在 discover 阶段用原生工具"
          "实际查看项目及文件内容，自行提出有 evidence 的 SA_GOAL 和下一步 SA_NEXT，解决其中存在的实际需求。"
          "工作中用 SA_LEARN 的 lesson 记录一个可复用的真实经验。每个已验证阶段提交 SA_FINISH。"
          "最后再次用原生工具检查这个项目有无可行事项。只有经过检查确实无可推进工作时才"
          "提交 SA_EXHAUSTED，带 status、summary、具体 checks 和 alternatives；最后回复中含有 source.txt 的确切内容。"
          "这些控制要求只针对主控，协作者遵循各自 assignment。仅操作这个测试项目，无需外部发布。")
    tid=store.create(goal,project,"deepseek",permission_profile="auto")
    print("isolated project",base,flush=True)
    try:
        last=None
        added=False
        deadline=time.monotonic()+420
        while time.monotonic()<deadline:
            # Single scheduler pump. Hold the same locks in the same order as
            # Engine.step: the first discovery pass can only start *after* the
            # new fixture is on disk and the preceding milestone has committed.
            with engine.guard,store.lock:
                if not added and store.rows("SELECT seq FROM events WHERE ticket=? AND kind='auto.milestone' LIMIT 1",(tid,)):
                    inbox=project/"PROJECT_INBOX.md"
                    inbox.write_text("验收清单\n\n当 delivery.txt 已交付，请新增 delivery.sha256，内容为该文件原始字节的"
                                     " SHA-256，要求 64 位小写 ASCII 十六进制，无 BOM、空格或结尾换行。"
                                     "如果它已存在并精确符合要求，此项目清单已完成，没有其他待处理条目。\n",
                                     encoding="utf-8")
                    added=True
                engine.step()
            jobs=store.rows("SELECT id,agent,turn,state,result FROM jobs WHERE ticket=? ORDER BY id",(tid,))
            states=[(j["agent"],j["turn"],j["state"]) for j in jobs]
            if states!=last:
                print(states,flush=True)
                last=states
            if jobs[0]["state"] in TERMINAL:
                break
            time.sleep(.05)
        counts={r["kind"]:r["n"] for r in store.rows("SELECT kind,count(*) AS n FROM events GROUP BY kind")}
        exact=(project/"delivery.txt").is_file() and (project/"delivery.txt").read_bytes()==nonce.encode()
        target=project/"delivery.sha256"
        second=added and target.is_file() and target.read_bytes()==hashlib.sha256(nonce.encode()).hexdigest().encode("ascii")
        inputs=[json.loads(r["payload"]) for r in store.rows(
            "SELECT payload FROM events WHERE job=? AND kind='input' ORDER BY seq",(jobs[0]["id"],))]
        inspected=any(p.get("autonomy",{}).get("phase")=="discover" for p in inputs)
        learned=counts.get("auto.learn",0)>0 and any(bool(p.get("autonomy",{}).get("learning")) for p in inputs)
        discovered=[json.loads(r["payload"]) for r in store.rows(
            "SELECT payload FROM events WHERE ticket=? AND kind='auto.discovered'",(tid,))]
        exhausted=[json.loads(r["payload"]) for r in store.rows(
            "SELECT payload FROM events WHERE ticket=? AND kind='auto.exhausted'",(tid,))]
        discovery_proven=any(e.get("probe_seq") and e.get("evidence") for e in discovered)
        final_proven=len(exhausted)==1 and exhausted[0].get("status")=="done" and bool(exhausted[0].get("last_success_seq"))
        ok=(jobs[0]["state"]=="done" and len(jobs)>1 and exact and second and inspected and learned and
            nonce in (jobs[0]["result"] or "") and counts.get("auto.milestone",0)>=2 and
            all(counts.get(k,0)>0 for k in ("auto.goal","auto.next")) and
            discovery_proven and final_proven and not counts.get("approval.request"))
        report=dict(passed=ok,exact_file=exact,exact_discovered_deliverable=second,discovery_seen=inspected,
                    persisted_learning=learned,discovery_proven=discovery_proven,exhaustion_proven=final_proven,
                    states=states,events=counts)
        store.export(tid,base/"events.jsonl")
        (base/"receipt.json").write_text(json.dumps(report,indent=2),encoding="utf-8")
        print(json.dumps(report,ensure_ascii=False),flush=True)
        return 0 if ok else 1
    finally:
        if engine.active or any(j["state"] not in TERMINAL for j in store.rows("SELECT state FROM jobs WHERE ticket=?",(tid,))):
            engine.cancel(tid)
        engine.close()
        store.db.close()


if __name__=="__main__":
    raise SystemExit(main())
