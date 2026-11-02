"""Cross-connection lifecycle races with controlled adapters and a private WAL DB."""
import json
from pathlib import Path
import sqlite3
import tempfile
import threading
import unittest
from types import SimpleNamespace

from engine import Engine, Store


FINISH = 'SA_FINISH {"status":"done","summary":"verified old objective"}'


class ContinuousConcurrency(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.project = Path(tmp.name)
        self.primary = Store(self.project/"state")
        self.addCleanup(self.primary.db.close)
        self.operator = Store(self.project/"state",recover=False)
        self.addCleanup(self.operator.db.close)
        self.ticket = self.primary.create("old objective",self.project,"codex",
                                          permission_profile="auto",allow_lead_routing=False)
        self.packets = []
        self.engine = None

    def attach(self, fn):
        def run(prompt,cwd,emit,cancel,**options):
            self.packets.append(json.loads(prompt.rsplit("\n",1)[1]))
            return fn(),None
        self.engine = Engine(self.primary,{"codex":SimpleNamespace(run=run)},timeout=30)
        self.addCleanup(self.engine.close)

    def quiesce(self):
        # Only the test calls step(). No next turn can start between assertions.
        with self.engine.guard:
            workers = [entry[0] for entry in self.engine.active.values()]
        for worker in workers:
            worker.join(5)
            self.assertFalse(worker.is_alive(),"the controlled turn should have returned")

    def job(self):
        return self.operator.rows("SELECT * FROM jobs WHERE ticket=? AND parent IS NULL",(self.ticket,))[0]

    def events(self, kind):
        return self.operator.rows("SELECT * FROM events WHERE ticket=? AND kind=? ORDER BY seq",
                                  (self.ticket,kind))

    def finalized_first(self, action):
        deciding, release = threading.Event(), threading.Event()
        def trace(statement):
            # Stop exactly at the final follow check, after the current-state
            # read. Another writer must not enter before the terminal update.
            if "kind='user.follow' AND seq>" in statement and not deciding.is_set():
                deciding.set()
                release.wait(5)
        self.primary.db.set_trace_callback(trace)
        self.operator.db.execute("PRAGMA busy_timeout=0")
        self.attach(lambda:FINISH)
        self.engine.step()
        mutate = (lambda:self.operator.cancel(self.ticket)) if action=="cancel" else (
            lambda:self.operator.follow(self.ticket,"new objective"))
        try:
            self.assertTrue(deciding.wait(5),"worker did not reach its final decision")
            with self.assertRaisesRegex(sqlite3.OperationalError,"locked"):
                mutate()
        finally:
            release.set()
        self.quiesce()
        self.primary.db.set_trace_callback(None)
        self.assertEqual(self.job()["state"],"waiting")
        self.assertEqual(len(self.events("auto.milestone")),1)
        mutate()
        if action == "cancel":
            self.assertEqual(self.job()["state"],"cancelled")
            self.engine.step()
            self.assertEqual(len(self.packets),1)
        else:
            self.assertEqual(self.job()["state"],"queued")
            self.engine.step()
            self.quiesce()
            self.assertEqual(self.packets[-1]["objective"],"new objective")

    def test_stop_serializes_with_final_commit(self):
        self.finalized_first("cancel")

    def test_follow_serializes_with_final_commit(self):
        self.finalized_first("follow")

    def operator_first(self, action, stale_failure=False):
        running, release = threading.Event(), threading.Event()
        def reply():
            if len(self.packets) == 1:
                running.set()
                if not release.wait(5):
                    raise TimeoutError("test barrier expired")
                if stale_failure:
                    raise ValueError("old non-transient adapter error")
            return FINISH
        self.attach(reply)
        self.engine.step()
        try:
            self.assertTrue(running.wait(5))
            if action == "cancel":
                self.operator.cancel(self.ticket)
            else:
                self.operator.follow(self.ticket,"new objective")
        finally:
            release.set()
        self.quiesce()
        self.assertFalse(self.events("auto.milestone"))
        self.assertFalse(self.events("failed"))
        if action == "cancel":
            self.assertEqual(self.job()["state"],"cancelled")
            self.assertFalse(self.events("reply"))
        else:
            root = self.job()
            self.assertEqual((root["state"],root["turn"],root["ready_after"]),("queued",2,0))
            self.engine.step()
            self.quiesce()
            self.assertEqual(self.packets[-1]["user_updates"],["new objective"])
            self.assertEqual(self.packets[-1]["objective"],"new objective")

    def test_committed_stop_cannot_be_overwritten_by_old_reply(self):
        self.operator_first("cancel")

    def test_committed_follow_supersedes_old_finish(self):
        self.operator_first("follow")

    def test_follow_supersedes_old_nontransient_exception(self):
        self.operator_first("follow",stale_failure=True)

    def legacy(self):
        # Existing pre-continuous auto tickets intentionally lack this opt-in row.
        with self.operator.tx() as db:
            db.execute("DELETE FROM continuous_runs WHERE ticket=?",(self.ticket,))

    def fail_transiently(self):
        raise ConnectionError("controlled reset")

    def advance_cooldown(self):
        # Move only the persisted deadline. Retry accounting still uses the
        # real recorded events and the normal scheduler entry point.
        with self.operator.tx() as db:
            db.execute("UPDATE jobs SET ready_after=0 WHERE ticket=?",(self.ticket,))

    def test_continuous_four_transient_failures_remain_queued(self):
        self.attach(self.fail_transiently)
        for attempt in range(1,5):
            self.advance_cooldown()
            self.engine.step()
            self.quiesce()
            root = self.job()
            self.assertEqual((root["state"],root["turn"]),("queued",attempt+1))
            self.assertGreater(root["ready_after"],0)
            retry = [json.loads(e["payload"]) for e in self.events("auto.retry")]
            self.assertEqual(len(retry),attempt)
            self.assertGreater(retry[-1]["after_seconds"],0)
            self.assertFalse(self.events("failed"))

    def test_legacy_auto_finishes_in_one_turn(self):
        self.legacy()
        self.attach(lambda:FINISH)
        self.engine.step()
        self.quiesce()
        self.assertIsNone(self.packets[0]["autonomy"])
        self.assertEqual((self.job()["state"],self.job()["turn"]),("done",1))
        self.assertEqual(len(self.events("auto.finish")),1)
        self.assertFalse(self.events("auto.milestone"))

    def test_legacy_auto_keeps_two_retry_limit(self):
        self.legacy()
        self.attach(self.fail_transiently)
        for expected in ("queued","queued","failed"):
            self.advance_cooldown()
            self.engine.step()
            self.quiesce()
            self.assertEqual(self.job()["state"],expected)
        self.assertEqual(len(self.events("auto.retry")),2)
