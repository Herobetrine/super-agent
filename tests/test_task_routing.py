import json
import threading
import unittest
from types import SimpleNamespace

from task_routing import discover_options, select_model


def route(provider="selected",model="native/default",mode="cli"):
    return dict(provider=provider,model=model,mode=mode)


def catalog():
    return {
        "modes":[{"id":"cli","available":True,"providers":["selected","other"]},
                 {"id":"desktop","available":False,"manual":True}],
        "providers":[
            {"id":"selected","models":[{"id":"v2","label":"friendly name"},{"id":"native/default"},
                                       {"id":"private/model","available":False},{"id":"v2"}],
             "base_url":"https://hidden.invalid", "credential":"must-not-escape"},
            {"id":"other","models":[{"id":"outside"}]},
        ],
        "default":route(),
        "legacy_defaults":{"user":route("other","outside")},
    }


class TaskRouting(unittest.TestCase):
    def test_source_and_mode_pinning_overrides_defaults(self):
        data = catalog()
        adapters = {"claude":SimpleNamespace(choices=lambda:data)}
        effective = discover_options(adapters,{},legacy_claude_route="user")
        self.assertEqual(select_model(effective,"claude"),route("other","outside"))
        pinned = {"claude":route(model="v2")}
        choices = discover_options(adapters,pinned,legacy_claude_route="user")
        self.assertEqual(choices,{"claude":{"provider":"selected","mode":"cli",
                                            "default_model":"v2","models":["v2","native/default"]}})
        self.assertEqual(select_model(choices,"claude","native/default"),route())
        for bad_model in ("outside","friendly name","private/model","native/default\n"):
            with self.assertRaises(ValueError):
                select_model(choices,"claude",bad_model)
        for value in ("must-not-escape","hidden.invalid","friendly name"):
            self.assertNotIn(value,json.dumps(choices))

    def test_unavailable_or_disallowed_bindings_never_fall_back(self):
        fixtures = []
        for field,value in (("mode","desktop"),("provider","missing"),("model","unknown")):
            fixtures.append((catalog(),{**route(),field:value}))
        blocked = catalog()
        blocked["providers"][0]["available"] = False
        fixtures.append((blocked,route()))
        blocked = catalog()
        blocked["modes"][0]["providers"] = ["other"]
        fixtures.append((blocked,route()))
        blocked = catalog()
        blocked["providers"][0]["models"].append({"id":"native/default","available":False})
        fixtures.append((blocked,route()))
        blocked = catalog()
        blocked["modes"][0].pop("available")
        fixtures.append((blocked,route()))
        for data,pin in fixtures:
            with self.subTest(pin=pin):
                self.assertEqual(discover_options({"agent":SimpleNamespace(choices=lambda:data)},{"agent":pin}),{})
        claude = {"claude":SimpleNamespace(choices=catalog)}
        self.assertEqual(discover_options(claude,{},legacy_claude_route="cc-switch"),{})

    def test_reload_and_parallel_adapter_failure_isolation(self):
        data = catalog()
        barrier = threading.Barrier(2,timeout=2)
        def scan():
            barrier.wait()
            return data
        def broken():
            barrier.wait()
            raise RuntimeError("private provider diagnostics")
        adapters = {"ok":SimpleNamespace(choices=scan), "broken":SimpleNamespace(choices=broken)}
        first = discover_options(adapters,{})
        self.assertEqual(set(first),{"ok"})
        self.assertEqual(first["ok"]["models"],["native/default","v2"])
        data["providers"][0]["models"] = [{"id":"new"}]
        self.assertEqual(discover_options(adapters,{}),{})
        data["default"]["model"] = "new"
        second = discover_options(adapters,{})
        self.assertEqual(select_model(second,"ok"),route(model="new"))
        with self.assertRaises(ValueError):
            select_model(second,"ok","v2")
