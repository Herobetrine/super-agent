"""Public ticket permission contract. Native adapters enforce the granted profile."""
PROFILES = {
    "readonly": ("只读", "读取资料和分析项目，禁用项目写入。"),
    "ask": ("寻求审批", "读操作自动执行。Codex 写入或突破沙箱时申请批准；DeepSeek 在取得本轮工作区写入授权后执行。"),
    "partial": ("部分审批", "自动执行受控工作区内的读写。Codex 对需要额外权限的操作申请批准；其他受限端拒绝越界操作。"),
    "full": ("完全访问", "所有参与者可用当前系统用户权限读写文件、运行命令、访问网络。无需逐次审批。"),
    "auto": ("自动模式 · 高危", "主控完成阶段后检查项目、记录经验并选择下一项工作，直到你停止或实查已无可推进事项。"),
}
UNRESTRICTED = frozenset(("full","auto"))
AUTHORITY = ("readonly","ask","partial","full","auto")


def child_profiles(parent):
    return list(AUTHORITY[:AUTHORITY.index(parent)+1]) if parent in AUTHORITY else ["readonly"]


DISCLAIMER = ("高危授权与免责声明：参与的 agent 可访问当前系统用户有权访问的文件、网络服务及本机凭据，"
              "可能删除或覆盖数据、对外传输项目内容，并产生模型或服务费用。"
              "新自动工单不设累计轮数上限，可持续调用并计费；完成一个阶段后仍会主动探查项目。仅在可信环境启用，需要时请点击停止。"
              "软件按原样提供，在适用法律允许的范围内不提供担保。")
AUTO_GUIDE = """
Autonomous project mode is active for this ticket. The original human request and later user_updates define the mandate.
You have full access as the current OS user. Do not ask for routine approval. Make reasonable in-scope decisions,
use the actual workspace and tools, delegate to any suitable harness, inspect reports, test results and repair failures.
Keep declared write intent accurate for scheduling. Do not confuse a peer's claim with independent verification.
Only the primary agent can revise the working objective, request another cycle, or close the project.
At the end of each primary reply, use separate single-line JSON control messages:
SA_GOAL {"objective":"updated milestone within the human's project mandate"} (optional).
Whenever you change the working objective or say it has changed, emit SA_GOAL on its own line to persist it.
Prose and tool-local checklists do not update the platform objective.
SA_NEXT {"body":"concrete next action, with what is already verified"} to continue autonomously.
SA_FINISH {"status":"done","summary":"verified result and evidence"} after the actual goal is fulfilled,
or SA_FINISH {"status":"blocked","summary":"specific irrecoverable external blocker and what was tried"}.
If a child failed, was cancelled, or only partly completed, verify the work yourself or assign a replacement.
For a done result, record each compensated direct child in the SA_FINISH "resolved" array as {"job":123,"evidence":"specific verification"}.
Wait for outstanding work before final closure. Keep a human-readable report in the reply.
The platform will supply final peer reports and continue without human prompting. On recovery, inspect prior events,
current files and existing subtasks before acting; an interrupted call may already have produced side effects.
"""


def controls(text,*,continuous=False):
    """Only a successful primary final reply commits goal and lifecycle controls."""
    import json
    parsed={}
    for line in text.split("\n"):
        tag,sep,raw=line.partition(" ")
        fields={"SA_GOAL":"objective","SA_NEXT":"body","SA_FINISH":"summary"}
        if continuous:
            fields.update(SA_LEARN="lesson",SA_EXHAUSTED="summary")
        if not sep or tag not in fields:
            continue
        data=json.loads(raw)
        field=fields[tag]
        if (not isinstance(data,dict) or not isinstance(data.get(field),str) or
            not data[field].strip() or len(data[field])>32000 or tag in parsed):
            raise ValueError(f"{tag} 缺少有效且唯一的 {field}")
        if tag in ("SA_FINISH","SA_EXHAUSTED") and data.get("status") not in ("done","blocked"):
            raise ValueError(f"{tag} 需要 done 或 blocked")
        if tag=="SA_EXHAUSTED":
            for key in ("checks","alternatives"):
                items=data.get(key)
                if not isinstance(items,list) or not 0<len(items)<=32 or any(
                    not isinstance(v,str) or not v.strip() or len(v)>4000 for v in items):
                    raise ValueError("SA_EXHAUSTED 需要检查结果 checks 和具体备选方案 alternatives")
        if tag in ("SA_FINISH","SA_EXHAUSTED"):
            resolved=data.get("resolved",[])
            if not isinstance(resolved,list) or any(not isinstance(r,dict) or type(r.get("job")) is not int
                    or not isinstance(r.get("evidence"),str) or not r["evidence"].strip() for r in resolved):
                raise ValueError("SA_FINISH.resolved 需要逐项提供任务编号和核验证据")
        parsed[tag]=data
    if "SA_FINISH" in parsed and "SA_NEXT" in parsed:
        raise ValueError("同轮不能同时结束并继续")
    if "SA_EXHAUSTED" in parsed and set(parsed)&{"SA_GOAL","SA_NEXT","SA_FINISH"}:
        raise ValueError("耗尽报告不能与派定新目标或结束阶段同时提交")
    return parsed
