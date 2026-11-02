"""Deterministic integration tests using real SQLite and controlled native workers."""
import hashlib
import json
from pathlib import Path
import tempfile
import threading
import time
import unittest
from types import SimpleNamespace

from engine import AGENTS, Engine, PREFIX, Store


def dispatch_line(agent, key, body, writable=False):
    cmd = dict(to=agent, id=key, body=body, write=writable)
    return PREFIX + json.dumps(cmd, ensure_ascii=False) + "\n"


class EngineTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.cwd = Path(self.tmp.name)
        self.store = Store(self.cwd / "state")
        self.engine = None
        self.gates = []

    def tearDown(self):
        for gate in self.gates:
            gate.set()
        if self.engine:
            self.engine.close()
        self.store.db.close()
        self.tmp.cleanup()

    def gate(self):
        event = threading.Event()
        self.gates.append(event)
        return event

    def start(self, worker, *, capacity=4):
        def run(prompt, cwd, emit, cancel, *, session_id=None, writable=False, route=None):
            envelope = json.loads(prompt.rsplit("\n", 1)[-1])
            return worker(envelope, emit, cancel, session_id, writable)
        self.engine = Engine(self.store, {a: SimpleNamespace(run=run) for a in AGENTS},
                             timeout=20, capacity=capacity)
        self.engine.start()

    def jobs(self, ticket):
        return self.store.rows("SELECT * FROM jobs WHERE ticket=? ORDER BY id", (ticket,))

    def wait(self, predicate):
        deadline = time.monotonic() + 7
        while time.monotonic() < deadline:
            result = predicate()
            if result:
                return result
            time.sleep(.015)
        self.fail("scheduler timeout: " + str(self.store.rows("SELECT id,state,result FROM jobs")))

    def test_routing_stream_dedup_export_and_follow(self):
        release, entered = self.gate(), self.gate()
        body = '一\r\n\t"\\两" 😀 e\u0301\u2028中间\u0085\n尾部空格  '
        report = "回答\r\n下一行\t🙂  "
        note = "追加指令\n保留末尾空格 "
        line = dispatch_line("claude", "one", body)
        packets = []

        def worker(packet, emit, cancel, session, writable):
            packets.append((packet, session))
            if packet["agent"] == "claude":
                entered.set()
                return report, "claude-session"
            if packet["turn"] == 1:
                for chunk in (line[:4], line[4:13], line[13:]):
                    emit("delta", chunk)
                release.wait(6)
                return line, "lead-session"
            return "归并完成", "lead-session"

        ticket = self.store.create("启动", self.cwd, "codex")
        self.start(worker)
        self.assertTrue(entered.wait(5), "子任务应在父任务最终返回之前开始")
        self.assertEqual(self.jobs(ticket)[0]["state"], "running")
        self.store.follow(ticket, note)
        release.set()
        self.wait(lambda: self.jobs(ticket)[0]["state"] == "done")
        root, child = self.jobs(ticket)
        self.assertEqual((root["result"], child["body"], child["result"]), ("归并完成", body, report))
        merged = [(p,s) for p,s in packets if p["agent"] == "codex" and p["turn"] > 1]
        self.assertEqual(len(merged), 1)
        packet, session = merged[0]
        self.assertEqual((session, packet["user_updates"], packet["reports"][0]["result"]),
                         ("lead-session", [note], report))
        sent = self.store.rows("SELECT payload FROM events WHERE kind='send'")
        self.assertEqual(len(sent), 1)
        self.assertEqual(json.loads(sent[0]["payload"])["sha256"],
                         hashlib.sha256(body.encode()).hexdigest())
        path = self.cwd / "events.jsonl"
        self.store.export(ticket, path)
        with path.open(encoding="utf-8") as exported:
            records = [json.loads(line) for line in exported]
        seq = [r["seq"] for r in records]
        self.assertEqual(seq, sorted(set(seq)))
        for record in records:
            self.assertEqual(hashlib.sha256(record["payload"].encode()).hexdigest(), record["sha256"])

    def test_interrupted_on_reopen_and_retry_deduplicates_committed_child(self):
        ticket = self.store.create("断点", self.cwd, "codex")
        root = self.jobs(ticket)[0]
        cmd = dict(id="original", to="deepseek", body="核对已提交的消息", write=False)
        with self.store.tx() as db:
            db.execute("UPDATE jobs SET state='running' WHERE id=?", (root["id"],))
        self.store.dispatch(root, cmd)
        # Crash after persisting dispatch, before the parent returns its final reply.
        self.store.db.close()
        self.store = Store(self.cwd / "state")
        self.assertEqual(self.jobs(ticket)[0]["state"], "interrupted")
        self.store.retry(root["id"])
        received = []
        def worker(p, emit, cancel, session, writable):
            if p["agent"] == "deepseek":
                received.append(p["assignment"])
                return "已核对", None
            if p["turn"] == 1:
                return PREFIX + json.dumps(cmd, ensure_ascii=False), None
            return "已汇总", None
        self.start(worker)
        self.wait(lambda: self.jobs(ticket)[0]["state"] == "done")
        self.assertEqual(len(self.jobs(ticket)), 2)
        self.assertEqual(received, ["核对已提交的消息"])

    def test_cancel_stops_work_and_prevents_late_dispatch(self):
        started, stopped = self.gate(), self.gate()
        ticket = self.store.create("取消请求", self.cwd, "codex")
        def worker(packet, emit, cancel, session, writable):
            started.set()
            if not cancel.wait(5):
                raise RuntimeError("未收到取消")
            emit("delta", dispatch_line("claude", "late", "过期消息"))
            stopped.set()
            return "late reply", None
        self.start(worker)
        self.assertTrue(started.wait(5))
        self.engine.cancel(ticket)
        self.assertTrue(stopped.wait(5))
        self.wait(lambda: not self.engine.active)
        jobs = self.jobs(ticket)
        self.assertEqual([j["state"] for j in jobs], ["cancelled"])

    def test_retry_interrupted_writer_under_draining_root(self):
        ticket = self.store.create("恢复父任务的未结写入", self.cwd, "codex", writes=True)
        root = self.jobs(ticket)[0]
        with self.store.tx() as db:
            db.execute("UPDATE jobs SET state='running' WHERE id=?", (root["id"],))
        child = self.store.dispatch(root, dict(to="deepseek", id="writer", body="写入", write=True))
        with self.store.tx() as db:
            db.execute("UPDATE jobs SET state='draining', result='transport failed' WHERE id=?", (root["id"],))
            db.execute("UPDATE jobs SET state='running' WHERE id=?", (child,))
        self.store.db.close()
        self.store = Store(self.cwd / "state")
        self.assertEqual([j["state"] for j in self.jobs(ticket)], ["draining", "interrupted"])
        with self.assertRaises(ValueError):
            self.store.follow(ticket, "此时上级无法处理")
        self.assertFalse(self.store.rows("SELECT seq FROM events WHERE kind='user.follow'"))
        self.store.retry(child)
        self.assertEqual([j["state"] for j in self.jobs(ticket)], ["draining", "queued"])
        self.start(lambda *args: ("已核对并写入", None))
        self.wait(lambda: self.jobs(ticket)[0]["state"] == "failed")
        self.assertEqual(self.jobs(ticket)[1]["state"], "done")

    def test_blank_project_is_rejected(self):
        with self.assertRaises(ValueError):
            self.store.create("写入项目", "", "codex", writes=True)

    def test_primary_may_work_alone_and_select_its_role(self):
        calls = []
        def worker(packet, emit, cancel, session, writable):
            calls.append((packet["agent"],writable))
            self.assertEqual(packet["role"], "")
            self.assertTrue(packet["can_assign_write"])
            self.assertEqual(packet["this_turn_can_write"], writable)
            self.assertEqual(packet["harness_capabilities"][packet["agent"]]["write_project"], writable)
            return 'SA_ROLE {"role":"接口设计"}\n已完成', None
        tickets = [self.store.create("独立工作",self.cwd,a,writes=True) for a in AGENTS]
        self.start(worker)
        self.wait(lambda:all(self.jobs(t)[0]["state"] == "done" for t in tickets))
        self.assertEqual(sorted(calls),sorted([("codex",True),("deepseek",True),
                                              ("claude",False),("antigravity",False)]))
        for t in tickets:
            primary, = self.jobs(t)
            self.assertEqual(primary["role"],"接口设计")

    def test_primary_writer_delegates_reader_immediately_and_writer_after_yield(self):
        ticket = self.store.create("按需协作",self.cwd,"codex",writes=True)
        reader_active, primary_yield = self.gate(), self.gate()
        calls = []
        line = PREFIX + json.dumps(dict(id="design",to="claude",role="接口设计",body="提出协议"),ensure_ascii=False)
        other = dispatch_line("deepseek","edit","实现协议",True)
        def worker(p, emit, cancel, session, writable):
            calls.append((p["agent"],p["turn"],p["role"],writable))
            if p["agent"] == "codex" and p["turn"] == 1:
                emit("delta",line+"\n"+other)
                self.assertTrue(reader_active.wait(4))
                primary_yield.wait(4)
                return line+"\n"+other, "primary"
            if p["agent"] == "claude":
                emit("delta",'SA_ROLE {"role":"协议设计"}\n')
                reader_active.set()
                return 'SA_ROLE {"role":"协议设计"}\n设计完成',None
            if p["agent"] == "codex":
                self.assertEqual([r["role"] for r in p["reports"]],["协议设计",""])
            return "ok",None
        self.start(worker)
        self.assertTrue(reader_active.wait(4))
        self.assertEqual(self.jobs(ticket)[2]["state"],"queued")
        self.assertNotIn(("deepseek",1,"",True),calls)
        # The stream and the final reply replay the same dispatch. Its role
        # still deduplicates even after the recipient revised its own role.
        primary_yield.set()
        self.wait(lambda:self.jobs(ticket)[0]["state"]=="done")
        self.assertEqual(len(self.jobs(ticket)),3)
        self.assertEqual(self.jobs(ticket)[1]["assigned_role"],"接口设计")
        self.assertCountEqual(calls,[("codex",1,"",True),("claude",1,"接口设计",False),
                                     ("deepseek",1,"",True),("codex",2,"",True)])

    def test_partial_parent_reopens_after_failed_child_retry(self):
        ticket = self.store.create("验证失败回报", self.cwd, "codex")
        attempts = []
        merged = []
        def worker(packet, emit, cancel, session, writable):
            if packet["agent"] == "claude":
                attempts.append(packet["job"])
                if len(attempts) == 1:
                    raise RuntimeError("upstream unavailable")
                return "recovered", "test-session"
            if packet["turn"] == 1:
                return dispatch_line("claude", "test", "检查"), "router"
            merged.append((session, packet["reports"][0]["state"], packet["reports"][0]["result"]))
            return "收到", "router"
        self.start(worker)
        self.wait(lambda: self.jobs(ticket)[0]["state"] == "partial")
        child_id = self.jobs(ticket)[1]["id"]
        self.store.retry(child_id)
        self.wait(lambda: self.jobs(ticket)[0]["state"] == "done")
        self.assertEqual(attempts, [child_id, child_id])
        self.assertEqual(merged, [("router", "failed", "upstream unavailable"),
                                  ("router", "done", "recovered")])

    def test_writers_serialize_across_tickets(self):
        first_started, release = self.gate(), self.gate()
        overlap, writers = [], []
        lock = threading.Lock()
        def worker(packet, emit, cancel, session, writable):
            if not writable:
                if packet["turn"] == 1:
                    return dispatch_line("codex", "edit", "写一个文件", True), None
                return "done", None
            with lock:
                writers.append(packet["ticket"])
                if len(writers) > 1 and not release.is_set():
                    overlap.append(packet["ticket"])
            if len(writers) == 1:
                first_started.set()
                release.wait(6)
            return "ok", None
        a, b = [self.store.create(name, self.cwd, "claude", writes=True) for name in ("A", "B")]
        self.start(worker)
        self.assertTrue(first_started.wait(5))
        self.wait(lambda: len(self.jobs(a)) == len(self.jobs(b)) == 2)
        # Drive extra scheduler passes while the first real worker is still blocked.
        for i in range(5):
            self.engine.step()
        self.assertEqual(len(writers), 1)
        release.set()
        self.wait(lambda: all(self.jobs(t)[0]["state"] == "done" for t in (a,b)))
        self.assertEqual(set(writers), {a,b})
        self.assertFalse(overlap)

    def test_parent_failure_does_not_hide_running_descendant(self):
        child_entered, fail, release = self.gate(), self.gate(), self.gate()
        ticket = self.store.create("处理中断", self.cwd, "antigravity", writes=True)
        def worker(packet, emit, cancel, session, writable):
            if packet["depth"] == 0 and packet["turn"] == 1:
                emit("delta", dispatch_line("deepseek", "edit", "正在写文件", True))
                fail.wait(6)
                raise RuntimeError("transport lost")
            if packet["agent"] == "deepseek":
                child_entered.set()
                while not release.wait(.02) and not cancel.is_set():
                    pass
                return "已写入", None
            return "核对已结束", None
        self.start(worker)
        self.assertTrue(child_entered.wait(5))
        fail.set()
        root_id = self.jobs(ticket)[0]["id"]
        self.wait(lambda: root_id not in self.engine.active)
        root, child = self.jobs(ticket)
        terminal = ("failed", "done", "cancelled")
        self.assertFalse(root["state"] in terminal and child["state"] not in terminal,
                         "父任务已显示终态，但仍有非终态子任务")
        release.set()
        self.wait(lambda: self.jobs(ticket)[0]["state"] in terminal)
