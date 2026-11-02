"""Continuous project work using real SQLite and controlled local workers."""
import json
from pathlib import Path
import tempfile
import threading
import time
import unittest
from types import SimpleNamespace

from engine import AGENTS, Engine, Store


def control(tag, **payload):
    return tag+" "+json.dumps(payload,ensure_ascii=False)


class ContinuousAutoTest(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path=Path(self.tmp.name)
        self.project=self.path/"project"
        self.project.mkdir()
        self.state=self.path/"state"
        self.store=Store(self.state)
        self.engine=None
        self.addCleanup(lambda:self.store.db.close())
        self.addCleanup(lambda:self.engine.close() if self.engine else None)
        self.calls=[]

    def configure(self,fn):
        def run(prompt,cwd,emit,cancel,**opts):
            packet=json.loads(prompt.rsplit("\n",1)[1])
            self.calls.append(packet)
            return fn(packet,Path(cwd),emit,cancel),None
        self.engine=Engine(self.store,{a:SimpleNamespace(run=run) for a in AGENTS},timeout=10)

    def advance(self,check):
        deadline=time.monotonic()+8
        while time.monotonic()<deadline:
            if check():
                return
            self.engine.step()
            if check():
                return
            time.sleep(.005)
        self.fail(str(self.store.rows("SELECT id,turn,state,result FROM jobs")))

    def create(self,goal):
        return self.store.create(goal,self.project,"deepseek",permission_profile="auto")

    def root(self,tid):
        return self.store.rows("SELECT * FROM jobs WHERE ticket=? AND parent IS NULL",(tid,))[0]

    def test_completed_milestone_requires_evidenced_discovery_and_subsequent_exhaustion(self):
        lesson="保留原始换行，诊断与产出分开。"
        unexpected="request-B: 文本末尾两个空格  \n"
        (self.project/"open-request.txt").write_text(unexpected,encoding="utf-8")
        attempts=[]
        discovery_packets=[]

        def evidence(emit,ident,result):
            emit("native",{"type":"tool_result","callId":ident,"status":"ok","result":result})

        def work(p,project,emit,cancel):
            if p["depth"]:
                data=(project/"open-request.txt").read_bytes()
                (project/"resolved.bin").write_bytes(data)
                evidence(emit,"write",f"resolved.bin matches source: {len(data)} bytes")
                return "resolved.bin was written and verified"
            auto=p["autonomy"]
            if auto["phase"]=="work":
                if not (project/"first.out").exists():
                    (project/"first.out").write_bytes(b"first\n")
                    evidence(emit,"check-A","first.out contains the requested marker")
                    return (control("SA_LEARN",lesson=lesson)+"\n"+
                            control("SA_FINISH",status="done",summary="First artifact independently read: first.out"))
                if not p["reports"]:
                    return control("SA_SEND",to="codex",id="write-B",body="Resolve the new file request.",write=True)
                self.assertIn("verified",p["reports"][-1]["result"])
                self.assertEqual((project/"resolved.bin").read_bytes(),(project/"open-request.txt").read_bytes())
                evidence(emit,"check-B","resolved.bin byte-for-byte matches open-request.txt")
                return control("SA_FINISH",status="done",summary="Verified second deliverable against open-request.txt.")
            self.assertEqual(auto["phase"],"discover")
            discovery_packets.append(p)
            self.assertIn(lesson,auto["learning"])
            if not (project/"resolved.bin").exists():
                text=(project/"open-request.txt").read_text(encoding="utf-8")
                if "read-backlog" not in attempts:
                    attempts.append("read-backlog")
                    # An unsupported guess is not authorization for a new task.
                    return (control("SA_GOAL",objective="Invent a new requirement",evidence="No inspection performed.")+
                            "\n"+control("SA_NEXT",body="Should not enter the work phase"))
                evidence(emit,"read-backlog",text)
                if "cite-backlog" not in attempts:
                    attempts.append("cite-backlog")
                    return control("SA_GOAL",objective="Uncited new objective")+"\n"+control("SA_NEXT",body="Do not launch")
                return (control("SA_GOAL",objective="Resolve request B",evidence="Read open-request.txt: "+text)+
                        "\n"+control("SA_NEXT",body="Delegate the declared file request and verify bytes"))
            stated=control("SA_EXHAUSTED",status="done",summary="Project fixture is complete.",
                           checks=["Checked first.out against its marker","Compared resolved.bin and the only open request"],
                           alternatives=["Repeating the write adds no value","No second outstanding request in the project"])
            if "unchecked-finish" not in attempts:
                attempts.append("unchecked-finish")
                # Previous work phase had a genuine tool result, but this
                # discovery period has none yet. The old receipt cannot close it.
                return stated
            evidence(emit,"final-audit",str(sorted(f.name for f in project.iterdir()))+
                     "; first.out and resolved.bin both match source.")
            if "incomplete-audit" not in attempts:
                attempts.append("incomplete-audit")
                return control("SA_EXHAUSTED",status="done",summary="Missing alternatives",checks=["Listed files"])
            return stated

        self.configure(work)
        tid=self.create("Deliver first.out, then continue real project work")
        self.advance(lambda:self.store.rows("SELECT seq FROM events WHERE ticket=? AND kind='auto.milestone'",(tid,)))
        self.assertNotIn(self.root(tid)["state"],("done","failed","cancelled"))
        self.engine.close()
        self.store.db.close()
        self.store=Store(self.state)
        self.configure(work)
        self.advance(lambda:self.root(tid)["state"]=="done")
        self.assertEqual((self.project/"resolved.bin").read_bytes(),(self.project/"open-request.txt").read_bytes())
        self.assertEqual(len(self.store.rows("SELECT seq FROM events WHERE ticket=? AND kind='auto.milestone'",(tid,))),2)
        self.assertEqual(len(self.store.rows("SELECT id FROM jobs WHERE ticket=? AND parent IS NOT NULL",(tid,))),1)
        self.assertGreaterEqual(len(discovery_packets),6)
        self.assertTrue(any(p["autonomy"]["recent"] for p in discovery_packets))
        self.assertFalse(any(p.get("objective")=="Invent a new requirement" for p in self.calls))
        self.assertFalse(any(p.get("objective")=="Uncited new objective" for p in self.calls))
        self.assertTrue(any(p.get("objective")=="Resolve request B" for p in self.calls))

    def test_blocked_milestone_rechecks_real_alternatives_before_stopping(self):
        checks=[]
        def worker(p,project,emit,cancel):
            if p["autonomy"]["phase"]=="work":
                return control("SA_FINISH",status="blocked",summary="The initial input is missing.")
            candidate=project/("missing-source.txt" if not checks else "missing-backup.txt")
            try:
                candidate.read_bytes()
            except FileNotFoundError:
                checks.append(f"{candidate.name}: not found")
            emit("native",{"type":"tool_result","callId":f"attempt-{len(checks)}","status":"error","result":checks[-1]})
            # Failed tool attempts are relevant to an external blocker. They
            # cannot substantiate a claim that all project work is complete.
            return control("SA_EXHAUSTED",status="done" if len(checks)==1 else "blocked",
                           summary="Checked supplied input and alternate local source.",
                           checks=list(checks),alternatives=["No alternate input exists; fabricating the source is invalid."])
        self.configure(worker)
        tid=self.create("Verify the external input or find a documented local copy")
        self.advance(lambda:self.root(tid)["state"]=="failed")
        self.assertEqual(checks,["missing-source.txt: not found","missing-backup.txt: not found"])
        reports=self.store.rows("SELECT payload FROM events WHERE ticket=? AND kind='auto.exhausted'",(tid,))
        self.assertEqual(len(reports),1)
        self.assertEqual(json.loads(reports[0]["payload"])["status"],"blocked")

    def test_progress_beyond_sixty_four_turns_until_operator_stops(self):
        last=threading.Event()
        def worker(p,project,emit,cancel):
            if p["depth"]:
                return "reviewed "+p["assignment"]
            n=p["turn"]
            if n==67:
                last.set()
                if not cancel.wait(4):
                    raise RuntimeError("operator stop did not cancel the in-flight worker")
                emit("delta",control("SA_SEND",id="late",to="deepseek",body="ignore")+"\n")
                return control("SA_NEXT",body="must not requeue after stop")
            with (project/"batch.log").open("a",encoding="utf-8") as log:
                log.write(f"batch {n}\n")
            emit("native",{"type":"tool_result","callId":f"batch-{n}","status":"ok","result":f"Wrote batch {n}."})
            return (control("SA_SEND",id=f"review-{n}",to="claude",body=f"Review batch {n}",write=False)+"\n"+
                    control("SA_NEXT",body=f"Validated batch {n}. Continue with batch {n+1}."))
        self.configure(worker)
        tid=self.create("Validate all pending batches until I stop the job")
        self.advance(last.is_set)
        self.engine.cancel(tid)
        self.advance(lambda:not self.engine.active)
        self.assertEqual(self.root(tid)["state"],"cancelled")
        self.assertEqual((self.project/"batch.log").read_text(encoding="utf-8").splitlines(),
                         [f"batch {n}" for n in range(1,67)])
        sends=self.store.rows("SELECT payload FROM events WHERE ticket=? AND kind='send' ORDER BY seq",(tid,))
        self.assertEqual([json.loads(r["payload"])["body"] for r in sends],[f"Review batch {n}" for n in range(1,67)])
        self.engine.close()
        self.store.db.close()
        self.store=Store(self.state)
        self.configure(worker)
        self.engine.step()
        self.assertFalse(self.engine.active)
        self.assertEqual(self.root(tid)["state"],"cancelled")
        with self.assertRaises(ValueError):
            self.store.follow(tid,"This isn't explicit authorization to resume")
        self.assertEqual(self.root(tid)["state"],"cancelled")
        self.assertEqual(len([p for p in self.calls if not p["depth"]]),67)
        self.assertEqual(len([p for p in self.calls if p["depth"]]),66)
