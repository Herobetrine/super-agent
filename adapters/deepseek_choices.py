"""Discover local DSH routes; only public model metadata leaves choices()."""
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import subprocess
from urllib.parse import urlsplit

from .process import CREATE_FLAGS

DESKTOP_LIMIT = "尚未验证能自动投递、回传并落实工作区权限的 DSH 桌面端入口；桌面自动执行不可用。"
_READ_ERRORS = (OSError, ValueError, sqlite3.Error, RuntimeError, TypeError, AttributeError, KeyError)


def platform_dir():
    return Path.home() / ".local/share/superagent"


def _home():
    return Path(os.environ.get("DSH_HOME",Path.home()/".dsh")).expanduser()


def _cc_locator():
    return Path(os.environ.get("APPDATA",Path.home()/"AppData/Roaming")) / "com.ccswitch.desktop/app_paths.json"


def _cc_db():
    locator = _cc_locator()
    data = json.loads(locator.read_text(encoding="utf-8-sig")) if locator.is_file() else {}
    override = data.get("app_config_dir_override")
    if override and not isinstance(override,str):
        raise ValueError("CC Switch 数据目录配置无效")
    base = Path(override.strip()).expanduser() if override and override.strip() else Path.home()/".cc-switch"
    if not base.is_absolute():
        raise ValueError("CC Switch 数据目录需要绝对路径")
    return base / "cc-switch.db"


def _yaml(path, default):
    if not path.is_file():
        return default
    try:
        import yaml
    except ImportError:
        raise RuntimeError("读取 DSH 的 YAML 配置需要 PyYAML") from None
    class Loader(yaml.SafeLoader):
        pass
    Loader.add_constructor("tag:yaml.org,2002:js", lambda *_: None)
    try:
        data = yaml.load(path.read_text(encoding="utf-8-sig"), Loader=Loader)
    except yaml.YAMLError:
        raise RuntimeError(f"无法安全解析 {path.name}") from None
    if data is not None and not isinstance(data,type(default)):
        raise ValueError(f"{path.name} 配置类型不符")
    return data or default


def _plugins(rows):
    for row in rows:
        if isinstance(row,dict):
            if isinstance(row.get("insert"),list):
                yield from _plugins(row["insert"])
            if row.get("id") and not row.get("disabled"):
                yield row


def _runtime():
    return platform_dir() / "dsh-runtime"


def _base_yaml():
    return _runtime() / "node_modules/@deepseek-ai/dsh-base/cordis.patch.yml"


def _runtime_catalog(config, endpoint_env=None):
    # The shipped plugins own their public API, credential reference, endpoint
    # and built-in model catalog. Read them without starting an agent or network.
    script = """import {supportedProtocols} from '@deepseek-ai/dsh-llm-pi-ai';
import {resolveAdapterOptions} from '@deepseek-ai/dsh-llm-deepseek-api-key';
import {readFileSync} from 'node:fs';
const {cfg,env}=JSON.parse(readFileSync(0,'utf8'));
const a=resolveAdapterOptions(cfg,{get:k=>Object.hasOwn(env,k)?{value:env[k]}:undefined});
console.log(JSON.stringify({protocols:supportedProtocols(),url:a.baseURL,ref:a.apiKeyEnv,
  models:a.models.map(({id,name,inputModalities,contextWindow})=>({id,name,inputModalities,contextWindow}))}));"""
    p = subprocess.run([shutil.which("node") or "node","--input-type=module","-e",script],
                       cwd=_runtime(),input=json.dumps({"cfg":config,"env":endpoint_env or {}}).encode(),capture_output=True,
                       timeout=8,creationflags=CREATE_FLAGS)
    if p.returncode:
        raise RuntimeError("无法读取已安装的 DSH 模型目录与协议列表")
    return json.loads(p.stdout)


def _profile_paths():
    root = _home()
    return [root/"profiles/desktop/cordis.patch.yml",root/"profiles/headless/cordis.patch.yml",
            root/"cordis.patch.yml"]


def watch_paths():
    home = _home()
    pkg = _runtime() / "node_modules/@deepseek-ai"
    paths = [platform_dir()/"deepseek-default.json",_cc_locator(),_base_yaml(),
             home/".credentials.yaml",home/".env",home/"settings.yaml",*_profile_paths(),
             pkg/"dsh-llm-deepseek/lib/index.js",pkg/"dsh-llm-deepseek-api-key/lib/index.js",
             pkg/"dsh-llm-pi-ai/lib/index.js",pkg/"dsh/package.json",pkg/"dsh/lib/bin.js"]
    try:
        db = _cc_db()
        paths.extend([db,Path(str(db)+"-wal")])
    except _READ_ERRORS:
        pass
    return list(dict.fromkeys(paths))


def _cc_rows(notices):
    try:
        db = _cc_db()
        if not db.is_file():
            return []
        conn = sqlite3.connect(db.as_uri()+"?mode=ro", uri=True, timeout=3)
        try:
            rows = conn.execute("SELECT id,name,settings_config,is_current FROM providers WHERE app_type='dsh' ORDER BY id").fetchall()
        finally:
            conn.close()
    except _READ_ERRORS:
        notices.append("CC Switch 数据库未能读取")
        return []
    valid = []
    for rid,name,raw,current in rows:
        try:
            cfg = json.loads(raw)
            opts = cfg.get("options", {})
            key = opts.get("apiKey")
            if ((key is not None and (not isinstance(key,str) or not key.strip()))
                    or ("baseURL" in opts and opts["baseURL"] != cfg.get("baseURL")) or opts.get("headers")):
                raise ValueError
            valid.append((rid,name,cfg,key,current))
        except _READ_ERRORS:
            notices.append(f"CC Switch 条目 {rid} 配置不完整")
    return valid


def _models(items):
    if isinstance(items,list):
        items = {m["id"]:m for m in items if isinstance(m,dict) and isinstance(m.get("id"),str)}
    if not isinstance(items,dict):
        return {}
    return {m:v for m,v in items.items() if isinstance(m,str) and m and isinstance(v,dict) and v.get("id")==m}


def _valid_url(url):
    if not isinstance(url,str):
        return False
    u = urlsplit(url)
    return bool(u.hostname and not u.username and not u.password and not u.query and not u.fragment
                and (u.scheme=="https" or u.scheme=="http" and u.hostname in ("localhost","127.0.0.1","::1")))


def _binding(cfg):
    return tuple(cfg.get(k) for k in ("api","baseURL","apiKeyEnv"))


def _env_values(path):
    env = {}
    if path.is_file():
        for line in path.read_text(encoding="utf-8-sig").splitlines():
            match = re.fullmatch(r"""\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^#\s]+))\s*(?:#.*)?""", line)
            if match:
                env[match[1]] = next(v for v in match.groups()[1:] if v is not None)
    env.update(os.environ)
    return env


def discover():
    notices = []
    def read(path,default):
        try:
            return _yaml(path,default)
        except _READ_ERRORS:
            notices.append(f"未能读取 {path.name}")
            return default
    home = _home()
    store = home / ".credentials.yaml"
    refs = read(store,{}).get("refs",{})
    refs = refs if isinstance(refs,dict) else {}
    try:
        env = _env_values(home/".env")
    except _READ_ERRORS:
        env = os.environ
        notices.append("无法读取 DSH .env")
    native = {}
    defaults = {}
    builtin = None
    official_cfg = {}
    for path in [_base_yaml(),*_profile_paths()]:
        for plug in _plugins(read(path,[])):
            cfg = plug.get("config",{})
            if not isinstance(cfg,dict):
                continue
            if plug["id"] == "agent-default-model":
                defaults.update({k:v for k,v in cfg.items() if k in ("provider","model") and isinstance(v,str)})
                if path == _base_yaml():
                    builtin = defaults.get("provider")
            if plug["id"] == "llm-pi-ai" and isinstance(cfg.get("providers"),dict):
                native.update(cfg["providers"])
            if plug["id"] == "llm-deepseek":
                official_cfg.update(cfg)
    settings = read(home/"settings.yaml",{})
    overrides = settings.get("llm-pi-ai")
    if isinstance(overrides,dict) and isinstance(overrides.get("providers"),dict):
        native.update(overrides["providers"])
    if isinstance(settings.get("agent-default-model"),dict):
        defaults.update({k:v for k,v in settings["agent-default-model"].items()
                         if k in ("provider","model") and isinstance(v,str)})
    if isinstance(settings.get("llm-deepseek"),dict):
        official_cfg.update(settings["llm-deepseek"])
    official_cfg = {k:v for k,v in official_cfg.items() if k in ("models","baseURL","apiKeyEnv")}
    try:
        runtime = _runtime_catalog({})
    except (*_READ_ERRORS,subprocess.TimeoutExpired):
        return {},defaults,notices+["无法读取隔离 Harness 的模型或协议目录"]
    protocols = runtime["protocols"]
    routes = {}
    if builtin:
        try:
            # Respect native endpoint overrides without handing credentials to
            # the catalog reader or silently falling back to its public default.
            overrides = {k:v for k,v in env.items() if k.endswith(("_BASE_URL","_BASEURL"))}
            spec = _runtime_catalog(official_cfg,overrides) if official_cfg or overrides else runtime
            url,ref = spec["url"],spec["ref"]
            if not _valid_url(url) or (urlsplit(url).netloc != urlsplit(runtime["url"]).netloc
                    and (not official_cfg.get("apiKeyEnv") or ref == runtime["ref"])):
                raise ValueError
            if ref in refs or env.get(ref):
                routes[builtin] = dict(label="DeepSeek 原生 API",endpoint=url,ref=ref,key=env.get(ref),
                    store=store,models=_models(spec["models"]),source="已安装 DSH 插件",official=True)
        except (*_READ_ERRORS,subprocess.TimeoutExpired):
            notices.append("DeepSeek 原生供应商的绑定或模型设置无效")
    cc = _cc_rows(notices)
    ccids = {r[0] for r in cc}
    candidates = [(rid,label,cfg,key,"CC Switch / DSH") for rid,label,cfg,key,_ in cc]
    for rid,cfg in native.items():
        if rid not in ccids and isinstance(cfg,dict):
            candidates.append((rid,cfg.get("displayName") or rid,cfg,None,"DSH 本机档案"))
    for rid,label,cfg,key,source in candidates:
        try:
            if not isinstance(cfg,dict) or not isinstance(rid,str) or not rid or rid==builtin:
                raise ValueError
            api,url,ref = _binding(cfg)
            if (api not in protocols or not _valid_url(url) or cfg.get("headers") or not isinstance(ref,str)
                    or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*",ref)):
                raise ValueError
            if not key:
                key = env.get(ref)
                if not key and ref not in refs:
                    raise ValueError
            peer = native.get(rid)
            if peer is not None and (not isinstance(peer,dict) or _binding(cfg) != _binding(peer)):
                raise ValueError
            models = _models(cfg.get("models"))
            if peer:
                for mid,m in _models(peer.get("models")).items():
                    models.setdefault(mid,m)
            if not models:
                raise ValueError
            routes[rid] = dict(label=label or rid,api=api,endpoint=url,ref=ref,key=key,store=store,
                        models=models,source=source,reasoning=cfg.get("reasoning"),official=False)
        except _READ_ERRORS:
            notices.append(f"供应商 {rid} 的协议、凭据绑定或模型配置不完整")
    return routes,defaults,notices


def _selection(native):
    path = platform_dir()/"deepseek-default.json"
    if path.is_file():
        try:
            obj=json.loads(path.read_text(encoding="utf-8-sig"))
            if (not isinstance(obj,dict) or set(obj)!={"provider","model","mode"}
                    or not all(isinstance(v,str) and v for v in obj.values())):
                raise ValueError
            return obj
        except (OSError,ValueError):
            raise RuntimeError("本机 DeepSeek 路由偏好文件无效，请修正配置或显式选择") from None
    return dict(provider=native.get("provider"),model=native.get("model"),mode="cli")


def resolve(selection):
    routes,native,_ = discover()
    selected = _selection(native) if selection is None else selection
    if not isinstance(selected,dict) or selected.get("mode") not in ("desktop","cli"):
        raise RuntimeError("无效的 DSH 执行模式")
    if selected["mode"] == "desktop":
        raise RuntimeError(DESKTOP_LIMIT)
    rid,model=selected.get("provider"),selected.get("model")
    if not isinstance(rid,str) or rid not in routes or not isinstance(model,str) or model not in routes[rid]["models"]:
        raise RuntimeError("DSH 指定的供应商、凭据绑定或模型已不可用，请重新明确选择")
    return rid,model,routes[rid]


def choices(cli_ready):
    routes,native,notices = discover()
    try:
        selected = _selection(native)
    except RuntimeError as exc:
        selected = {}
        notices.append(str(exc))
    if selected.get("provider") not in routes or selected.get("model") not in routes[selected["provider"]]["models"]:
        notices.append("本机所选路由已失效，请重新选择")
    providers = [{"id":rid,"label":route["label"],"models":[
        {"id":mid,"label":m.get("name") or mid} for mid,m in route["models"].items()
    ]} for rid,route in routes.items()]
    return dict(providers=providers,default=selected,modes=[
        dict(id="cli",label="Harness CLI",available=cli_ready,detail="隔离的上游 Harness headless"),
        dict(id="desktop",label="DeepSeek Harness Desktop",available=False,detail=DESKTOP_LIMIT),
    ],downloads=[
        dict(label="Windows 桌面版（官方）",url="https://download.deepseek.com/desktop/dsh-latest-windows-x64.exe"),
        dict(label="Harness 官网与平台下载",url="https://www.deepseek.com/zh/harness/"),
        dict(label="CLI 安装说明",url="https://github.com/deepseek-ai/deepseek-harness#run-from-npm"),
    ],detail="配置实时读取，服务端可用性以实际调用为准。"+"。".join(notices))
