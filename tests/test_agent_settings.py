import json
import os
from pathlib import Path
import tempfile
import threading
import time
import unittest
from types import SimpleNamespace

os.environ.setdefault("QT_QPA_PLATFORM","offscreen")
from PyQt6.QtWidgets import QApplication

from agent_settings import AgentSettings, SUPPORT
from engine import Engine, Store


def choice(provider="configured",model="first",mode="cli"):
    return dict(mode=mode,provider=provider,model=model)


class AgentSettingsTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.app=QApplication.instance() or QApplication([])

    def setUp(self):
        temp=tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.base=Path(temp.name)
        self.store=Store(self.base/"state")
        self.addCleanup(self.store.db.close)

    def wait(self,predicate):
        deadline=time.monotonic()+7
        while time.monotonic()<deadline:
            self.app.processEvents()
            if predicate():
                return
            time.sleep(.02)
        self.fail("timeout")

    def test_catalog_is_live_and_never_replaces_removed_selection(self):
        file=self.base/"provider.json"
        file.write_text('["first"]')
        def catalog():
            models=json.loads(file.read_text())
            return dict(providers=[dict(id="configured",label="本机配置",
                        models=[dict(id=m,label=m) for m in models])],
                        modes=[dict(id="cli",label="CLI",available=True),
                               dict(id="desktop",label="桌面",available=False,detail="尚未打通")],
                        default=choice(model=models[0]))
        adapter=SimpleNamespace(choices=catalog,watch_paths=lambda:[file])
        ui=AgentSettings({a:adapter for a in SUPPORT},{})
        ui.show()
        self.addCleanup(ui.reject)
        self.wait(lambda:not ui.pending)
        page=ui.pages["deepseek"]
        self.assertFalse(page.mode.model().item(page.mode.findData("desktop")).isEnabled())
        file.write_text('["first","dynamically-added"]')
        self.wait(lambda:page.model.findData("dynamically-added")>=0)
        page.model.setCurrentIndex(page.model.findData("dynamically-added"))
        page.model.activated.emit(page.model.currentIndex())
        self.assertEqual(ui.selections()["deepseek"],choice(model="dynamically-added"))
        file.write_text('["another"]')
        self.wait(lambda:page.model.findData("another")>=0)
        self.assertFalse(page.valid())
        self.assertEqual(ui.selections()["deepseek"]["model"],"dynamically-added")
        page.use_default()
        self.assertNotIn("deepseek",ui.selections())
        self.assertEqual(page.selected()["model"],"another")

    def test_routes_snapshot_dispatch_resume_and_change(self):
        routes={"codex":choice(),"claude":choice(model="child-model")}
        self.store.save_routes(routes)
        tid=self.store.create("hello",self.base,"codex")
        self.store.save_routes({"codex":choice(model="later")})
        self.assertEqual(self.store.get_routes(tid),routes)
        received=[]
        def run(prompt,cwd,emit,cancel,session_id=None,**kw):
            packet=json.loads(prompt.rsplit("\n",1)[1])
            received.append((packet["agent"],session_id,kw.get("selection")))
            self.assertEqual(packet["selected_execution"],kw["selection"])
            if packet["agent"]=="codex" and packet["turn"]==1:
                return 'SA_SEND {"id":"one","to":"claude","body":"check"}',"a-session"
            return "done","a-session"
        engine=Engine(self.store,{a:SimpleNamespace(run=run) for a in SUPPORT})
        self.addCleanup(engine.close)
        with self.assertRaises(ValueError):
            self.store.save_routes({},ticket=tid)
        engine.start()
        done=lambda:self.store.rows("SELECT state FROM jobs WHERE ticket=? AND parent IS NULL",(tid,))[0]["state"]=="done"
        self.wait(done)
        self.assertEqual(received,[("codex",None,routes["codex"]),("claude",None,routes["claude"]),
                                   ("codex","a-session",routes["codex"])])
        self.store.save_routes({"codex":choice(model="fresh"),"claude":routes["claude"]},ticket=tid)
        self.store.follow(tid,"continue")
        self.wait(done)
        self.assertEqual(received[-1],("codex",None,choice(model="fresh")))
        self.assertEqual(self.store.get_routes()["codex"]["model"],"later")

    def test_unavailable_desktop_does_not_start_other_transport(self):
        tid=self.store.create("test",self.base,"codex",selections={"codex":choice(mode="desktop")})
        started=threading.Event()
        fake=SimpleNamespace(choices=lambda:dict(modes=[dict(id="desktop",available=False)]),
                             run=lambda *args,**kwargs:started.set())
        e=Engine(self.store,{"codex":fake})
        self.addCleanup(e.close)
        e.start()
        self.wait(lambda:self.store.rows("SELECT state FROM jobs WHERE ticket=?",(tid,))[0]["state"]=="failed")
        self.assertFalse(started.is_set())

    def test_changed_native_default_affects_new_jobs_without_retargeting_a_session(self):
        configured=[choice(model="initial")]
        used=[]
        def run(prompt,cwd,emit,cancel,**options):
            used.append(options["selection"]["model"])
            return "done","native"
        adapter=SimpleNamespace(choices=lambda:dict(default=configured[0]),run=run)
        engine=Engine(self.store,{"codex":adapter})
        self.addCleanup(engine.close)
        ticket=self.store.create("start",self.base,"codex")
        engine.start()
        self.wait(lambda:self.store.rows("SELECT state FROM jobs WHERE ticket=?",(ticket,))[0]["state"]=="done")
        configured[0]=choice(model="new")
        self.store.follow(ticket,"continue")
        self.wait(lambda:self.store.rows("SELECT state FROM jobs WHERE ticket=?",(ticket,))[0]["state"]=="done")
        fresh=self.store.create("new chat",self.base,"codex")
        self.wait(lambda:self.store.rows("SELECT state FROM jobs WHERE ticket=?",(fresh,))[0]["state"]=="done")
        self.assertEqual(used,["initial","initial","new"])
