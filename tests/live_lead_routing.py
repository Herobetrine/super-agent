"""Optional billable probe: real lead chooses a non-default Claude model."""
import json
from pathlib import Path
import secrets
import sys
import tempfile
import time

sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
from adapters import claude, deepseek
from engine import Engine, Store, TERMINAL


def main():
    cli=claude.choices()
    route=cli["default"]
    provider=next(p for p in cli["providers"] if p["id"]==route["provider"])
    alternatives={m["id"] for m in provider["models"] if m["id"]!=route["model"] and m.get("available",True)}
    if route["mode"] != "cli" or not provider.get("available",True) or not alternatives:
        raise RuntimeError("当前 Claude Code 供应商没有可用于核验的非默认 CLI 模型")
    root=Path(tempfile.mkdtemp(prefix="superagent-lead-routing-"))
    project=root/"project"
    project.mkdir()
    answer=secrets.token_hex(12)
    (project/"challenge.txt").write_bytes(answer.encode("ascii"))
    hub=Store(root/"state")
    engine=Engine(hub,capacity=2,timeout=140)
    goal=("这是一个只读取证测试。下面的调度要求仅适用于 depth=0 的主控；子任务严格执行自己的 assignment。"
          "主控第一轮请从 delegate_models.claude.models 中自行选择一个适合简单文件读取、且与该 Claude "
          "目录的 default_model 不同的型号。不要自己打开文件。只发一条 SA_SEND 给 claude，设置 model 为你选中的"
          "完整 ID，permission_profile 为 readonly，write 为 false。请 Claude 使用原生文件读取工具读取 "
          "challenge.txt，只返回完整随机字符串，不加格式或说明。主控等待子任务；在第二轮核对并原样报告收到的随机字符串，"
          "不再派单。如果子任务失败，如实报告。")
    ticket=hub.create(goal,project,"deepseek",selections={"deepseek":deepseek.choices()["default"],"claude":route},
                      permission_profile="partial",allow_lead_routing=True)
    print(f"fixture={root} ticket={ticket} fixed Claude provider={route['provider']} default={route['model']}",flush=True)
    engine.start()
    try:
        deadline=time.monotonic()+210
        while time.monotonic()<deadline:
            jobs=hub.rows("SELECT * FROM jobs WHERE ticket=? ORDER BY id",(ticket,))
            if jobs[0]["state"] in TERMINAL:
                break
            time.sleep(.25)
        jobs=hub.rows("SELECT * FROM jobs WHERE ticket=? ORDER BY id",(ticket,))
        child=next((j for j in jobs[1:] if j["agent"]=="claude"),{})
        selected=json.loads(child.get("route_override") or "{}")
        chosen=selected.get("model")
        native=[json.loads(e["payload"]) for e in hub.rows(
            "SELECT payload FROM events WHERE job=? AND kind='native' ORDER BY seq",(child.get("id"),))]
        init=[e.get("model") for e in native if e.get("type")=="system" and e.get("subtype")=="init"]
        assistant=[e["message"] for e in native if e.get("type")=="assistant" and e.get("message")]
        results=[e for e in native if e.get("type")=="result" and e.get("subtype")=="success"]
        reads=[x for m in assistant for x in m.get("content",[]) if isinstance(x,dict) and x.get("type")=="tool_use"
               and x.get("name")=="Read" and str(x.get("input",{}).get("file_path","")).endswith("challenge.txt")]
        reports=[r for e in hub.rows("SELECT payload FROM events WHERE job=? AND kind='input' ORDER BY seq",
                                    (jobs[0]["id"],)) for r in json.loads(e["payload"]).get("reports",[])
                 if r["id"]==child.get("id")]
        tests={
            "lead_done":jobs[0]["state"]=="done",
            "one_child_done":len(jobs)==2 and child.get("state")=="done",
            "same_provider_and_mode":selected.get("provider")==route["provider"] and selected.get("mode")==route["mode"],
            "non_default_model":chosen in alternatives,
            "restricted_child":child.get("permission_override")=="readonly",
            "native_init_model":bool(chosen and init and str(init[-1]).removesuffix("[1M]")==chosen),
            "native_response_model":bool(chosen and assistant and assistant[-1].get("model")==chosen),
            "native_read_tool":bool(reads),
            "exact_native_handoff":bool(results and reports and results[-1]["result"]==answer==
                                        child.get("result")==reports[-1]["result"]),
            "lead_has_file_token":answer in (jobs[0]["result"] or ""),
        }
        receipt=dict(passed=all(tests.values()),checks=tests,requested=selected,
                     native_init=init[-1:] if init else [],native_assistant=assistant[-1].get("model") if assistant else None,
                     default_model=route["model"],states=[(j["id"],j["agent"],j["state"]) for j in jobs])
        hub.export(ticket,root/"events.jsonl")
        (root/"receipt.json").write_text(json.dumps(receipt,indent=2),encoding="utf-8")
        print(json.dumps(receipt),flush=True)
        return 0 if receipt["passed"] else 1
    finally:
        if engine.active:
            engine.cancel(ticket)
        engine.close()
        hub.db.close()


if __name__=="__main__":
    sys.exit(main())
