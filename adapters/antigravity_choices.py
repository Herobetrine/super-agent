"""Read saved Gemini-compatible routes without transmitting their credentials."""
import hashlib
import json
import os
from pathlib import Path
import sqlite3
from urllib.parse import urlsplit

from .claude_choices import db_path


def native_path():
    return Path.home() / ".gemini/antigravity-cli/settings.json"


def native_config():
    try:
        value = json.loads(native_path().read_text(encoding="utf-8-sig"))
        return value if isinstance(value, dict) else {}
    except (OSError, ValueError):
        return {}


def _route(record_id, label, base, token, model):
    # A provider cannot redirect an existing selection by changing the stored endpoint.
    key = "agy-" + hashlib.sha256(json.dumps([record_id,base]).encode()).hexdigest()[:24]
    models = [{"id": model, "label": model}] if isinstance(model,str) and 0 < len(model) < 200 else []
    try:
        url = urlsplit(base) if isinstance(base, str) else urlsplit("")
    except ValueError:
        url = urlsplit("")
    supported = (not base or (url.hostname and not url.username and not url.password
                 and not url.query and not url.fragment and (url.scheme == "https" or
                 (url.scheme == "http" and url.hostname in ("127.0.0.1","::1","localhost")))))
    enabled = bool(token and supported and models)
    public = {"id": key, "label": label + (f" · {url.hostname}" if url.hostname else ""),
              "available": enabled, "models": models, "defaultModel": model if models else "",
              "detail": ("本机保存的 Gemini 协议端点；仅在选定后使用" if enabled else
                         "缺少配对的密钥、有效端点或已配置模型")}
    return public, base, token


def routes():
    records = []
    config = native_config()
    if config.get("modelProvider") == "gemini":
        records.append(_route("native", "Gemini API · 当前 agy 配置", os.getenv("GOOGLE_GEMINI_BASE_URL",""),
                              os.getenv("GEMINI_API_KEY",""), config.get("model") or os.getenv("GEMINI_MODEL")))
    try:
        path = db_path()
        if not path.is_file():
            return records
        conn = sqlite3.connect(path.as_uri()+"?mode=ro",uri=True,timeout=2)
        try:
            rows = conn.execute("SELECT id,name,settings_config FROM providers WHERE app_type='agy' ORDER BY name").fetchall()
        finally:
            conn.close()
        for ident, label, raw in rows:
            try:
                settings = json.loads(raw)
                env = settings.get("env",{})
                model = (settings.get("config",{}).get("model",{}) or {}).get("name") or env.get("GEMINI_MODEL")
                records.append(_route(ident,label,env.get("GOOGLE_GEMINI_BASE_URL",""),env.get("GEMINI_API_KEY"),model))
            except (ValueError, AttributeError, TypeError):
                records.append(_route(ident, label, "", None, None))
    except (OSError, TypeError, ValueError, sqlite3.Error):
        pass
    return records
