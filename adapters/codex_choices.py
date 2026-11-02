"""Discover selectable Codex routes without exporting provider credentials."""
import http.client
import ipaddress
import json
import os
from pathlib import Path
import re
import socket
import subprocess
from urllib.parse import urlsplit

from .process import CREATE_FLAGS


def _home():
    return Path(os.environ.get("CODEX_HOME") or Path.home() / ".codex").expanduser().resolve()


def profile_path():
    name = os.environ.get("SUPERAGENT_CODEX_PROFILE","").strip()
    if not name:
        return None
    if not re.fullmatch(r"[a-zA-Z0-9_-]+",name):
        raise ValueError("SUPERAGENT_CODEX_PROFILE 须为有效的 Codex profile 名")
    return _home() / (name + ".config.toml")


def _read_toml(path):
    if not path.is_file():
        return {}
    try:
        import tomllib
    except ImportError:  # Python 3.10, including standard Windows pip installations
        try:
            import tomli as tomllib
        except ImportError:
            from pip._vendor import tomli as tomllib
    with path.open("rb") as source:
        return tomllib.load(source)


def _overlay(base, overrides):
    result = dict(base)
    for key, value in overrides.items():
        if isinstance(value,dict) and isinstance(base.get(key),dict):
            value = _overlay(base[key],value)
        result[key] = value
    return result


def _settings():
    base = _read_toml(_home() / "config.toml")
    profile = profile_path()
    if profile:
        if not profile.is_file():
            raise ValueError("所选 Codex profile 文件不存在")
        base = _overlay(base,_read_toml(profile))
    return base


def _catalog_path(config):
    if isinstance(config.get("model_catalog_json"),str) and config["model_catalog_json"]:
        return _home() / Path(config["model_catalog_json"])


def watch_paths():
    directory = _home()
    paths = [directory / "config.toml", directory / "models_cache.json"]
    paths.extend(sorted(directory.glob("*.config.toml")))
    try:
        profile = profile_path()
        if profile:
            paths.append(profile)
        catalog = _catalog_path(_settings())
        if catalog:
            paths.append(catalog)
    except (OSError, ValueError, ImportError):
        pass
    return list(dict.fromkeys(paths))


def thread_settings(provider, config):
    """Send private profile routing values over RPC, never via process arguments."""
    if not profile_path():
        return {}
    values = {}
    definition = (config.get("model_providers") or {}).get(provider)
    if definition:
        values["model_providers"] = {provider:definition}
    if provider == "openai" and config.get("openai_base_url"):
        values["openai_base_url"] = config["openai_base_url"]
    catalog = _catalog_path(config)
    if catalog and provider == (config.get("model_provider") or "openai"):
        values["model_catalog_json"] = str(catalog)
    return values


def _item(name, label=None):
    if isinstance(name,str) and re.fullmatch(r"[\w][\w./:+-]{0,199}",name):
        return {"id":name, "label":str(label or name)}


def endpoint_overrides(provider, config):
    """Keep project configuration from changing a selected gateway's origin."""
    cfg = (config.get("model_providers") or {}).get(provider,{})
    address = cfg.get("base_url")
    key = f"model_providers.{provider}.base_url"
    if provider == "openai" and config.get("openai_base_url"):
        key,address = "openai_base_url",config["openai_base_url"]
    if not address:
        return []
    url = urlsplit(address)
    if url.scheme not in ("http","https") or not url.hostname or url.username or url.password or url.fragment or url.query:
        raise RuntimeError("该供应商地址无法安全固定，请核对 Codex 个人配置")
    args = ["-c",key+"="+json.dumps(address)]
    if profile_path() and cfg:
        # Register a profile-only provider for the daemon's global ID check.
        args += ["-c",f"model_providers.{provider}.name="+json.dumps(provider)]
    return args


def _cli_catalog(exe, provider, *, bundled=False):
    result = subprocess.run(
        [exe, "-c", "model_provider="+json.dumps(provider), "debug", "models", *(["--bundled"] if bundled else [])],
        stdin=subprocess.DEVNULL, capture_output=True, timeout=8,
        cwd=Path.home(), creationflags=CREATE_FLAGS,
    )
    if result.returncode:
        return []
    return [_item(m.get("slug"),m.get("display_name")) for m in json.loads(result.stdout)["models"]
            if m.get("visibility") == "list"]


def _provider_catalog(cfg):
    # A gateway may advertise a different catalog from the CLI's bundled one.
    # Use an explicit same-origin HTTPS catalog for remote providers. Only
    # local loopback providers get an implicit /models lookup. http.client
    # uses no proxy, follows no redirect and won't authenticate another host.
    url = urlsplit(cfg.get("base_url") or "")
    try:
        local = ipaddress.ip_address(url.hostname or "").is_loopback
    except ValueError:
        local = url.hostname == "localhost" and all(
            ipaddress.ip_address(addr[4][0]).is_loopback for addr in socket.getaddrinfo("localhost",None)
        )
    if url.scheme not in ("http","https") or url.username or url.password or url.query or url.fragment:
        return [],"当前供应商未提供可安全读取的模型目录"
    endpoint = urlsplit(cfg.get("model_catalog_url") or "")
    if endpoint.geturl():
        if (endpoint.scheme,endpoint.hostname,endpoint.port) != (url.scheme,url.hostname,url.port) or endpoint.username or endpoint.password or endpoint.fragment:
            return [],"已配置的模型目录不在同一认证来源"
        if not local and endpoint.scheme != "https":
            return [],"远端模型目录需要 HTTPS"
        path = endpoint.path + ("?" + endpoint.query if endpoint.query else "")
    elif local:
        path = url.path.rstrip("/")+"/models"
    else:
        return [],"未配置该供应商的同源 model_catalog_url 或本地模型目录"
    connection = http.client.HTTPSConnection if url.scheme == "https" else http.client.HTTPConnection
    client = connection(url.hostname, url.port, timeout=3)
    try:
        headers = dict(cfg.get("http_headers") or {})
        for name, source in (cfg.get("env_http_headers") or {}).items():
            if source in os.environ:
                headers[name] = os.environ[source]
        token = cfg.get("experimental_bearer_token") or os.environ.get(cfg.get("env_key") or "")
        if token:
            headers["Authorization"] = "Bearer "+token
        client.request("GET",path,headers=headers)
        response = client.getresponse()
        if response.status != 200:
            return [],f"模型目录返回 HTTP {response.status}"
        raw = response.read(1024*1024 + 1)
        if len(raw) > 1024*1024:
            return [],"模型目录超过 1 MiB"
        data = json.loads(raw)
        if isinstance(data.get("models"),list):
            return _file_models(data),"供应商模型目录"
        return [_item(m.get("id")) for m in data.get("data",[]) if isinstance(m,dict)],"供应商模型目录"
    finally:
        client.close()


def _file_models(catalog):
    return [_item(m.get("slug"),m.get("display_name")) for m in catalog.get("models",[])
            if isinstance(m,dict) and m.get("visibility","list") == "list"]


def _saved_models(effective):
    """Include explicit model IDs saved for the same effective provider definition."""
    base = _read_toml(_home() / "config.toml")
    defaults = [base,effective]
    for path in sorted(_home().glob("*.config.toml")):
        try:
            defaults.append(_overlay(base,_read_toml(path)))
        except (OSError, ValueError):
            continue
    result = {}
    custom = effective.get("model_providers") or {}
    for config in defaults:
        provider = config.get("model_provider") or "openai"
        if (config.get("model_providers") or {}).get(provider) != custom.get(provider):
            continue
        if provider == "openai" and config.get("openai_base_url") != effective.get("openai_base_url"):
            continue
        model = _item(config.get("model"))
        if model:
            result.setdefault(provider,{})[model["id"]] = model
    return result


def choices(exe, *, config=None):
    try:
        if config is None:
            config = _settings()
        saved = _saved_models(config)
        issue = ""
    except (OSError, ValueError, ImportError,TypeError):
        config = {}
        saved = {}
        issue = "Codex 配置或选定的 profile 无法读取，请检查配置"
    default_provider = config.get("model_provider") or "openai"
    custom = config.get("model_providers") or {}
    providers = []
    for name in dict.fromkeys(["openai",*custom,default_provider]) if not issue else ():
        if not isinstance(name,str) or not re.fullmatch(r"[a-zA-Z0-9_-]{1,96}",name):
            continue
        cfg = custom.get(name,{})
        if name == "openai" and config.get("openai_base_url"):
            cfg = {**cfg,"base_url":config["openai_base_url"]}
        models = []
        detail = "没有当前供应商的模型声明"
        catalog = _catalog_path(config)
        try:
            if catalog and name == default_provider:
                if catalog.stat().st_size > 32*1024*1024:
                    raise ValueError("模型目录过大")
                models = _file_models(json.loads(catalog.read_text(encoding="utf-8-sig")))
                detail = "已应用当前 profile 或配置声明的本地 model_catalog_json"
            elif cfg:
                models,detail = _provider_catalog(cfg)
            elif exe and name == "openai":
                models = _cli_catalog(exe,name,bundled=bool(catalog))
                detail = "Codex 原生模型目录" if models else "Codex 未返回模型目录"
        except (OSError, subprocess.TimeoutExpired, ValueError, TypeError, KeyError, http.client.HTTPException) as exc:
            detail = f"读取此供应商模型目录失败（{type(exc).__name__}）"
        unique = {m["id"]:m for m in models if m}
        for model in saved.get(name,{}).values():
            if model["id"] not in unique:
                unique[model["id"]] = {"id":model["id"],"label":model["label"]+"（本机配置）"}
        if not models and unique:
            detail += "，已列出同一来源配置中明确保存的 model"
        providers.append({"id":name, "label":cfg.get("name") or ("OpenAI" if name == "openai" else name),
                          "models":list(unique.values()), "detail":detail})
    default_models = next((p["models"] for p in providers if p["id"] == default_provider),[])
    default_model = config.get("model") or (default_models[0]["id"] if default_models else "")
    return {
        "providers":providers,
        "modes":[{"id":"cli","label":"Codex CLI · app-server","available":bool(exe and not issue),
                  "detail":"平台通过原生 stdio 协议执行，逐轮核对供应商及模型。"},
                 {"id":"desktop","label":"Codex 桌面应用","available":False,
                  "detail":"尚无已验证的桌面应用自动派工与回传通道。"}],
        "default":{"provider":default_provider,"model":default_model,"mode":"cli"},
        "downloads":[{"label":"Codex CLI 官方安装","url":"https://learn.chatgpt.com/docs/cli"},
                     {"label":"Windows 桌面端官方下载","url":"https://get.microsoft.com/installer/download/9PLM9XGG6VKS?cid=website_cta_psi"}],
        "detail":issue or "使用当前 CODEX_HOME 和可选 profile。模型目录可见性与账号推理权限分别验证。",
    }


def select(exe, selection):
    """Resolve a route once at admission and keep it stable throughout the turn."""
    if selection is None and not profile_path():
        return {},[],{}
    if selection is not None and (not isinstance(selection,dict) or selection.get("mode") != "cli"):
        raise RuntimeError("Codex 目前仅有 CLI 自动执行通道")
    try:
        config = _settings()
    except (OSError, ValueError, ImportError) as exc:
        raise RuntimeError("Codex 个人配置或选定的 profile 无法读取") from exc
    catalog = choices(exe,config=config)
    selection = catalog["default"] if selection is None else selection
    provider, model = selection.get("provider"), selection.get("model")
    valid = any(p["id"] == provider and any(m["id"] == model for m in p["models"])
                for p in catalog["providers"])
    if not valid:
        raise RuntimeError("所选 Codex 供应商或模型已不在本机目录中，请刷新选择")
    flags = ["-c","model_provider="+json.dumps(provider),*endpoint_overrides(provider,config)]
    return {"modelProvider":provider,"model":model},flags,thread_settings(provider,config)
