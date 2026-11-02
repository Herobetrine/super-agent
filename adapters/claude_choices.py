"""Resolve fresh Claude provider records without probing external vendors."""
from dataclasses import dataclass, field
import hashlib
import http.client
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
from urllib.parse import urlsplit
from .process import CREATE_FLAGS


def _read(path):
    if not path.is_file():
        return {}
    data = json.loads(path.read_text(encoding="utf-8-sig"))
    if not isinstance(data, dict):
        raise ValueError("配置文件须为对象")
    return data


def _device_dir():
    return Path.home() / ".cc-switch"


def _override_file():
    return Path(os.environ.get("APPDATA", Path.home() / "AppData/Roaming")) / "com.ccswitch.desktop/app_paths.json"


def db_path():
    value = _read(_override_file()).get("app_config_dir_override")
    folder = Path(value).expanduser() if value else _device_dir()
    if not folder.is_absolute():
        raise ValueError("CC Switch 自定义数据库目录必须是绝对路径")
    return folder / "cc-switch.db"


def _native_file():
    return Path(os.environ.get("CLAUDE_CONFIG_DIR") or Path.home() / ".claude") / "settings.json"


def binding_file():
    return Path.home() / ".local/share/superagent/claude-route.json"


def watch_paths():
    paths = [_override_file(), _device_dir() / "settings.json", _device_dir() / "live-state.json",
             _native_file(), _native_file().with_name(".credentials.json"), binding_file()]
    try:
        db = db_path()
    except (ValueError, TypeError, OSError):
        return paths
    return [*paths, db, Path(str(db) + "-wal")]


def cli_base(base):
    """The Claude client appends /v1/messages to the SDK base URL."""
    return base.rstrip("/").removesuffix("/v1")


def _models_at(base, auth_name, token):
    url = urlsplit(base)
    if url.hostname not in ("127.0.0.1", "::1", "localhost") or url.scheme not in ("http","https"):
        raise ValueError("只允许直接读取已配置的回环模型目录")
    kind = http.client.HTTPSConnection if url.scheme == "https" else http.client.HTTPConnection
    conn = kind(url.hostname, url.port, timeout=2)
    headers = ({"x-api-key": token} if auth_name == "ANTHROPIC_API_KEY"
               else {"Authorization": "Bearer " + token})
    try:
        conn.request("GET", urlsplit(cli_base(base)).path + "/v1/models", headers=headers)
        r = conn.getresponse()
        if r.status != 200:
            raise RuntimeError(f"模型目录 HTTP {r.status}")
        b = r.read(512 * 1024 + 1)
        if len(b) > 512 * 1024:
            raise ValueError("模型目录过大")
        return sorted({m["id"] for m in json.loads(b)["data"] if isinstance(m,dict)
                       and isinstance(m.get("id"),str) and 0 < len(m["id"]) < 200})
    finally:
        conn.close()


@dataclass
class Record:
    app: str
    row_id: str
    name: str
    base: str = ""
    auth_name: str = ""
    token: str = field(default="", repr=False)
    models: list = field(default_factory=list)
    preferred: str = ""
    reason: str = ""

    @property
    def id(self):
        # A changed origin creates a new selection. Rotated credentials and renamed rows keep theirs.
        attrs = json.dumps([self.app, self.row_id, self.base, self.auth_name])
        return "ccs-" + hashlib.sha256(attrs.encode()).hexdigest()[:24]

    def public(self):
        app = {"claude":"Claude Code", "claude-desktop":"Claude Desktop", "native":"当前 CLI"}[self.app]
        host = urlsplit(self.base).hostname if self.base else ""
        return {"id": self.id, "label": f"{self.name} · {app}" + (f" · {host}" if host else ""),
                "available": not self.reason, "detail": self.reason or "端点与凭据来自同一配置记录",
                "models": [{"id": m, "label": m} for m in self.models], "defaultModel": self.preferred}


class Unavailable(RuntimeError):
    """A user-facing, credential-free reason a configured route cannot run."""


def _record(app, row_id, name, settings, meta, cache):
    rec = Record(app, row_id, name)
    try:
        env = settings.get("env", {})
        for key in ("ANTHROPIC_MODEL","ANTHROPIC_DEFAULT_OPUS_MODEL","ANTHROPIC_DEFAULT_SONNET_MODEL",
                    "ANTHROPIC_DEFAULT_HAIKU_MODEL","ANTHROPIC_DEFAULT_FABLE_MODEL"):
            value = env.get(key)
            if isinstance(value, str) and value:
                rec.models.append(value)
        rec.preferred = settings.get("model") or env.get("ANTHROPIC_MODEL") or ""
        if rec.preferred:
            rec.models.append(rec.preferred)
        for dest in (meta.get("claudeDesktopModelRoutes") or {}).values():
            if isinstance(dest, dict) and isinstance(dest.get("model"), str):
                rec.models.append(dest["model"])
        rec.models = list(dict.fromkeys(m for m in rec.models if isinstance(m,str) and 0 < len(m) < 200))
        base = env.get("ANTHROPIC_BASE_URL", "")
        url = urlsplit(base)
        if not url.hostname or url.username or url.password or url.query or url.fragment:
            raise Unavailable("本地记录未提供可绑定的独立端点")
        local = url.hostname in ("127.0.0.1","::1","localhost")
        if url.scheme != "https" and not (local and url.scheme == "http"):
            raise Unavailable("需要 HTTPS 或本机回环端点")
        rec.base = base.rstrip("/")
        rec.auth_name = "ANTHROPIC_AUTH_TOKEN" if env.get("ANTHROPIC_AUTH_TOKEN") else "ANTHROPIC_API_KEY"
        rec.token = env.get(rec.auth_name)
        if not isinstance(rec.token,str) or not rec.token:
            raise Unavailable("未绑定此端点的配对凭据")
        if meta.get("apiFormat") not in ("anthropic", None, ""):
            raise Unavailable("此记录的协议需要独立适配器")
        if local:
            key = (rec.base, rec.auth_name, rec.token)
            if key not in cache:
                try:
                    cache[key] = _models_at(*key)
                except (OSError, KeyError, TypeError, ValueError, RuntimeError, http.client.HTTPException):
                    cache[key] = None
            if cache[key] is not None:
                rec.models = cache[key]
        if not rec.models:
            raise Unavailable("本机配置或本地目录未提供可选模型")
    except Unavailable as exc:
        rec.reason = str(exc)
    except (ValueError, AttributeError, TypeError, KeyError):
        rec.reason = "无法解析此记录"
    return rec


def official_env():
    env = os.environ.copy()
    # Credentials configured alongside a different API origin must stay there.
    if env.get("ANTHROPIC_BASE_URL"):
        env.pop("ANTHROPIC_API_KEY", None)
        env.pop("ANTHROPIC_AUTH_TOKEN", None)
    for key in ("ANTHROPIC_BASE_URL","ANTHROPIC_CUSTOM_HEADERS","CLAUDE_CODE_USE_BEDROCK",
                "CLAUDE_CODE_USE_VERTEX","CLAUDE_CODE_USE_FOUNDRY"):
        env.pop(key,None)
    native = _read(_native_file()).get("env", {})
    if isinstance(native,dict) and not native.get("ANTHROPIC_BASE_URL"):
        for key in ("ANTHROPIC_API_KEY","ANTHROPIC_AUTH_TOKEN"):
            if native.get(key):
                env[key] = native[key]
    return env


def _official(exe):
    rec = Record("native", "first-party", "官方账号", auth_name="sdk-login")
    rec.reason = "CLI 未检测到可用的官方登录或官方 API 认证"
    if not exe:
        return rec
    try:
        common = [exe, "--safe-mode", "--setting-sources", ""]
        with tempfile.TemporaryDirectory(prefix="superagent-sdk-info-") as folder:
            kwargs = dict(cwd=folder, env=official_env(), creationflags=CREATE_FLAGS, capture_output=True, timeout=8)
            auth = subprocess.run([*common,"auth","status"], **kwargs)
            status = json.loads(auth.stdout)
            if auth.returncode or not status.get("loggedIn") or status.get("apiProvider") != "firstParty":
                return rec
            argv = [*common, "-p", "--input-format","stream-json","--output-format","stream-json",
                    "--verbose","--strict-mcp-config","--tools","","--no-session-persistence"]
            msg = {"type":"control_request","request_id":"catalog","request":{"subtype":"initialize"}}
            proc = subprocess.run(argv,input=(json.dumps(msg)+"\n").encode(),**kwargs)
            for line in proc.stdout.splitlines():
                outer = json.loads(line).get("response",{})
                if outer.get("request_id") == "catalog" and outer.get("subtype") == "success":
                    models = outer.get("response",{}).get("models",[])
                    rec.models = [m["value"] for m in models if isinstance(m.get("value"),str) and m["value"]]
                    break
        rec.reason = "" if rec.models else "已登录，但 SDK 初始化没有返回模型目录"
    except (ValueError, TypeError, KeyError, AttributeError, OSError, subprocess.TimeoutExpired):
        rec.reason = "本次未能验证官方认证及 SDK 模型目录"
    return rec


def discover(exe=None):
    rows, notes, cache = [], [], {}
    device, live, bound = {}, {}, {}
    for target, path in [(device, _device_dir() / "settings.json"), (live, _device_dir() / "live-state.json"),
                         (bound, binding_file())]:
        try:
            target.update(_read(path))
        except (ValueError, OSError):
            notes.append(f"无法读取 {path.name}")
    official = _official(exe)
    try:
        db = db_path()
        if db.is_file():
            conn = sqlite3.connect(db.as_uri()+"?mode=ro", uri=True, timeout=2)
            try:
                columns = {c[1] for c in conn.execute("PRAGMA table_info(providers)")}
                category = "category" if "category" in columns else "NULL"
                records = conn.execute(f"SELECT app_type,id,name,settings_config,meta,{category} FROM providers "
                                       "WHERE app_type IN ('claude','claude-desktop') ORDER BY app_type,name").fetchall()
            finally:
                conn.close()
            for app, key, name, raw, meta, category in records:
                try:
                    rec = _record(app,key,name,json.loads(raw),json.loads(meta) if meta else {},cache)
                except (TypeError, ValueError):
                    rec = Record(app,key,name,reason="这条配置格式有误")
                if category == "official" and not rec.base:
                    rec.auth_name, rec.models, rec.reason = official.auth_name, official.models, official.reason
                rows.append(rec)
    except (TypeError, ValueError, OSError, sqlite3.Error):
        notes.append("CC Switch 数据库暂不可读")
    native = None
    try:
        source = _native_file()
        config = _read(source)
        if os.environ.get("ANTHROPIC_BASE_URL"):
            # Never pair an environment endpoint with a credential from a file.
            config = {"env": {k:v for k,v in os.environ.items() if k.startswith("ANTHROPIC_")}}
        if config:
            native = _record("native",str(source),"本机配置",config,{},cache)
            matched = next((r for r in rows if r.app == "claude" and not r.reason and
                            (r.base,r.auth_name,r.token) == (native.base,native.auth_name,native.token)), None)
            if matched:
                if native.preferred in matched.models:
                    matched.preferred = native.preferred
                native = matched
            else:
                rows.append(native)
    except (TypeError, ValueError, OSError):
        notes.append("Claude Code 当前配置暂不可读")
    active = next((r for r in rows if r.app == "claude-desktop" and
                   r.row_id == device.get("currentProviderClaudeDesktop")), None)
    apps = live.get("apps", {})
    state = apps.get("claude-desktop", {}) if isinstance(apps,dict) else {}
    if not isinstance(state,dict):
        state = {}
    if state.get("mode") not in ("direct",None):
        active = None
        notes.append("Desktop 路由由代理或聚合配置管理，请明确选择供应商")
    bound_match = active and (active.app,active.row_id,active.base) == (
        bound.get("app_type"),bound.get("provider_id"),str(bound.get("base_url","")).rstrip("/"))
    if bound_match and not active.preferred and bound.get("model") in active.models:
        active.preferred = bound["model"]
    def selection(row):
        model = row.preferred if row.preferred in row.models else row.models[0]
        return {"provider":row.id,"model":model,"mode":"cli"}
    legacy = {}
    if bound_match and not active.reason and bound.get("model") in active.models:
        legacy["cc-switch"] = {**selection(active), "model":bound["model"]}
    if native and not native.reason and native.preferred in native.models:
        legacy["user"] = selection(native)
    if not native or (not native.base and native.reason):
        native = next((r for r in rows if r.app == "claude" and r.auth_name == "sdk-login"), None)
        if not native:
            native = official
            rows.append(official)
    # With no Desktop selection, a native CLI installation is sufficient. A broken
    # explicit Desktop selection is surfaced as unavailable and never substituted.
    if not device.get("currentProviderClaudeDesktop") and state.get("mode") in ("direct", None):
        active = native
    if active is None and (device.get("currentProviderClaudeDesktop") or state.get("mode") not in ("direct",None)):
        active = Record("claude-desktop", "unresolved:"+str(device.get("currentProviderClaudeDesktop","")),
                        "当前选择不可解析", reason="无法匹配设备已选供应商及其直接路由")
        rows.append(active)
    default = (selection(active) if active and not active.reason else
               {"provider":active.id if active else "", "model":"", "mode":"cli"})
    return rows, default, legacy, notes
