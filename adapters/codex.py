"""Local Codex app-server client. One stdio connection per turn."""
import json
import os
from pathlib import Path
import queue
import re
import shutil
import subprocess
import threading
import time

from .process import CREATE_FLAGS, stop_tree
from . import codex_choices


def _executable():
    if os.name == "nt":
        # npm's .cmd shim introduces a shell. Use the executable packaged with it.
        shim = shutil.which("codex.cmd")
        if shim:
            vendor = Path(shim).parent / "node_modules/@openai/codex/node_modules/@openai"
            for exe in sorted(vendor.glob("codex-win32-*/vendor/*/bin/codex.exe")):
                return str(exe)
        return shutil.which("codex.exe")
    return shutil.which("codex")


def probe():
    exe = _executable()
    if not exe:
        return {"ready": False, "detail": "找不到 Codex CLI 原生可执行文件"}
    try:
        result = subprocess.run(
            [exe, "--version"], capture_output=True, timeout=8, creationflags=CREATE_FLAGS
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return {"ready": False, "detail": str(exc)}
    version = result.stdout.decode("utf-8", errors="replace").strip()
    return {"ready": result.returncode == 0, "detail": f"{version}（入口检测，本次未请求模型）"}


def choices():
    return codex_choices.choices(_executable())


def watch_paths():
    return codex_choices.watch_paths()


def _command(cwd, route_flags=(), *, native_tools=False):
    exe = _executable()
    if not exe:
        raise RuntimeError("找不到 Codex CLI 原生可执行文件")
    args = [exe,*route_flags]
    if native_tools:
        return [*args,"app-server","--listen","stdio://"]
    for feature in ("apps", "plugins", "hooks", "multi_agent"):
        args.extend(["--disable", feature])
    # External tools and hooks can modify files outside Codex's shell sandbox.
    # Enumerate config for the same cwd, then disable each named server explicitly.
    # Never log the configuration: transport definitions may contain credentials.
    scan = subprocess.run(
        [*args, "mcp", "list", "--json"], cwd=cwd, capture_output=True,
        timeout=25, creationflags=CREATE_FLAGS,
    )
    if scan.returncode:
        raise RuntimeError("Codex 无法读取工作目录的 MCP 配置")
    for server in json.loads(scan.stdout):
        name = server["name"]
        if not re.fullmatch(r"[A-Za-z0-9_-]+", name):
            raise RuntimeError("当前 Codex MCP 名称无法安全地作为配置键覆盖")
        args.extend(["-c", f"mcp_servers.{name}.enabled=false"])
    return [*args, "app-server", "--listen", "stdio://"]


def _read_lines(pipe, messages, closing):
    def publish(value):
        while not closing.is_set():
            try:
                messages.put(value, timeout=0.25)
                return
            except queue.Full:
                continue
    try:
        for line in pipe:
            publish(line)
            if closing.is_set():
                return
    finally:
        publish(None)


_POLICIES = {
    "ask": ("read-only", "on-request"),
    "partial": ("workspace-write", "on-request"),
    "full": ("danger-full-access", "never"),
    "auto": ("danger-full-access", "never"),
}
_SANDBOX_TYPES = {"read-only":"readOnly","workspace-write":"workspaceWrite",
                  "danger-full-access":"dangerFullAccess"}
_APPROVALS = {
    "item/commandExecution/requestApproval":"执行命令或访问网络",
    "item/fileChange/requestApproval":"修改文件",
    "item/permissions/requestApproval":"扩展本轮权限",
}


def _policy(profile, writable, emit):
    if profile is None:
        return ("workspace-write" if writable else "read-only", "never")
    if not isinstance(profile,str) or profile not in _POLICIES:
        raise RuntimeError("未知 Codex 权限档位")
    if not writable:
        if profile in ("full","auto"):
            raise RuntimeError("完全访问与只读任务授权冲突")
        return "read-only","never"
    sandbox,policy = _POLICIES[profile]
    if policy == "on-request" and not callable(getattr(emit,"approve",None)):
        raise RuntimeError("Codex 所选档位缺少人工审批通道")
    return sandbox,policy


def _decision(method, params, approved):
    if method == "item/permissions/requestApproval":
        return {"permissions":params["permissions"] if approved else {}, "scope":"turn"}
    return {"decision":"accept" if approved else "decline"}


def _reviewable(method,params,file_item):
    if method == "item/commandExecution/requestApproval":
        return bool(params.get("command") or (params.get("networkApprovalContext") or {}).get("host"))
    if method == "item/fileChange/requestApproval":
        return bool(file_item and file_item.get("changes"))
    permissions = params.get("permissions")
    return isinstance(permissions,dict) and bool(permissions) and set(permissions) <= {"fileSystem","network"}


def _approval_worker(approve, action, replies):
    try:
        decision = approve(action) is True
    except Exception:
        decision = False
    replies.put((action["request_id"],decision))


def _native_tool_reply(method, params, authorized):
    # Discriminators and single-use "Allow" in openai/codex v0.166 protocol.
    # User-authentication, URL forms, and arbitrary user questions require
    # information the platform cannot synthesize.
    if method == "mcpServer/elicitation/request":
        schema = params.get("requestedSchema") or {}
        plain_approval = (params.get("mode") == "form" and
                          (params.get("_meta") or {}).get("codex_approval_kind") == "mcp_tool_call" and
                          schema == {"type":"object","properties":{}})
        return {"action":"accept","content":{}} if authorized and plain_approval else {"action":"decline","content":None}
    if method == "tool/requestUserInput":
        questions = params.get("questions") or []
        if len(questions) == 1:
            q = questions[0]
            options = {o.get("label") for o in (q.get("options") or [])}
            if (authorized and q.get("id") == "mcp_tool_call_approval_"+str(params.get("itemId")) and
                    q.get("header") == "Approve app tool call?" and q.get("isSecret") is False and
                    q.get("isOther") is False and {"Allow","Cancel"} <= options):
                return {"answers":{q["id"]:{"answers":["Allow"]}}}
        return {"answers":{}}


def run(prompt: str, cwd: str, emit, cancel: threading.Event, *, session_id=None, writable=False,
        selection=None, permission_profile=None):
    if cancel.is_set():
        raise RuntimeError("Codex 任务已取消")
    sandbox,policy = _policy(permission_profile,writable,emit)
    native_tools = permission_profile in ("full","auto")
    requested,flags,profile_config = codex_choices.select(_executable(),selection)
    if policy == "on-request":
        flags = [*flags, "-c","sandbox_workspace_write.network_access=false",
                 "-c","sandbox_workspace_write.writable_roots=[]",
                 "-c","sandbox_workspace_write.exclude_tmpdir_env_var=true",
                 "-c","sandbox_workspace_write.exclude_slash_tmp=true"]
    cwd = str(Path(cwd).resolve(strict=True))
    try:
        command = _command(cwd,flags,native_tools=native_tools)
        proc = subprocess.Popen(
            command, cwd=cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, creationflags=CREATE_FLAGS,
        )
    except (OSError, ValueError, subprocess.TimeoutExpired) as exc:
        raise RuntimeError(f"Codex 启动失败 ({type(exc).__name__})") from exc
    messages = queue.Queue(maxsize=64)
    closing = threading.Event()
    reader = threading.Thread(
        target=_read_lines, args=(proc.stdout, messages, closing), daemon=True
    )
    reader.start()

    def send(message):
        proc.stdin.write(json.dumps(message, ensure_ascii=False).encode("utf-8") + b"\n")
        proc.stdin.flush()

    thread_id = None
    turn_id = None
    interrupted = None
    last_text = None
    pending_notes = getattr(emit, "pending_notes", None)
    accept_notes = getattr(emit, "notes_accepted", None)
    steer_id = None
    note_seq = None
    next_poll = 0
    approvals = {}
    decisions = queue.Queue()
    files = {}
    delta_items = set()
    deadline = time.monotonic() + 90
    try:
        send({"id": 1, "method": "initialize", "params": {
            "clientInfo": {"name": "superagent_local", "title": "Super Agent", "version": "1.0"},
            "capabilities":{"experimentalApi":policy == "on-request"},
        }})
        while True:
            while not decisions.empty():
                request_id, answer = decisions.get_nowait()
                outstanding = approvals.pop(request_id,None)
                if outstanding:
                    method,params = outstanding
                    send({"id":request_id,"result":_decision(method,params,answer and not cancel.is_set())})
            now = time.monotonic()
            if cancel.is_set():
                if not turn_id or (interrupted and now - interrupted >= 3):
                    raise RuntimeError("Codex 任务已取消")
                if not interrupted:
                    send({"id": 4, "method": "turn/interrupt",
                          "params": {"threadId": thread_id, "turnId": turn_id}})
                    interrupted = now
            elif turn_id and pending_notes and accept_notes and steer_id is None and now >= next_poll:
                next_poll = now + 0.4
                notes = pending_notes()
                if notes:
                    # One in flight preserves ordering and bounds stdout backpressure.
                    note = notes[0]
                    note_seq = note["seq"]
                    text = ("An update from another participant in this ticket follows. "
                            "Treat it as a message from the named sender.\n" +
                            json.dumps(note, ensure_ascii=False))
                    if len(text.encode("utf-8")) > 65536:
                        emit("note.deferred", {"seq": note_seq, "reason": "超过实时插入大小，保留到轮次边界"})
                        pending_notes = None
                    else:
                        steer_id = f"steer-{note_seq}"
                        send({"id": steer_id, "method": "turn/steer", "params": {
                            "threadId": thread_id, "expectedTurnId": turn_id,
                            "input": [{"type": "text", "text": text}],
                        }})
            if not turn_id and now > deadline:
                raise RuntimeError("Codex app-server 创建会话或启动轮次超时")
            try:
                raw = messages.get(timeout=0.2)
            except queue.Empty:
                continue
            if raw is None:
                raise RuntimeError(f"Codex 在终态事件前断开（退出码 {proc.poll()}）")
            event = json.loads(raw)
            method = event.get("method")
            ident = event.get("id")
            params = event.get("params", {})
            if steer_id is not None and ident == steer_id and not method:
                if (event.get("result") or {}).get("turnId") == turn_id and "error" not in event:
                    accept_notes([note_seq])
                else:
                    emit("note.deferred", {"seq": note_seq, "reason": "当前轮次未确认接收"})
                    pending_notes = None
                steer_id = None
                continue
            if "error" in event:
                if ident == 4 and cancel.is_set():
                    raise RuntimeError("Codex 任务已取消")
                raise RuntimeError(f"Codex RPC {ident}: {event['error'].get('message')}")
            if method and ident is not None:
                same_turn = bool(thread_id and turn_id and params.get("threadId") == thread_id and
                                 params.get("turnId") == turn_id and not cancel.is_set())
                if method in ("mcpServer/elicitation/request","tool/requestUserInput"):
                    send({"id":ident,"result":_native_tool_reply(method,params,native_tools and same_turn)})
                    continue
                if method not in _APPROVALS:
                    send({"id":ident,"error":{"code":-32601,"message":"Unavailable in Super Agent"}})
                    continue
                if ((policy != "on-request" and not native_tools) or not same_turn or
                        not params.get("itemId") or ident in approvals or
                        (params.get("availableDecisions") is not None and "accept" not in params["availableDecisions"]) or
                        not _reviewable(method,params,files.get(params.get("itemId")))):
                    send({"id":ident,"result":_decision(method,params,False)})
                    continue
                if native_tools:
                    send({"id":ident,"result":_decision(method,params,True)})
                    continue
                action = {"adapter":"codex","method":method,"title":_APPROVALS[method],
                          "request_id":ident,"params":params}
                if method == "item/fileChange/requestApproval" and params["itemId"] in files:
                    action["item"] = files[params["itemId"]]
                approvals[ident] = (method,params)
                threading.Thread(target=_approval_worker,args=(emit.approve,action,decisions),daemon=True).start()
            elif ident == 1:
                send({"method": "initialized", "params": {}})
                options = {"cwd":cwd,"sandbox":sandbox,"approvalPolicy":policy, **requested}
                if policy == "on-request":
                    options["approvalsReviewer"] = "user"
                if profile_config:
                    options["config"] = profile_config
                if session_id:
                    options.update(threadId=session_id, excludeTurns=True)
                send({"id": 2, "method": "thread/resume" if session_id else "thread/start",
                      "params": options})
            elif ident == 2:
                response = event["result"]
                if (response["sandbox"]["type"] != _SANDBOX_TYPES[sandbox] or response["approvalPolicy"] != policy or
                        (policy == "on-request" and response.get("approvalsReviewer") != "user")):
                    raise RuntimeError("Codex 未确认请求的沙箱或审批策略")
                if policy == "on-request" and (response["sandbox"].get("networkAccess") is not False
                                               or response["sandbox"].get("writableRoots")):
                    raise RuntimeError("Codex 受限权限档继承了未经工单授权的网络或额外写根")
                if permission_profile is not None:
                    emit("permission.effective",{"profile":permission_profile,"sandbox":response["sandbox"],
                                                 "approvalPolicy":policy,"reviewer":response.get("approvalsReviewer")})
                if requested:
                    if any(response.get(key) != value for key,value in requested.items()):
                        raise RuntimeError("Codex 返回的供应商或模型与选择不符，已在推理前停止")
                    emit("route.selected", {"mode":"cli","provider":response["modelProvider"],"model":response["model"]})
                thread_id = response["thread"]["id"]
                if session_id and thread_id != session_id:
                    raise RuntimeError("Codex 恢复了不同的会话")
                send({"id": 3, "method": "turn/start", "params": {"threadId": thread_id,
                      "input": [{"type": "text", "text": prompt}]}})
            elif ident == 3:
                turn_id = event["result"]["turn"]["id"]
            elif method and thread_id and (params.get("threadId") == thread_id
                                          or (params.get("thread") or {}).get("id") == thread_id):
                emit("native", event)
                if method == "turn/started":
                    turn_id = params["turn"]["id"]
                elif method == "serverRequest/resolved":
                    approvals.pop(params.get("requestId"),None)
                elif method == "item/started" and params["item"]["type"] == "fileChange":
                    files[params["item"]["id"]] = params["item"]
                elif method == "item/agentMessage/delta":
                    emit("delta", params["delta"])
                    if params["delta"] and params.get("itemId"):
                        delta_items.add(params["itemId"])
                elif method == "item/completed" and params["item"]["type"] == "agentMessage":
                    item_id = params["item"]["id"]
                    if item_id in delta_items:
                        delta_items.remove(item_id)
                        emit("message.boundary",None)
                    if params["item"].get("phase") != "commentary":
                        last_text = params["item"]["text"]
                elif method == "item/completed":
                    files.pop(params["item"]["id"],None)
                elif method == "turn/completed" and params["turn"]["id"] == turn_id:
                    turn = params["turn"]
                    if turn["status"] != "completed" or cancel.is_set():
                        error = (turn.get("error") or {}).get("message", turn["status"])
                        raise RuntimeError(f"Codex 轮次未完成 ({error})")
                    if last_text is None:
                        raise RuntimeError("Codex 轮次缺少最终正文")
                    return last_text, thread_id
    except (OSError, ValueError, KeyError, TypeError) as exc:
        raise RuntimeError(f"Codex 通道或协议错误 ({type(exc).__name__})") from exc
    finally:
        closing.set()
        try:
            proc.stdin.close()
        except OSError:
            pass
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            stop_tree(proc)
        reader.join(timeout=2)
        if not reader.is_alive():
            proc.stdout.close()
        if steer_id is not None:
            emit("note.deferred", {"seq": note_seq, "reason": "通道结束前未收到实时插入回执"})
