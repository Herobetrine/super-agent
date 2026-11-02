"""Claude Code print-mode adapter; prompts stay off the command line."""
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import subprocess
import threading
import time
from urllib.parse import urlsplit

from .process import CREATE_FLAGS, stream_lines
from . import claude_choices as catalog
from .claude_choices import watch_paths
from .native_permissions import scope_for


_gateway_lock = threading.Lock()


def _selected_provider():
    """Read only the authorized Claude provider, using CC Switch's read-only DB."""
    binding = Path.home() / ".local/share/superagent/claude-route.json"
    if not binding.is_file():
        raise RuntimeError("未绑定 CC Switch 供应商，请先选择 Claude 路由")
    approved = json.loads(binding.read_text(encoding="utf-8"))
    if approved.get("app_type") != "claude-desktop":
        raise RuntimeError("当前绑定没有指定 Claude Desktop，请重新确认 Ferryman 路由")
    root = Path.home() / ".cc-switch"
    settings = json.loads((root / "settings.json").read_text(encoding="utf-8-sig"))
    live = json.loads((root / "live-state.json").read_text(encoding="utf-8-sig"))
    if (settings.get("currentProviderClaudeDesktop") != approved["provider_id"]
            or live.get("apps", {}).get("claude-desktop", {}).get("mode") != "direct"):
        raise RuntimeError("CC Switch 中 Claude Desktop 的当前直连供应商已改变")
    db = catalog.db_path()
    conn = sqlite3.connect(db.as_uri() + "?mode=ro", uri=True, timeout=3)
    try:
        rows = conn.execute(
            "SELECT id, settings_config FROM providers WHERE app_type='claude-desktop' "
            "AND id=? AND is_current=1", (approved["provider_id"],)
        ).fetchall()
    finally:
        conn.close()
    if len(rows) != 1 or rows[0][0] != approved["provider_id"]:
        raise RuntimeError("CC Switch 当前供应商与已授权记录不符")
    config = json.loads(rows[0][1])["env"]
    base, model = config["ANTHROPIC_BASE_URL"], approved["model"]
    url = urlsplit(base)
    if (base != approved["base_url"] or url.scheme not in ("http", "https")
            or url.hostname not in ("127.0.0.1", "::1") or url.username or url.password
            or url.query or url.fragment):
        raise RuntimeError("当前 Claude Desktop 端点与已确认的本地网关不符")
    if not config.get("ANTHROPIC_AUTH_TOKEN"):
        raise RuntimeError("CC Switch 已授权供应商缺少认证配置")
    return base, config["ANTHROPIC_AUTH_TOKEN"], model


def _magpie_config():
    if os.name != "nt":
        return None
    root = Path.home() / ".config/magpie"
    applied = root / "applied.json"
    settings = Path.home() / ".claude/settings.json"
    if not applied.is_file() or not settings.is_file():
        return
    if "claude" not in json.loads(applied.read_text(encoding="utf-8-sig")):
        return
    user = json.loads(settings.read_text(encoding="utf-8-sig"))
    url = urlsplit(user.get("env", {}).get("ANTHROPIC_BASE_URL", ""))
    mg = json.loads((root / "settings.json").read_text(encoding="utf-8-sig"))
    if (url.scheme != "http" or url.hostname != "127.0.0.1" or url.username or url.password
            or url.query or url.fragment or url.port != mg.get("port", 3425) or url.path not in ("", "/")):
        return None
    config = user["env"]
    return url.geturl(), config["ANTHROPIC_AUTH_TOKEN"], user.get("model") or config.get("ANTHROPIC_MODEL")


def _local_magpie(cancel):
    """Wake the installed gateway only for a Claude configuration it manages."""
    config = _magpie_config()
    if not config:
        return
    url = urlsplit(config[0])
    base = Path(os.environ.get("LOCALAPPDATA", Path.home() / "AppData/Local")) / "Programs/magpie"
    exe = base / "magpie.exe"
    cli = base / "magpie.com"
    if not exe.is_file() or not cli.is_file():
        raise RuntimeError("Claude Code 当前使用 Magpie，请安装或启动已配置的本机网关")
    env = {**os.environ, "MAGPIE_ADDR": f"127.0.0.1:{url.port}"}

    def healthy():
        p = subprocess.run([str(cli), "healthcheck"], capture_output=True, timeout=8,
                           creationflags=CREATE_FLAGS, env=env)
        return p.returncode == 0

    with _gateway_lock:
        if healthy():
            return
        if cancel.is_set():
            raise RuntimeError("任务已取消")
        with socket.socket() as s:
            s.settimeout(0.5)
            if s.connect_ex(("127.0.0.1", url.port)) == 0:
                raise RuntimeError("本地网关端口已有监听，但 Magpie 健康检查未通过")
        subprocess.Popen([str(exe), "serve"], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                         stderr=subprocess.DEVNULL, creationflags=CREATE_FLAGS, env=env)
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline and not cancel.wait(0.5):
            if healthy():
                return
        raise RuntimeError("Magpie 尚未就绪，请检查本地网关")


def choices():
    rows, default, legacy, notes = catalog.discover(_executable())
    return {"providers": [row.public() for row in rows], "default": default, "legacy_defaults": legacy,
            "modes": [{"id": "cli", "label": "Claude Code CLI", "available": bool(_executable()),
                       "detail": "原生消息流；工具权限按当前会话档位执行"},
                      {"id": "desktop", "label": "Claude Desktop", "available": False,
                       "detail": "桌面 Code 页仅支持交互；未打通自动投递与终稿回传"}],
            "downloads": [{"label": "Claude Code 官方安装", "url": "https://code.claude.com/docs/en/setup"},
                          {"label": "Claude Desktop 官方下载", "url": "https://claude.com/download"}],
            "detail": "。".join(notes) or "按本机配置读取。在选择外部供应商之前，不会向其请求目录或发送任务。"}


def _executable():
    if os.name == "nt":
        exe = shutil.which("claude.exe")
        if exe:
            return exe
        shim = shutil.which("claude.cmd")
        if shim:
            exe = Path(shim).parent / "node_modules/@anthropic-ai/claude-code/bin/claude.exe"
            if exe.is_file():
                return str(exe)
        return None
    return shutil.which("claude")


def probe():
    exe = _executable()
    if not exe:
        return {"ready": False, "detail": "未找到 Claude Code 原生可执行文件"}
    try:
        p = subprocess.run([exe, "--version"], capture_output=True, timeout=8,
                           creationflags=CREATE_FLAGS)
        if override := os.getenv("SUPERAGENT_CLAUDE_BASE_URL"):
            route = f"自定义 {urlsplit(override).hostname}"
        elif os.getenv("SUPERAGENT_CLAUDE_ROUTE", "cc-switch") == "cc-switch":
            base, _, model = _selected_provider()
            route = f"CC Switch {urlsplit(base).hostname} / {model}"
        else:
            route = "Claude Code 用户配置"
        return {"ready": p.returncode == 0, "detail": f"{p.stdout.decode().strip()} · {route}（未请求模型）"}
    except (OSError, KeyError, ValueError, sqlite3.Error, RuntimeError, subprocess.TimeoutExpired) as exc:
        return {"ready": False, "detail": str(exc)}


def _resolve_selection(selection):
    if not isinstance(selection, dict) or selection.get("mode") != "cli":
        raise RuntimeError("Claude Desktop 未打通自动回传，自动任务请选择 CLI")
    rows, _, _, _ = catalog.discover(_executable())
    record = next((r for r in rows if r.id == selection.get("provider")), None)
    if record is None or record.reason:
        raise RuntimeError("所选供应商不可用或其端点已改变")
    if selection.get("model") not in record.models:
        raise RuntimeError("模型未出现在此供应商的当前目录中")
    return catalog.cli_base(record.base), record.token, record.auth_name, selection["model"]


def run(prompt: str, cwd: str, emit, cancel: threading.Event, *, session_id=None, writable=False,
        route=None, selection=None, permission_profile=None):
    scope = scope_for("claude",permission_profile,writable,cwd,emit,cancel)
    unrestricted = scope == "danger-full-access"
    exe = _executable()
    if not exe:
        raise RuntimeError("找不到 Claude Code")
    argv = [exe, "-p", "--output-format", "stream-json", "--input-format", "stream-json",
            "--verbose", "--include-partial-messages"]
    if unrestricted:
        argv += ["--permission-mode", "bypassPermissions"]
    else:
        argv += ["--safe-mode", "--strict-mcp-config", "--disable-slash-commands", "--no-chrome",
                 "--tools", "Read,Glob,Grep", "--permission-mode", "dontAsk"]
    env = os.environ.copy()
    # Explicit per-platform overrides are inherited by the child only.
    # Never mix another server's credentials with a new endpoint.
    # A job's explicit route takes precedence over optional environment overrides.
    auth_name = "ANTHROPIC_AUTH_TOKEN"
    if selection is not None:
        base, token, auth_name, model = _resolve_selection(selection)
        ferryman = False
        if urlsplit(base).hostname in ("127.0.0.1", "::1", "localhost"):
            bound = catalog.binding_file()
            ferryman = bound.is_file() and base == json.loads(bound.read_text(encoding="utf-8")).get("base_url")
    else:
        base = env.get("SUPERAGENT_CLAUDE_BASE_URL") if route is None else None
        token = env.get("SUPERAGENT_CLAUDE_AUTH_TOKEN") if route is None else None
        if bool(base) != bool(token):
            raise RuntimeError("自定义 Claude 网关需同时指定 BASE_URL 和 AUTH_TOKEN")
        model = env.get("SUPERAGENT_CLAUDE_MODEL") if route is None else None
        route = route if route is not None else env.get("SUPERAGENT_CLAUDE_ROUTE", "cc-switch")
        if route not in ("cc-switch", "user"):
            raise RuntimeError("Claude 路由必须为 cc-switch 或 user")
        ferryman = not base and route == "cc-switch"
        if ferryman:
            base, token, model = _selected_provider()
    if ferryman:
        # This Ferryman deployment rejects synthetic system messages between turns.
        caps = env.get("CLAUDE_CODE_MODEL_CAPABILITIES", "").rstrip(";")
        env["CLAUDE_CODE_MODEL_CAPABILITIES"] = (caps + ";" if caps else "") + model + "=-mid_conv_system"
    if auth_name == "sdk-login":
        env = catalog.official_env()
        if not unrestricted:
            argv += ["--setting-sources", ""]
    elif base:
        if not unrestricted:
            argv += ["--setting-sources", ""]
        for key in list(env):
            if (key.startswith(("ANTHROPIC_", "CLAUDE_CODE_OAUTH"))
                    or key in ("CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX",
                               "CLAUDE_CODE_USE_FOUNDRY", "SUPERAGENT_CLAUDE_AUTH_TOKEN")):
                env.pop(key)
        env["ANTHROPIC_BASE_URL"], env[auth_name] = base, token
    else:
        _local_magpie(cancel)
        if not unrestricted:
            argv += ["--setting-sources", "user"]
        native = catalog._read(catalog._native_file()).get("env", {})
        configured_base = env.get("ANTHROPIC_BASE_URL") or native.get("ANTHROPIC_BASE_URL", "")
        url = urlsplit(configured_base)
        if url.hostname in ("127.0.0.1","::1","localhost") and url.path.rstrip("/").endswith("/v1"):
            # The credential still comes from the user's existing settings. Only
            # reconcile the SDK's own /v1 suffix for this exact local origin.
            overrides = {"env":{"ANTHROPIC_BASE_URL":catalog.cli_base(configured_base)}}
            argv += ["--settings", json.dumps(overrides)]
    if model:
        argv += ["--model", model]
    if unrestricted and (base or auth_name == "sdk-login"):
        # Keep normal skills, MCP, hooks and settings, while preventing endpoint
        # or credential settings from overriding the specifically selected route.
        env["CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST"] = "1"
    if session_id:
        argv += ["--resume", session_id]
    terminal = None

    def receive(line):
        nonlocal terminal
        event = json.loads(line)
        emit("native", event)
        if selection is not None and auth_name != "sdk-login" and event.get("type") == "system" and event.get("subtype") == "init":
            if str(event.get("model", "")).removesuffix("[1M]") != model.removesuffix("[1M]"):
                raise RuntimeError("Claude Code 报告的启动模型与所选模型不符")
        if (event.get("type") == "system" and event.get("subtype") == "api_retry"
                and event.get("error") == "authentication_failed"):
            raise RuntimeError("Claude Code 供应商拒绝认证；请更新当前路由的凭据")
        if event.get("type") == "stream_event":
            frame = event.get("event", {})
            delta = frame.get("delta", {})
            if delta.get("type") == "text_delta":
                emit("delta", delta["text"])
            if frame.get("type") == "content_block_stop":
                emit("message.boundary", None)
        if event.get("type") == "result":
            terminal = event
            if event.get("is_error") or event.get("subtype") != "success":
                raise RuntimeError("Claude Code " + str(event.get("result") or event.get("subtype")))

    event = {"type": "user", "message": {"role": "user", "content": prompt}}
    emit("adapter.permission", {"profile":permission_profile,"scope":scope})
    stream_lines(argv, cwd, (json.dumps(event, ensure_ascii=False) + "\n").encode("utf-8"),
                 receive, cancel, env=env)
    if not terminal or not isinstance(terminal.get("result"), str) or not terminal.get("session_id"):
        raise RuntimeError("Claude Code 未返回完整的 result 事件")
    if session_id and terminal["session_id"] != session_id:
        raise RuntimeError("Claude Code 返回了与请求不符的会话 ID")
    return terminal["result"], terminal["session_id"]
