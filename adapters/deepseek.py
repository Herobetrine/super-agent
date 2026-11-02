"""Drive the pinned upstream DeepSeek Harness headless profile over NDJSON."""
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import tempfile
import threading

from .process import CREATE_FLAGS, stream_lines
from . import deepseek_choices as catalog
from .native_permissions import scope_for


VERSION = "0.2.1-rc.5"


def _platform_root():
    return catalog.platform_dir()


def watch_paths():
    return catalog.watch_paths()


def _command():
    node = shutil.which("node")
    cli = _platform_root() / "dsh-runtime/node_modules/@deepseek-ai/dsh"
    if not node or not (cli / "lib/bin.js").is_file():
        raise RuntimeError(f"找不到平台专用 DeepSeek Harness。请按 docs/deepseek.md 安装 {VERSION}")
    if json.loads((cli / "package.json").read_bytes())["version"] != VERSION:
        raise RuntimeError(f"平台专用 DeepSeek Harness 版本不符，预期 {VERSION}")
    return [node, "--expose-internals", str(cli / "lib/bin.js")]


def choices():
    try:
        _command()
        available = True
    except (OSError,ValueError,RuntimeError):
        available = False
    return catalog.choices(available)


def probe():
    try:
        proc = subprocess.run([*_command(), "--version"], capture_output=True,
                              timeout=10, creationflags=CREATE_FLAGS)
        if proc.returncode:
            raise RuntimeError(f"DSH 启动失败（exit {proc.returncode}）")
        rid,model,route = catalog.resolve(None)
        return {"ready": True, "detail": f"DSH {VERSION} · {route['label']} {rid}/{model}（默认入口检测，未请求模型）"}
    except (OSError, RuntimeError, ValueError, sqlite3.Error, subprocess.TimeoutExpired) as exc:
        return {"ready": False, "detail": str(exc)}


def run(prompt: str, cwd: str, emit, cancel: threading.Event, *, session_id=None, writable=False,
        selection=None, permission_profile=None):
    mode = scope_for("deepseek",permission_profile,writable,cwd,emit,cancel,workspace_writer=True)
    rid,model,route = catalog.resolve(selection)
    # Distinct persistent homes prevent a write-enabled Session from being
    # adopted under the read-only route. DSH also checks the recorded cwd.
    state = _platform_root() / "dsh-state" / mode
    env = {**os.environ, "DSH_HOME": str(state), "DSH_PERMISSION_MODE": mode}
    final = None
    identity = None

    def on_line(raw):
        nonlocal final, identity
        try:
            event = json.loads(raw)
        except (ValueError, UnicodeError) as exc:
            raise RuntimeError("DSH 输出无效 NDJSON") from exc
        if not isinstance(event, dict):
            raise RuntimeError("DSH 输出无效事件")
        kind = event.get("type")
        if kind == "thinking":
            return
        emit("native", event)
        if kind == "session":
            identity = event.get("sessionId")
        elif kind == "text" and isinstance(event.get("text"), str):
            emit("delta", event["text"])
            # DSH emits a complete committed visible block in each text event.
            emit("message.boundary", None)
        elif kind == "final":
            final = event.get("text")

    # Pin the native model and filesystem policy in this invocation's last
    # Cordis layer, without modifying any user or project configuration.
    policy = f"""- id: agent-default-model
  config:
    provider: {json.dumps(rid)}
    model: {json.dumps(model)}
- id: sandbox-policy
  config:
    mode: {mode}
    workspaceRoot: !!js process.cwd()
- id: approval
  config:
    policy: never
- id: permission
  config:
    defaultPreset: {mode}
    presets:
      {mode}:
        sandbox: {mode}
        approval: never
"""
    if route["official"]:
        spec = {"baseURL":route["endpoint"],"apiKeyEnv":route["ref"],
                "models":list(route["models"].values())}
        if route["key"]:
            env["SUPERAGENT_DSH_CALL_KEY"] = route["key"]
            spec["apiKeyEnv"] = "SUPERAGENT_DSH_CALL_KEY"
        policy += f"""- id: llm-pi-ai
  disabled: true
- id: llm-deepseek
  config: {json.dumps(spec,ensure_ascii=False)}
"""
    else:
        # The profile contains only allowlisted public fields of the selected
        # model. Secrets live in the child environment or native credential store.
        allowed = ("id","name","contextWindow","input","reasoningEfforts","maxTokens","compat")
        fields = {k:v for k,v in route["models"][model].items() if k in allowed}
        ref = "SUPERAGENT_DSH_CALL_KEY" if route["key"] else route["ref"]
        profile = dict(api=route["api"],baseURL=route["endpoint"],apiKeyEnv=ref,models=[fields])
        if route["reasoning"]:
            profile["reasoning"] = route["reasoning"]
        policy += f"""- id: llm-deepseek
  disabled: true
- id: llm-pi-ai
  config:
    providers: {json.dumps({rid:profile},ensure_ascii=False)}
"""
        if route["key"]:
            env[ref] = route["key"]
    if not route["key"]:
        policy += f"""- id: credentials
  disabled: false
  config:
    path: {json.dumps(str(route['store'].resolve()))}
    watch: false
"""
    with tempfile.TemporaryDirectory(prefix="superagent-dsh-") as temp:
        patch = Path(temp) / "policy.yml"
        patch.write_text(policy, encoding="utf-8")
        args = [*_command(), "--profile", "headless", "--patch", str(patch), "--json"]
        if session_id:
            args.extend(["--session-id", session_id])
        emit("adapter.permission", {"profile":permission_profile,"scope":mode})
        emit("adapter.route", {"provider":rid,"endpoint":route["endpoint"],"requested_model":model,
                               "mode":"cli","source":route["source"]})
        stream_lines([*args, "-"], cwd, prompt.encode("utf-8"), on_line, cancel, env=env)
    if not isinstance(identity, str) or not isinstance(final, str) or not final:
        raise RuntimeError("DSH 未返回完整的 session/final 事件")
    return final, identity
