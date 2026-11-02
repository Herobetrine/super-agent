"""Drive Google's Antigravity CLI using its native NDJSON event stream."""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import threading

from .process import CREATE_FLAGS, stream_lines
from . import antigravity_choices as config
from .claude_choices import watch_paths as _cc_watch_paths
from .native_permissions import scope_for


def probe():
    exe = shutil.which("agy")
    if not exe:
        return {"ready": False, "detail": "未找到 Google Antigravity CLI (agy)"}
    try:
        p = subprocess.run([exe, "--version"], capture_output=True, timeout=8,
                           creationflags=CREATE_FLAGS)
        return {"ready": p.returncode == 0, "detail": f"agy {p.stdout.decode().strip()}（入口检测，未请求模型）"}
    except (OSError, ValueError, subprocess.TimeoutExpired) as exc:
        return {"ready": False, "detail": str(exc)}


def watch_paths():
    exe = shutil.which("agy")
    return [*_cc_watch_paths(), config.native_path(), *([Path(exe)] if exe else [])]


def _provider_env(provider, row):
    env = os.environ.copy()
    overrides = {"modelProvider": ""}
    for key in ("GEMINI_API_KEY", "GOOGLE_GEMINI_BASE_URL", "AGY_ADC_AUTH"):
        env.pop(key, None)
    if row:
        public, base, token = row
        if not public["available"] or provider != public["id"]:
            raise RuntimeError("所选 Antigravity 配置不完整")
        overrides.update(modelProvider="gemini", model=public["defaultModel"])
        env["GEMINI_API_KEY"] = token
        if base:
            env["GOOGLE_GEMINI_BASE_URL"] = base
    elif provider == "adc" and os.getenv("AGY_ADC_AUTH","").lower() == "true":
        env["AGY_ADC_AUTH"] = "true"
    elif provider != "google":
        raise RuntimeError("Antigravity 供应商不可用")
    return env, overrides


def _reader_env(provider="google", row=None):
    # Each restricted route has separate permissions and conversation storage.
    home = Path.home() / ".local/share/superagent" / ("agy-reader" if provider == "google" else "agy-reader-" + provider)
    settings = home / ".gemini/antigravity-cli/settings.json"
    settings.parent.mkdir(parents=True, exist_ok=True)
    denied = ["write_file(*)", "command(*)", "mcp(*)", "execute_url(*)", "read_url(*)"]
    env, policy = _provider_env(provider,row)
    policy["permissions"] = {"deny":denied}
    env.update(HOME=str(home),USERPROFILE=str(home))
    try:
        existing = json.loads(settings.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        existing = None
    if existing == policy:
        return env
    with tempfile.NamedTemporaryFile("w", dir=settings.parent, delete=False, encoding="utf-8") as f:
        json.dump(policy, f)
        name = f.name
    try:
        os.replace(name, settings)
    finally:
        Path(name).unlink(missing_ok=True)
    return env


def _models(exe, provider):
    proc = subprocess.run([exe, "models"], capture_output=True, timeout=15, env=_reader_env(provider),
                          creationflags=CREATE_FLAGS)
    if proc.returncode:
        raise RuntimeError("agy models 未能返回当前供应商的模型目录")
    models = []
    for line in proc.stdout.decode("utf-8").splitlines():
        key, sep, label = line.partition("\t")
        if sep and re.fullmatch(r"[A-Za-z0-9._/-]{1,150}", key) and label.strip():
            models.append({"id": key, "label": label.strip()})
    return models


def choices():
    exe = shutil.which("agy")
    providers = []
    native = [("google", "Google · Antigravity 登录账号")]
    if os.getenv("AGY_ADC_AUTH","").lower() == "true":
        native.append(("adc","Google Cloud · Application Default Credentials"))
    for provider, label in native:
        models, detail = [], "来自本机 agy models"
        try:
            if exe:
                models = _models(exe, provider)
                if not models:
                    detail = "本机没有返回可识别的模型"
            else:
                detail = "未安装 agy"
        except (ValueError, TypeError, AttributeError, OSError, RuntimeError, subprocess.TimeoutExpired):
            detail = "无法读取 agy 当前模型目录"
        if exe:
            models = [{"id":"@native", "label":"跟随 agy 原生默认模型"}, *models]
        providers.append({"id": provider, "label": label, "models": models,
                          "available": bool(models), "detail": detail})
    configured_routes = config.routes()
    providers.extend(row[0] for row in configured_routes)
    default = next((p for p in providers if p["id"] == "adc"), providers[0])
    if config.native_config().get("modelProvider") == "gemini" and configured_routes:
        default = configured_routes[0][0]
    catalog = default["models"]
    configured_model = os.getenv("SUPERAGENT_AGY_MODEL") or config.native_config().get("model")
    default_model = next((m["id"] for m in catalog if configured_model in (m["id"],m["label"])),
                         default.get("defaultModel") or ("@native" if catalog else ""))
    return {"providers": providers, "default": {"mode": "cli", "provider": default["id"], "model": default_model},
            "modes": [{"id": "cli", "label": "Antigravity CLI", "available": bool(exe), "detail": "原生双向消息流；工具权限按当前会话档位执行"},
                      {"id": "desktop", "label": "Antigravity App", "available": False,
                       "detail": "本机桌面 App 尚未打通可自动投递并回传终稿的桥接"}],
            "downloads": [{"label": "Antigravity CLI 官方安装", "url": "https://antigravity.google/docs/cli/install"},
                          {"label": "Antigravity 桌面官方下载", "url": "https://antigravity.google/download"}],
            "detail": "经登录账户或明确配置的 Google API 获取目录；选择的模型原样交给 agy。"}


def run(prompt: str, cwd: str, emit, cancel: threading.Event, *, session_id=None, writable=False,
        selection=None, permission_profile=None):
    scope = scope_for("antigravity",permission_profile,writable,cwd,emit,cancel)
    exe = shutil.which("agy")
    if not exe:
        raise RuntimeError("找不到 agy")
    argv = [exe, "--input-format", "stream-json", "--output-format", "stream-json"]
    if session_id:
        argv += ["--conversation", session_id]
    provider = "google"
    row = None
    model = os.getenv("SUPERAGENT_AGY_MODEL")
    if selection is not None:
        if not isinstance(selection, dict) or selection.get("mode") != "cli":
            raise RuntimeError("Antigravity 桌面尚未打通自动调用，自动任务请选择 CLI")
        provider, model = selection.get("provider"), selection.get("model")
        if provider in ("google", "adc"):
            models = [{"id":"@native"}] if model == "@native" else _models(exe, provider)
        else:
            row = next((r for r in config.routes() if r[0]["id"] == provider and r[0]["available"]), None)
            if row is None:
                raise RuntimeError("Antigravity 供应商已变更或缺少所需配置")
            models = row[0]["models"]
        if not isinstance(model, str) or not any(m["id"] == model for m in models):
            raise RuntimeError("模型未出现在所选 Antigravity 供应商的当前目录中")
    if model and model != "@native":
        argv += ["--model", model]
    if scope == "danger-full-access":
        env,overrides = _provider_env(provider,row)
        argv += ["--dangerously-skip-permissions", "--settings", json.dumps(overrides)]
    else:
        env = _reader_env(provider,row)
        argv += ["--disable-slash-commands"]
    terminal = None

    def receive(line):
        nonlocal terminal
        event = json.loads(line)
        emit("native", event)
        if event.get("event") == "step_update":
            step = event.get("step_update", {})
            if step.get("step_type") == "agent_response":
                if step.get("text_delta"):
                    emit("delta", step["text_delta"])
                if step.get("state") == "DONE":
                    emit("message.boundary", None)
        if event.get("event") == "result":
            terminal = event["result"]
            if terminal.get("status") != "SUCCESS":
                raise RuntimeError("Antigravity " + str(terminal.get("error") or terminal.get("status")))

    body = json.dumps({"event": "user", "message": {"content": prompt}}, ensure_ascii=False)
    emit("adapter.permission", {"profile":permission_profile,"scope":scope})
    stream_lines(argv, cwd, (body + "\n").encode("utf-8"), receive, cancel,
                 env=env)
    if not terminal or not isinstance(terminal.get("response"), str) or not terminal.get("conversation_id"):
        raise RuntimeError("Antigravity 未返回完整的 SUCCESS 事件")
    if session_id and terminal["conversation_id"] != session_id:
        raise RuntimeError("Antigravity 返回了与请求不符的会话 ID")
    return terminal["response"], terminal["conversation_id"]
