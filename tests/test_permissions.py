"""Exercise profile propagation, operator decisions and autonomous project lifecycle."""
import json
from pathlib import Path
import tempfile
import time
import unittest
from types import SimpleNamespace
from threading import Event

from engine import Engine, Store


def checked_exhaustion(emit):
    emit("native",{"type":"tool_result","callId":"inspect","status":"ok","result":"fixture verified"})
    return 'SA_EXHAUSTED {"status":"done","summary":"fixture complete","checks":["checked fixture"],"alternatives":["no other authorized work"]}'


class PermissionsTests(unittest.TestCase):
    def setUp(self):
        tmp=tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.base=Path(tmp.name)
        self.s=Store(self.base/"state")
        self.addCleanup(lambda:self.s.db.close())

    def run_workers(self,fn):
        def runner(prompt,cwd,emit,cancel,**opts):
            packet=json.loads(prompt.rsplit("\n",1)[1])
            return fn(packet,emit,cancel,opts),None
        self.e=Engine(self.s,{a:SimpleNamespace(run=runner) for a in ("claude","codex","deepseek","antigravity")})
        self.addCleanup(self.e.close)
        self.e.start()

    def wait(self,check):
        deadline=time.monotonic()+6
        while time.monotonic()<deadline:
            value=check()
            if value:
                return value
            time.sleep(.02)
        self.fail("timeout: "+str(self.s.rows("SELECT id,state,result FROM jobs")))

    def root(self,t):
        return self.s.rows("SELECT * FROM jobs WHERE parent IS NULL AND ticket=?",(t,))[0]

    def test_goal_continues_delegates_and_finishes(self):
        t=self.s.create("project",self.base,"claude",permission_profile="auto")
        seen=[]
        def worker(p,emit,cancel,opts):
            seen.append((p["agent"],opts["permission_profile"],opts["writable"],p["objective"]))
            if p["depth"]>0:
                return "verified"
            if p["turn"]==1:
                return 'SA_GOAL {"objective":"validate"}\nSA_NEXT {"body":"run check"}'
            if p["turn"]==2:
                self.assertEqual(p["next_step"]["body"],"run check")
                return 'SA_SEND {"id":"review","to":"antigravity","body":"check","write":false}'
            if p["autonomy"]["phase"]=="discover":
                return checked_exhaustion(emit)
            self.assertEqual(p["reports"][0]["result"],"verified")
            return 'SA_FINISH {"status":"done","summary":"verified twice"}'
        self.run_workers(worker)
        self.wait(lambda:self.root(t)["state"]=="done")
        self.assertEqual(len(seen),5)
        self.assertTrue(all(x[1:3]==("auto",True) for x in seen))
        self.assertEqual(seen[-1][-1],"validate")
        self.assertEqual(len(self.s.rows("SELECT seq FROM events WHERE kind='auto.exhausted'")),1)

    def test_allow_deny_cancel_and_expiration(self):
        decisions=[]
        def worker(p,emit,cancel,opts):
            verdict=emit.approve({"title":"temporary file","action":"edit"})
            decisions.append(verdict)
            return "allowed" if verdict else "denied"
        self.run_workers(worker)
        for method in ("allow","deny","cancel"):
            t=self.s.create("edit",self.base,"codex",permission_profile="ask")
            row=self.wait(lambda:self.s.rows("SELECT * FROM approvals WHERE ticket=? AND decision='pending'",(t,)))[0]
            if method=="cancel":
                self.e.cancel(t)
            else:
                self.s.decide(row["id"],method=="allow")
            self.wait(lambda:self.root(t)["state"] in ("done","cancelled"))
            with self.assertRaises(ValueError):
                self.s.decide(row["id"],True)
        self.wait(lambda:len(decisions)==3)
        self.assertEqual(decisions,[True,False,False])

    def test_updated_goal_supersedes_inflight_finish(self):
        started,release=Event(),Event()
        def worker(p,emit,cancel,opts):
            if p["turn"]==1:
                started.set()
                release.wait(3)
                return 'SA_GOAL {"objective":"stale"}\nSA_FINISH {"status":"done","summary":"stale"}'
            self.assertIn("new requirement",p["user_updates"])
            if p["autonomy"]["phase"]=="discover":
                return checked_exhaustion(emit)
            return 'SA_GOAL {"objective":"revised"}\nSA_FINISH {"status":"done","summary":"new work verified"}'
        t=self.s.create("first",self.base,"codex",permission_profile="auto")
        self.run_workers(worker)
        self.assertTrue(started.wait(3))
        self.s.follow(t,"new requirement")
        release.set()
        self.wait(lambda:self.root(t)["state"]=="done")
        self.assertEqual(self.s.rows("SELECT objective FROM tickets WHERE id=?",(t,))[0]["objective"],"revised")
        self.assertEqual(self.root(t)["turn"],3)

    def test_recovery_never_reuses_approval(self):
        for mode in ("ask","auto"):
            self.s.create("start",self.base,"codex",permission_profile=mode)
        with self.s.tx() as db:
            db.execute("UPDATE jobs SET state='running'")
            db.execute("INSERT INTO approvals(ticket,job,turn,action) VALUES(1,1,1,'{}')")
        self.s.db.close()
        self.s=Store(self.base/"state")
        self.assertEqual(self.root(1)["state"],"interrupted")
        self.assertEqual((self.root(2)["state"],self.root(2)["turn"]),("queued",2))
        with self.assertRaises(ValueError):
            self.s.decide(1,True)
        self.assertEqual(self.s.rows("SELECT decision FROM approvals")[0]["decision"],"expired")
        with self.assertRaises(ValueError):
            self.s.create("invalid",self.base,"codex",permission_profile="typo")

    def test_failure_requires_reconciliation_before_autonomous_completion(self):
        t=self.s.create("repair",self.base,"deepseek",permission_profile="auto")
        def worker(p,emit,cancel,opts):
            if p["depth"]:
                raise RuntimeError("fixture failure")
            if p["autonomy"]["phase"]=="discover":
                return checked_exhaustion(emit)
            if p["turn"]==1:
                return 'SA_SEND {"id":"one","to":"codex","body":"inspect"}'
            if p["turn"]==2:
                return 'SA_FINISH {"status":"done","summary":"not sufficient"}'
            failed=p["reports"][0]["id"]
            self.assertIn(str(failed),p["next_step"]["body"])
            return "SA_FINISH "+json.dumps(dict(status="done",summary="independent check",
                       resolved=[dict(job=failed,evidence="checked output against test fixture")]))
        self.run_workers(worker)
        self.wait(lambda:self.root(t)["state"]=="done")
        self.assertEqual(self.root(t)["turn"],4)
        results=self.s.rows("SELECT payload FROM events WHERE kind='auto.milestone'")
        self.assertEqual(len(results),1)
        self.assertTrue(json.loads(results[0]["payload"])["resolved"])
