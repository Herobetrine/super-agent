"""Exercise delegated model/permission selection through scheduler and SQLite."""
import json
from pathlib import Path
import tempfile
import threading
import time
import unittest
from types import SimpleNamespace

from engine import AGENTS, Engine, Store, TERMINAL


def send(**fields):
    command = {"id":"delegate","to":"deepseek","body":"检查 🧪\r\n原文 ","write":False}
    return "SA_SEND "+json.dumps({**command,**fields},ensure_ascii=False)


class LeadRoutingTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = Path(self.tmp.name)
        self.store = Store(self.path/"state")
        self.addCleanup(lambda: self.store.db.close())
        self.engine = None
        self.addCleanup(lambda: self.engine.close() if self.engine else None)
        self.calls = []
        self.extra = ["review-model"]
        self.configured = {"provider":"project-provider","model":"base","mode":"cli"}

    def choices(self,agent):
        return {
            "default":{"provider":"user-default","model":"small","mode":"cli"},
            "providers":[
                {"id":"user-default","label":"Current native provider","available":True,
                 "models":[{"id":m,"label":m} for m in ("small","native-alt")]},
                {"id":"project-provider","label":"Project provider","available":True,
                 "models":[{"id":m,"label":m} for m in ["base",*self.extra]]+
                          [{"id":"retired","label":"Unavailable model","available":False}]}],
            "modes":[{"id":"cli","label":"CLI","available":True},
                     {"id":"desktop","label":"Desktop","available":False}],
        }

    def start(self, handler, capacity=4):
        def adapter(name):
            def run(prompt,cwd,emit,cancel,**opts):
                packet=json.loads(prompt.rsplit("\n",1)[1])
                self.calls.append((name,packet,opts))
                answer=handler(name,packet,emit,cancel)
                return answer,"session-"+str(packet["job"])
            return SimpleNamespace(run=run,choices=lambda:self.choices(name))
        self.engine=Engine(self.store,{a:adapter(a) for a in AGENTS},capacity=capacity,timeout=8)
        self.engine.start()

    def create(self,enabled=True,profile="full",use_defaults=False):
        selections={} if use_defaults else {"deepseek":self.configured.copy()}
        return self.store.create("route this ticket",self.path,"codex",selections=selections,
                                 permission_profile=profile,allow_lead_routing=enabled)

    def jobs(self,ticket):
        return self.store.rows("SELECT * FROM jobs WHERE ticket=? ORDER BY id",(ticket,))

    def wait(self,fn):
        end=time.monotonic()+5
        while time.monotonic()<end:
            result=fn()
            if result:
                return result
            time.sleep(.015)
        self.fail(str(self.store.rows("SELECT id,turn,state,result FROM jobs")))

    def finish(self,ticket):
        self.wait(lambda:self.jobs(ticket)[0]["state"] in TERMINAL)
        return self.jobs(ticket)

    def test_child_gets_exact_model_within_the_ticket_provider_and_lower_permission(self):
        wire=send(model="review-model",permission_profile="readonly")
        def worker(name,p,emit,cancel):
            if p["depth"] == 0 and p["turn"] == 1:
                menu=p["delegate_models"]["deepseek"]
                self.assertEqual((menu["provider"],menu["mode"],menu["default_model"]),
                                 ("project-provider","cli","base"))
                self.assertIn("review-model",menu["models"])
                self.assertNotIn("native-alt",menu["models"])
                self.assertNotIn("retired",menu["models"])
                for part in (wire[:5],wire[5:]+"\n"):
                    emit("delta",part)
                return wire
            return "verified"
        ticket=self.create()
        self.start(worker)
        root,child=self.finish(ticket)
        self.assertEqual((root["state"],child["state"]),("done","done"))
        self.assertEqual(child["permission_override"],"readonly")
        selected={**self.configured,"model":"review-model"}
        self.assertEqual(json.loads(child["route_override"]),selected)
        [(name,p,opts)]=[call for call in self.calls if call[0]=="deepseek"]
        self.assertEqual(opts["selection"],selected)
        self.assertFalse(opts["writable"])
        self.assertIsNone(opts.get("permission_profile"))
        self.assertFalse(p["this_turn_can_write"])
        self.assertEqual(p["permission_profile"],"readonly")
        self.assertEqual(p["assignment"],"检查 🧪\r\n原文 ")
        self.assertEqual(len(self.store.rows("SELECT seq FROM events WHERE kind='send'")),1)
        self.assertEqual(self.store.get_routes(ticket)["deepseek"],self.configured)

    def test_native_provider_default_and_permission_only_override(self):
        ticket=self.create(use_defaults=True)
        def worker(name,p,emit,cancel):
            if p["depth"]==0 and p["turn"]==1:
                return "\n".join((send(id="only-model",model="native-alt"),
                                  send(id="only-perm",permission_profile="partial",write=True)))
            return "reviewed"
        self.start(worker)
        jobs=self.finish(ticket)
        self.assertEqual([r["state"] for r in jobs],["done","done","done"])
        picked={r["msgkey"]:r["id"] for r in jobs[1:]}
        opts={p["job"]:o for a,p,o in self.calls if a=="deepseek"}
        model_only=opts[picked["only-model"]]
        self.assertEqual(model_only["selection"],{"provider":"user-default","model":"native-alt","mode":"cli"})
        self.assertEqual(model_only["permission_profile"],"full")
        self.assertTrue(model_only["writable"])
        perm_only=opts[picked["only-perm"]]
        self.assertEqual(perm_only["selection"],{"provider":"user-default","model":"small","mode":"cli"})
        self.assertTrue(perm_only["writable"])
        self.assertEqual(perm_only["permission_profile"],"partial")

    def test_disabled_unknown_foreign_model_or_escalation_never_runs_child(self):
        by_ticket={}
        def worker(name,p,emit,cancel):
            return by_ticket[p["ticket"]] if not p["depth"] else "unexpected execution"
        cases=[
            (False,"full",{"model":"review-model"}),
            (False,"full",{"permission_profile":"readonly"}),
            (True,"readonly",{"permission_profile":"full"}),
            (True,"ask",{"permission_profile":"partial"}),
            (True,"partial",{"permission_profile":"invented"}),
            (True,"full",{"model":"native-alt"}),
            (True,"full",{"model":"missing-model"}),
            (True,"full",{"model":"retired"}),
            (True,"full",{"provider":"user-default","model":"native-alt"}),
            (True,"full",{"mode":"desktop","model":"review-model"}),
            (True,"full",{"selection":{"provider":"user-default","model":"native-alt","mode":"cli"}}),
        ]
        for enabled,profile,fields in cases:
            by_ticket[self.create(enabled,profile)]=send(**fields)
        self.start(worker)
        for tid in by_ticket:
            with self.subTest(command=by_ticket[tid]):
                jobs=self.finish(tid)
                self.assertIn(jobs[0]["state"],("failed","partial"))
                self.assertFalse([x for x in self.calls if x[1]["ticket"]==tid and x[1]["depth"]>0])

    def test_user_change_during_catalog_discovery_prevents_stale_auto_finish(self):
        selecting,release=threading.Event(),threading.Event()
        normal=self.choices
        def paused(agent):
            selecting.set()
            release.wait(4)
            return normal(agent)
        self.choices=paused
        ticket=self.create(profile="auto")
        def worker(name,p,emit,cancel):
            return 'SA_FINISH {"status":"done","summary":"checked"}'
        self.start(worker)
        try:
            self.assertTrue(selecting.wait(4))
            self.store.follow(ticket,"updated while opening model directories")
        finally:
            release.set()
        root=self.finish(ticket)[0]
        self.assertEqual(root["turn"],2)
        inputs=[p for _,p,_ in self.calls]
        self.assertEqual(inputs[-1]["objective"],"updated while opening model directories")
        self.assertEqual(inputs[-1]["user_updates"],["updated while opening model directories"])
        self.assertEqual(self.store.rows("SELECT turn FROM events WHERE ticket=? AND kind='auto.finish'",(ticket,)),
                         [{"turn":2}])

    def test_worker_cannot_override_or_launder_its_readonly_grant(self):
        cases=[
            send(id="model",to="claude",model="native-alt"),
            send(id="raise",to="claude",permission_profile="full"),
            send(id="write",to="codex",write=True),
            send(id="plain",to="claude"),
        ]
        tasks={self.create():case for case in cases}
        def worker(name,p,emit,cancel):
            if p["depth"]==0 and p["turn"]==1:
                return send(permission_profile="readonly")
            if p["depth"]==1 and p["turn"]==1:
                return tasks[p["ticket"]]
            return "result"
        self.start(worker)
        for tid,wire in tasks.items():
            with self.subTest(wire=wire):
                jobs=self.finish(tid)
                grandchildren=[o for _,p,o in self.calls if p["ticket"]==tid and p["depth"]==2]
                if wire==cases[-1]:
                    self.assertEqual(jobs[0]["state"],"done")
                    self.assertEqual(len(grandchildren),1)
                    self.assertFalse(grandchildren[0]["writable"])
                    self.assertNotIn(grandchildren[0].get("permission_profile"),("full","auto"))
                else:
                    self.assertEqual(jobs[1]["state"],"failed")
                    self.assertFalse(grandchildren)

    def test_conflicting_replay_does_not_mutate_the_original_grant(self):
        cases=[{"model":"base"},{"permission_profile":"full"}]
        changes={self.create():case for case in cases}
        original={"model":"review-model","permission_profile":"readonly"}
        def worker(name,p,emit,cancel):
            if p["depth"]==0:
                emit("delta",send(**original)+"\n")
                return send(**{**original,**changes[p["ticket"]]})
            return "verified"
        self.start(worker)
        for tid in changes:
            with self.subTest(change=changes[tid]):
                root,child=self.finish(tid)
                self.assertEqual(root["state"],"failed")
                self.assertEqual(json.loads(child["route_override"])["model"],"review-model")
                self.assertEqual(child["permission_override"],"readonly")
                self.assertEqual(len(self.store.rows("SELECT seq FROM events WHERE ticket=? AND kind='send'",(tid,))),1)

    def test_interrupted_root_replays_once_even_if_the_catalog_changes(self):
        wire=send(model="review-model",permission_profile="readonly")
        child_finished=threading.Event()
        root_waiting=threading.Event()
        def initial(name,p,emit,cancel):
            if p["depth"]==0:
                emit("delta",wire+"\n")
                root_waiting.set()
                cancel.wait(6)
                raise RuntimeError("interrupted before parent reply")
            child_finished.set()
            return "original report"
        ticket=self.create(profile="auto")
        self.start(initial)
        self.assertTrue(root_waiting.wait(4))
        self.assertTrue(child_finished.wait(4))
        self.wait(lambda:self.jobs(ticket)[1]["state"]=="done")
        self.engine.close()
        self.store.db.close()
        self.store=Store(self.path/"state")
        self.extra.clear()
        def recovered(name,p,emit,cancel):
            return wire+'\nSA_FINISH {"status":"done","summary":"existing report verified"}'
        self.start(recovered)
        root,child=self.finish(ticket)
        self.assertEqual(root["state"],"done")
        self.assertEqual(child["result"],"original report")
        self.assertEqual(len([x for x in self.calls if x[0]=="deepseek"]),1)
        self.assertEqual(len(self.store.rows("SELECT seq FROM events WHERE ticket=? AND kind='send'",(ticket,))),1)
