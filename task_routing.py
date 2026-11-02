"""Public model choices from each harness's authorized, currently selected source."""
from concurrent.futures import ThreadPoolExecutor


def _identifier(value):
    return isinstance(value,str) and 0 < len(value) <= 256 and bool(value.strip()) and value.isprintable()


def _row(rows, value):
    # A duplicated route ID is ambiguous, even if one copy is marked available.
    matches = [r for r in rows if isinstance(r,dict) and r.get("id") == value]
    return matches[0] if len(matches) == 1 else {}


def _options(catalog, selection):
    if (not isinstance(selection,dict) or not isinstance(catalog,dict)
            or catalog.get("available",True) is not True):
        return None
    provider, mode, default = (selection.get(k) for k in ("provider","mode","model"))
    if not all(_identifier(v) for v in (provider,mode,default)):
        return None
    entry = _row(catalog.get("modes",[]),mode)
    if entry.get("available") is not True:
        return None
    if "providers" in entry and (not isinstance(entry["providers"],list) or provider not in entry["providers"]):
        return None
    source = _row(catalog.get("providers",[]),provider)
    if not source or source.get("available",True) is not True:
        return None
    ids = {}
    for model in source.get("models",[]):
        if isinstance(model,dict) and _identifier(model.get("id")):
            mid = model["id"]
            ids[mid] = ids.get(mid,True) and model.get("available",True) is True
    if not ids.get(default):
        return None
    models = [default,*(mid for mid,available in ids.items() if available and mid != default)]
    return {"provider":provider,"mode":mode,"default_model":default,"models":models}


def discover_options(adapters, ticket_routes, *, legacy_claude_route=None):
    """Refresh adapter catalogs, export only executable IDs, and never change provider or mode.

    A pinned selection supersedes a legacy binding, which supersedes the adapter's
    own current default. A missing binding or model omits that harness entirely.
    Discovery delegates to adapter.choices(); this module has no network transport.
    """
    if ticket_routes is None:
        ticket_routes = {}
    if not isinstance(ticket_routes,dict) or not adapters:
        return {}

    def discover(agent):
        try:
            catalog = adapters[agent].choices()
            if agent in ticket_routes:
                selected = ticket_routes[agent]
            elif agent == "claude" and legacy_claude_route is not None:
                selected = catalog.get("legacy_defaults",{}).get(legacy_claude_route)
            else:
                selected = catalog.get("default")
            return _options(catalog,selected)
        except Exception:
            # Adapter failures can contain private config or subprocess output.
            # An unreadable source grants no route and cannot hide other harnesses.
            return None

    agents = [name for name in adapters if _identifier(name)]
    if not agents:
        return {}
    with ThreadPoolExecutor(max_workers=min(4,len(agents)),thread_name_prefix="route-catalog") as pool:
        return {name:row for name,row in zip(agents,pool.map(discover,agents)) if row}


def select_model(options, agent, model=None):
    """Build an executable selection from the exact snapshot offered to the planner."""
    entry = options.get(agent) if isinstance(options,dict) and _identifier(agent) else None
    if (not isinstance(entry,dict) or not isinstance(entry.get("models"),list)
            or not all(_identifier(entry.get(k)) for k in ("provider","mode","default_model"))):
        raise ValueError("该执行者当前没有已授权的模型目录")
    requested = entry.get("default_model") if model is None else model
    if not _identifier(requested) or requested not in entry["models"]:
        raise ValueError("所选模型不在该执行者当前获准的供应商与执行端目录中")
    return {"provider":entry["provider"],"mode":entry["mode"],"model":requested}
