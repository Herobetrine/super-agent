"""Replay committed wire commands after an interrupted autonomous parent turn."""
import json
from pathlib import Path
import tempfile
import threading
import time
import unittest
from types import SimpleNamespace

from engine import Engine, Store


class AutoRecoveryTests(unittest.TestCase):
    def test_follow_invalidates_finished_and_inflight_milestones(self):
        with tempfile.TemporaryDirectory() as tmp:
            project = Path(tmp)
            store = Store(project / "state")
            ticket = store.create("old cleanup",project,"codex",permission_profile="auto")
            entered,release = threading.Event(),threading.Event()
            packets = []
            def worker(prompt,cwd,emit,cancel,**opts):
                packet = json.loads(prompt.rsplit("\n",1)[1])
                packets.append(packet)
                if packet["turn"] == 1:
                    return 'SA_GOAL {"objective":"clean staging"}\nSA_NEXT {"body":"remove old staging"}',None
                if packet["turn"] == 3:
                    entered.set()
                    release.wait(4)
                    return 'SA_GOAL {"objective":"superseded"}\nSA_NEXT {"body":"stale in-flight plan"}',None
                return 'SA_FINISH {"status":"done","summary":"verified"}',None

            def await_done():
                deadline = time.monotonic()+5
                while time.monotonic()<deadline:
                    if store.rows("SELECT state FROM jobs WHERE ticket=?",(ticket,))[0]["state"]=="done":
                        return
                    time.sleep(.01)
                self.fail(str(store.rows("SELECT state,turn FROM jobs")))

            engine = Engine(store,{"codex":SimpleNamespace(run=worker)})
            engine.start()
            try:
                await_done()
                store.follow(ticket,"Inspect only; stop old cleanup.")
                self.assertTrue(entered.wait(4))
                self.assertEqual(packets[2]["objective"],"Inspect only; stop old cleanup.")
                self.assertIsNone(packets[2]["next_step"])
                store.follow(ticket,"Final instruction; drop previous inspection.")
                release.set()
                await_done()
                self.assertEqual(packets[3]["objective"],"Final instruction; drop previous inspection.")
                self.assertIsNone(packets[3]["next_step"])
                self.assertEqual(len(packets),4)
            finally:
                release.set()
                engine.close()
                store.db.close()

    def test_two_recoveries_replay_writer_and_note_once(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        project = Path(tmp.name)
        state = project / "state"
        store = Store(state)
        self.addCleanup(lambda: store.db.close())
        ticket = store.create("fixture", project, "codex", permission_profile="auto")

        def parent():
            return store.rows("SELECT * FROM jobs WHERE ticket=? AND parent IS NULL",(ticket,))[0]

        def started(jid):
            with store.tx() as db:
                db.execute("UPDATE jobs SET state='running' WHERE id=?",(jid,))

        initial = parent()
        started(initial["id"])
        cmd = {"id":"write-once","to":"deepseek","body":"apply once","write":True}
        child = store.dispatch(initial,cmd)
        note = {"id":"clarify","job":child,"body":"逐字\r\n保留空格 "}
        receipt = store.note_agent(initial,note)
        # The side effect has already finished, but the parent never committed
        # its reply. Both stream records were safely committed in turn one.
        with store.tx() as db:
            db.execute("UPDATE jobs SET state='done',result='saved' WHERE id=?",(child,))
        for expected_turn in (2,3):
            store.db.close()
            store = Store(state)
            self.assertEqual((parent()["state"],parent()["turn"],parent()["dispatch_turn"]),
                             ("queued",expected_turn,1))
            if expected_turn == 2:
                # A second interruption must still refer to the original turn.
                started(initial["id"])
        wire = "SA_SEND "+json.dumps(cmd)+"\nSA_NOTE "+json.dumps(note,ensure_ascii=False)
        ran = []

        def worker(prompt,cwd,emit,cancel,**opts):
            packet = json.loads(prompt.rsplit("\n",1)[1])
            ran.append(packet["job"])
            emit("delta",wire+"\n")
            return wire+'\nSA_FINISH {"status":"done","summary":"checked"}', None

        engine = Engine(store,{name:SimpleNamespace(run=worker) for name in ("codex","deepseek")})
        engine.start()
        try:
            deadline = time.monotonic()+4
            while parent()["state"]!="done" and time.monotonic()<deadline:
                time.sleep(.01)
        finally:
            engine.close()
        self.assertEqual(ran,[initial["id"]])
        self.assertEqual((parent()["state"],parent()["dispatch_turn"]),("done",None))
        self.assertEqual(store.rows("SELECT id FROM jobs WHERE parent=?",(initial["id"],)),[{"id":child}])
        self.assertEqual(store.rows("SELECT seq,source_turn,target FROM mailbox WHERE source=?",(initial["id"],)),
                         [{"seq":receipt,"source_turn":1,"target":child}])
        self.assertFalse(store.rows("SELECT seq FROM events WHERE kind='note.rejected'"))

        # The stable key only carries across recovery. Fresh human-directed
        # work can use the same message key for a genuinely new assignment.
        store.follow(ticket,"another authorized change")
        later = parent()
        started(later["id"])
        self.assertNotEqual(child,store.dispatch(later,{**cmd,"body":"apply different change"}))
