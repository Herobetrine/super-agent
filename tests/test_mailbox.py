"""Mailbox integration checks with controlled native transports and real SQLite."""
import json
import sqlite3
import tempfile
import threading
import time
import unittest
from pathlib import Path
from types import SimpleNamespace

from engine import AGENTS, Engine, MAX_AGENT_NOTES, Store, digest


class MailboxTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.path = Path(self.temp.name)
        self.store = Store(self.path / "state")
        self.engine = None

    def tearDown(self):
        if self.engine:
            self.engine.close()
        self.store.db.close()
        self.temp.cleanup()

    def start(self, native):
        def run(prompt, cwd, emit, cancel, **kw):
            return native(json.loads(prompt.rsplit("\n",1)[1]), emit), None
        self.engine = Engine(self.store, {a:SimpleNamespace(run=run) for a in AGENTS}, timeout=12)
        self.engine.start()

    def wait(self, check):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            value = check()
            if value:
                return value
            time.sleep(.015)
        self.fail(str(self.store.rows("SELECT id,state,result FROM jobs")))

    def jobs(self,ticket):
        return self.store.rows("SELECT * FROM jobs WHERE ticket=? ORDER BY id",(ticket,))

    def test_steering_exact_once_without_extra_turn(self):
        ticket = self.store.create("即时补充", self.path, "codex")
        root = self.jobs(ticket)[0]["id"]
        active = threading.Event()
        seen = []
        def worker(packet,emit):
            self.assertEqual(packet["turn"],1)
            active.set()
            messages = self.wait(emit.pending_notes)
            seen.extend(messages)
            seqs = [n["seq"] for n in messages]
            emit.notes_accepted(seqs)
            emit.notes_accepted(seqs)
            self.assertEqual(emit.pending_notes(),[])
            return "steered"
        self.start(worker)
        self.assertTrue(active.wait(3))
        body = "带空格  \r\n🧭\t尾 "
        seq = self.store.note_user(ticket,root,body)
        self.wait(lambda:self.jobs(ticket)[0]["state"]=="done")
        self.assertEqual(seen,[dict(seq=seq,sender="user",body=body)])
        self.assertEqual(len(self.store.rows("SELECT seq FROM events WHERE kind='note.accepted'")),1)

    def test_waiting_router_and_running_child_exchange(self):
        ticket = self.store.create("转接", self.path, "deepseek")
        root = self.jobs(ticket)[0]["id"]
        release = threading.Event()
        body = '转给你\r\n"逐字" 🙂  '
        packets = []
        def worker(p, emit):
            packets.append(p)
            if p["agent"] == "claude":
                if p["turn"] == 1:
                    self.assertTrue(release.wait(4))
                    return "initial"
                return "confirmed " + p["notes"][0]["body"]
            if p["turn"] == 1:
                return 'SA_SEND {"id":"c","to":"claude","body":"等补充"}'
            if p["turn"] == 2:
                dest = next(r["job"] for r in p["roster"] if r["agent"]=="claude")
                msg = "SA_NOTE "+json.dumps(dict(id="exact",job=dest,body=body),ensure_ascii=False)
                emit("delta",msg[:3]); emit("delta",msg[3:]+"\n")
                release.set()
                return msg
            return p["reports"][0]["result"]
        self.start(worker)
        self.wait(lambda:len(self.jobs(ticket))==2 and
                  [r["state"] for r in self.jobs(ticket)]==["waiting","running"])
        self.store.note_user(ticket,root,"向正在运行的任务补充要求")
        self.wait(lambda:self.jobs(ticket)[0]["state"]=="done")
        router,worker_job = self.jobs(ticket)
        self.assertEqual(router["result"],"confirmed "+body)
        self.assertEqual(worker_job["turn"],2)
        delivered = next(p for p in packets if p["job"]==worker_job["id"] and p["turn"]==2)["notes"]
        self.assertEqual(len(delivered),1)
        self.assertEqual((delivered[0]["body"],delivered[0]["sha256"]),(body,digest(body)))
        self.assertEqual(len(self.store.rows("SELECT seq FROM mailbox WHERE source IS NOT NULL")),1)

    def test_refuse_stale_cross_ticket_and_conflicting_replay(self):
        ticket = self.store.create("一", self.path, "codex")
        source = self.jobs(ticket)[0]
        other = self.store.create("二", self.path, "claude")
        foreign = self.jobs(other)[0]["id"]
        with self.store.tx() as db:
            db.execute("UPDATE jobs SET state='running' WHERE id=?",(source["id"],))
        local = self.store.dispatch(source,dict(id="kid",to="claude",body="x"))
        with self.store.tx() as db:
            db.execute("UPDATE jobs SET state='done' WHERE id=?",(local,))
        for jid in (foreign,local):
            with self.assertRaises(ValueError):
                self.store.note_user(ticket,jid,"x")
            msg = dict(id=str(jid),job=jid,body="persists exactly")
            seq = self.store.note_agent(source,msg)
            self.assertEqual(self.store.note_agent(source,msg),seq)
            with self.assertRaises(ValueError):
                self.store.note_agent(source,{**msg,"body":"changed"})
        self.assertEqual(len(self.store.rows("SELECT * FROM mailbox WHERE target IS NULL")),2)
        self.assertFalse(self.store.rows("SELECT * FROM mailbox WHERE target IS NOT NULL"))
        self.assertFalse(self.store.rows("SELECT seq FROM events WHERE ticket=? AND kind LIKE 'note%'",(other,)))

    def test_queued_and_steered_mail_replayed_after_interruption(self):
        ticket = self.store.create("重启",self.path,"codex")
        root = self.jobs(ticket)[0]
        seq1 = self.store.note_user(ticket,root["id"],"early")
        with self.store.tx() as db:
            db.execute("UPDATE jobs SET state='running' WHERE id=?",(root["id"],))
        seq2 = self.store.note_user(ticket,root["id"],"later")
        self.store.accept_notes(root,[seq2])
        self.store.db.close()
        self.store = Store(self.path / "state")
        self.store.retry(root["id"])
        seen = []
        def worker(p,emit):
            seen.extend((n["seq"],n["body"]) for n in p["notes"])
            self.assertFalse(emit.pending_notes())
            return "recovered"
        self.start(worker)
        self.wait(lambda:self.jobs(ticket)[0]["state"]=="done")
        self.assertEqual(seen,[(seq1,"early"),(seq2,"later")])

    def test_claude_route_migration_concurrent_open(self):
        directory = self.path / "legacy"
        directory.mkdir()
        with sqlite3.connect(directory / "hub.sqlite3") as db:
            db.execute("""CREATE TABLE tickets(id INTEGER PRIMARY KEY, goal TEXT NOT NULL, cwd TEXT NOT NULL,
                          writes INTEGER NOT NULL, lead TEXT NOT NULL, created REAL NOT NULL)""")
            db.execute("INSERT INTO tickets VALUES(1,'old','x',0,'claude',0)")
        db.close()
        failures = []
        gate = threading.Barrier(4)
        def open_store():
            try:
                gate.wait()
                s = Store(directory,recover=False)
                try:
                    self.assertEqual(s.rows("SELECT claude_route FROM tickets")[0]["claude_route"],"user")
                finally:
                    s.db.close()
            except Exception as exc:
                failures.append(exc)
        threads = [threading.Thread(target=open_store) for _ in range(4)]
        for th in threads:
            th.start()
        for th in threads:
            th.join(8)
            self.assertFalse(th.is_alive())
        self.assertEqual(failures,[])

    def test_ticket_route_reaches_claude_and_writer_capability(self):
        with self.assertRaises(ValueError):
            self.store.create("x",self.path,"claude",claude_route="typo")
        ticket = self.store.create("route smoke",self.path,"claude",True)
        root = self.jobs(ticket)[0]
        calls = []
        def claude(prompt,cwd,emit,cancel,**opts):
            packet=json.loads(prompt.rsplit("\n",1)[1])
            calls.append((opts["route"],packet["claude_route"]))
            for agent in ("claude","antigravity"):
                with self.assertRaises(ValueError):
                    self.store.dispatch(root,dict(id=agent,to=agent,body="forbidden",write=True))
            return "done",None
        self.engine = Engine(self.store,{"claude":SimpleNamespace(run=claude)})
        self.engine.start()
        self.wait(lambda:self.jobs(ticket)[0]["state"]=="done")
        self.assertEqual(calls,[("cc-switch","cc-switch")])

    def test_auto_note_budget_dedup_and_manual_exemption(self):
        ticket = self.store.create("budget",self.path,"codex")
        source = self.jobs(ticket)[0]
        with self.store.tx() as db:
            db.execute("UPDATE jobs SET state='running' WHERE id=?",(source["id"],))
        dest = self.store.dispatch(source,dict(id="child",to="deepseek",body="wait"))
        msg = dict(job=dest,body="原文")
        for n in range(MAX_AGENT_NOTES):
            last = self.store.note_agent(source,{**msg,"id":str(n)})
        self.assertEqual(self.store.note_agent(source,{**msg,"id":str(MAX_AGENT_NOTES-1)}),last)
        refused = self.store.note_agent(source,{**msg,"id":"over"})
        self.assertEqual(refused,self.store.note_agent(source,{**msg,"id":"over"}))
        self.assertIsNone(self.store.rows("SELECT target FROM mailbox WHERE seq=?",(refused,))[0]["target"])
        row = self.store.rows("SELECT kind,payload FROM events WHERE seq=?",(refused,))[0]
        self.assertEqual(row["kind"],"note.rejected")
        self.assertIn(str(MAX_AGENT_NOTES),json.loads(row["payload"])["reason"])
        self.store.note_user(ticket,dest,"来自操作者")
        self.assertEqual(len(self.store.rows("SELECT seq FROM mailbox WHERE target=?",(dest,))),MAX_AGENT_NOTES+1)
