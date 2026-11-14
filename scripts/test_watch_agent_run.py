"""Synthetic offline fixtures. No live journal or provider access."""
import importlib.util
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest


SPEC = importlib.util.spec_from_file_location("watch_agent_run", Path(__file__).with_name("watch-agent-run.py"))
watch = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(watch)


class WatchRunTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        self.addCleanup(self.db.close)
        self.db.executescript("""
            CREATE TABLE jobs(id INTEGER PRIMARY KEY,ticket INTEGER,parent INTEGER,agent TEXT,turn INTEGER,state TEXT);
            CREATE TABLE events(seq INTEGER PRIMARY KEY,ts REAL,ticket INTEGER,job INTEGER,turn INTEGER,kind TEXT,payload TEXT);
            CREATE INDEX events_ticket ON events(ticket,seq);
            INSERT INTO jobs VALUES(1,6,NULL,'claude',2,'running');
            INSERT INTO jobs VALUES(2,7,NULL,'claude',1,'done');
            INSERT INTO jobs VALUES(3,6,1,'codex',1,'running');
            INSERT INTO jobs VALUES(4,6,1,'deepseek',1,'running');
        """)
        self.seq = 100
        self.sampler = watch.Sampler(self.db, 6, 100, 100)

    def event(self, value, kind="native", job=1, turn=1, ticket=6):
        self.seq += 1
        self.db.execute("INSERT INTO events VALUES(?,?,?,?,?,?,?)",
                        (self.seq, 123.0, ticket, job, turn, kind, json.dumps(value)))

    def usage(self, output=5):
        return dict(input_tokens=10, cache_read_input_tokens=80, cache_creation_input_tokens=10, output_tokens=output)

    def test_claude_dedup_separate_sources_and_content_free(self):
        self.event({"type": "assistant", "message": {"id": "secret-message-id", "usage": self.usage(1),
                    "content": [{"text": "DO-NOT-LEAK"}], "credential": "DO-NOT-LEAK"}})
        self.event({"type": "assistant", "message": {"id": "secret-message-id", "usage": self.usage(5)}})
        self.event({"type": "result", "subtype": "success", "usage": self.usage(), "result": "DO-NOT-LEAK"})
        self.event({"type": "result", "subtype": "success", "usage": self.usage()})
        self.event({"type": "result", "parent_tool_use_id": "child", "usage": self.usage(999)})
        self.event({"type": "result", "subtype": "success", "usage": self.usage(999)}, ticket=7, job=2)
        self.event({"guide": "DO-NOT-LEAK", "full_prompt_bytes": 1000, "sent_prompt_bytes": 100,
                    "saved_bytes": 900, "incremental": True}, kind="running")
        records, more = self.sampler.poll()
        summary = self.sampler.summary()
        self.assertFalse(more)
        self.assertEqual(summary["claude_terminal"]["samples"], 1)
        self.assertEqual(summary["claude_terminal"]["tokens"]["output_tokens"], 5)
        self.assertEqual(summary["claude_requests_separate_do_not_add"]["samples"], 1)
        self.assertEqual(summary["claude_requests_separate_do_not_add"]["tokens"]["output_tokens"], 5)
        self.assertEqual(summary["claude_terminal"]["cache_read_fraction"], .8)
        self.assertEqual(summary["delivery"]["saved_bytes"], 900)
        request_rows = [row for row in records if row.get("source") == "claude_request_main_loop"]
        self.assertEqual(request_rows[0]["sample_id"], request_rows[1]["sample_id"])
        self.assertTrue(request_rows[0]["is_root"])
        self.assertTrue(all(row["ticket"] == 6 for row in records))
        serialized = json.dumps([records, summary])
        self.assertNotIn("DO-NOT-LEAK", serialized)
        self.assertNotIn("secret-message-id", serialized)
        self.assertNotIn('"job": 2', serialized)

    def test_null_zero_and_failed_result(self):
        self.event({"type": "result", "subtype": "error_during_execution", "usage": {
            name: 0 for name in watch.TOKEN_NAMES}})
        self.event({"type": "result", "subtype": "success", "usage": {name: 0 for name in watch.TOKEN_NAMES}}, turn=2)
        self.sampler.poll()
        summary = self.sampler.summary()
        self.assertEqual(summary["claude_terminal"]["samples"], 2)
        self.assertEqual(summary["claude_terminal"]["complete_samples"], 2)
        self.assertIsNone(summary["claude_terminal"]["cache_read_fraction"])

    def test_recovery_delivery_is_separate_from_full_and_delta(self):
        sizes = dict(full_prompt_bytes=1000, sent_prompt_bytes=100, saved_bytes=900)
        self.event(dict(sizes, recovery_delivery=True, incremental=False,
                        guide="DO-NOT-LEAK"), kind="running")
        # Recovery takes precedence even if a producer supplies both flags.
        self.event(dict(sizes, recovery_delivery=True, incremental=True), kind="running")
        records, _ = self.sampler.poll()
        delivery = self.sampler.summary()["delivery"]
        self.assertEqual([row["mode"] for row in records], ["recovery", "recovery"])
        self.assertEqual(delivery["requests"], 2)
        self.assertEqual(delivery["recovery_requests"], 2)
        self.assertEqual(delivery.get("full_requests", 0), 0)
        self.assertEqual(delivery.get("delta_requests", 0), 0)
        self.assertEqual(delivery["measured_requests"], 2)
        self.assertEqual(delivery["full_prompt_bytes"], 2000)
        self.assertEqual(delivery["sent_prompt_bytes"], 200)
        self.assertEqual(delivery["saved_bytes"], 1800)
        self.assertNotIn("DO-NOT-LEAK", json.dumps([records, delivery]))

    def test_legacy_delivery_flags_keep_existing_modes(self):
        flags = [{}, {"incremental": True}, {"incremental": False},
                 {"recovery_delivery": False, "incremental": True},
                 {"recovery_delivery": "true"}, {"incremental": "true"}]
        for value in flags:
            self.event(value, kind="running")
        records, _ = self.sampler.poll()
        delivery = self.sampler.summary()["delivery"]
        self.assertEqual([row["mode"] for row in records],
                         ["full", "delta", "full", "delta", "full", "full"])
        self.assertEqual(delivery["requests"], 6)
        self.assertEqual(delivery["full_requests"], 4)
        self.assertEqual(delivery["delta_requests"], 2)
        self.assertNotIn("recovery_requests", delivery)
        self.assertTrue(all(not row["complete"] for row in records))

    def test_failed_terminal_preserves_zero_and_excludes_positive_usage(self):
        self.event({"type": "result", "subtype": "error_during_execution",
                    "usage": dict.fromkeys(watch.TOKEN_NAMES, 0)})
        self.event({"type": "result", "subtype": "success", "is_error": True,
                    "usage": self.usage()}, turn=2)
        self.sampler.poll()
        summary = self.sampler.summary()
        terminal = summary["claude_terminal"]
        self.assertEqual(terminal["tokens"]["input_tokens"], 10)
        self.assertEqual(terminal["complete_samples"], 2)
        self.assertEqual(terminal["cache_ratio_samples"], 0)
        self.assertEqual(terminal["outcomes"], {"failed": 2})
        self.assertEqual(summary["observed_turns"][0]["claude_terminal_tokens"],
                         dict.fromkeys(watch.TOKEN_NAMES, 0))
        self.assertEqual(summary["cache_assessment"], "no_successful_terminal")

    def test_partial_requests_survive_failed_terminal_separately(self):
        self.event({"type": "assistant", "message": {"id": "request", "usage": self.usage()}})
        self.sampler.poll()
        pending = self.sampler.summary()
        self.assertEqual(pending["observed_turns"][0]["outcome"], "pending")
        self.assertEqual(pending["claude_terminal"]["samples"], 0)
        self.assertEqual(pending["cache_assessment"], "awaiting_terminal")
        self.event({"type": "result", "subtype": "error_during_execution"})
        self.sampler.poll()
        summary = self.sampler.summary()
        self.assertEqual(summary["observed_turns"][0]["outcome"], "failed")
        self.assertEqual(summary["claude_terminal"]["cache_ratio_samples"], 0)
        self.assertIsNone(summary["claude_terminal"]["tokens"]["input_tokens"])
        self.assertEqual(summary["claude_requests_separate_do_not_add"]["cache_ratio_samples"], 1)
        self.assertEqual(summary["claude_requests_separate_do_not_add"]["cache_read_fraction"], .8)

    def test_unknown_and_success_zero_are_distinct_from_cache_samples(self):
        self.event({"type": "result", "usage": {"input_tokens": 0}})
        self.event({"type": "result", "subtype": "success",
                    "usage": dict.fromkeys(watch.TOKEN_NAMES, 0)}, turn=2)
        self.sampler.poll()
        summary = self.sampler.summary()
        self.assertEqual(summary["claude_terminal"]["outcomes"], {"unknown": 1, "success": 1})
        self.assertEqual(summary["claude_terminal"]["cache_ratio_samples"], 0)
        self.assertEqual(summary["cache_assessment"], "warm_up_or_missing")
        tokens = summary["observed_turns"][0]["claude_terminal_tokens"]
        self.assertEqual(tokens["input_tokens"], 0)
        self.assertIsNone(tokens["cache_read_input_tokens"])

    def test_terminal_replay_replaces_outcome_without_double_counting(self):
        self.event({"type": "result", "subtype": "success", "usage": self.usage()})
        self.sampler.poll()
        self.assertEqual(self.sampler.summary()["claude_terminal"]["cache_ratio_samples"], 1)
        self.event({"type": "result", "subtype": "error_during_execution",
                    "usage": dict.fromkeys(watch.TOKEN_NAMES, 0)})
        self.event({}, kind="reply")
        self.sampler.poll()
        summary = self.sampler.summary()
        self.assertEqual(summary["claude_terminal"]["samples"], 1)
        self.assertEqual(summary["claude_terminal"]["cache_ratio_samples"], 0)
        self.assertEqual(summary["claude_terminal"]["outcomes"], {"failed": 1})
        self.assertEqual(summary["claude_terminal"]["tokens"]["output_tokens"], 0)

    def test_pause_failure_ends_attempt_without_inventing_native_usage(self):
        self.event({"state": "waiting", "reason": "DO-NOT-LEAK"}, kind="turn.pause_failure")
        records, _ = self.sampler.poll()
        summary = self.sampler.summary()
        turn = summary["observed_turns"][0]
        self.assertTrue(turn["ended"])
        self.assertEqual(turn["outcome"], "failed")
        self.assertIsNotNone(turn["platform_ended_at"])
        self.assertIsNone(turn["claude_terminal_tokens"])
        self.assertEqual(summary["claude_terminal"]["samples"], 0)
        self.assertNotIn("DO-NOT-LEAK", json.dumps(records))

    def test_codex_first_last_delta_dedup_and_rollback(self):
        def codex(total, last):
            return {"method": "thread/tokenUsage/updated", "params": {"threadId": "secret-thread",
                    "tokenUsage": {"total": {"totalTokens": total, "inputTokens": total - 5, "outputTokens": 5},
                                   "last": {"totalTokens": last, "inputTokens": last - 2, "outputTokens": 2}}}}
        self.event(codex(100, 20), job=3)
        self.event(codex(100, 20), job=3)
        self.event(codex(120, 20), job=3)
        self.event(codex(110, 20), job=3)
        self.sampler.poll()
        summary = self.sampler.summary()
        self.assertEqual(summary["codex_observed_deltas"]["samples"], 2)
        self.assertEqual(summary["codex_observed_deltas"]["tokens"]["input_tokens"], 38)
        self.assertEqual(summary["codex_missing_baselines_or_regressions"], 2)

    def test_deepseek_step_dedup_and_missing(self):
        self.event({"type": "session", "sessionId": "secret-session"}, job=4)
        frame = {"type": "status", "phase": "step_end", "turn": 1, "step": 1,
                 "usage": {"totalTokens": 100, "outputTokens": 10, "cacheReadTokens": 70, "cacheWriteTokens": 5}}
        self.event(frame, job=4)
        self.event(frame, job=4)
        self.event({"type": "status", "phase": "step_end", "turn": 1, "step": 2}, job=4)
        self.sampler.poll()
        summary = self.sampler.summary()["deepseek_steps"]
        self.assertEqual(summary["samples"], 2)
        self.assertEqual(summary["complete_samples"], 1)
        self.assertEqual(summary["tokens"]["input_tokens"], 90)
        self.assertAlmostEqual(summary["cache_read_fraction"], 70 / 90)

    def test_bounded_tail_and_atomic_summary(self):
        self.sampler.batch_size = 1
        self.event({}, kind="input")
        self.event({}, kind="retry")
        _, more = self.sampler.poll()
        self.assertTrue(more)
        self.assertEqual(self.sampler.cursor, 101)
        self.sampler.poll()
        self.assertEqual(self.sampler.event_counts["retry"], 1)
        self.assertEqual(self.sampler.poll(), ([], False))
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "summary.json"
            watch.atomic_summary(path, self.sampler.summary())
            self.assertEqual(json.loads(path.read_text())["through_seq"], 102)
            self.assertFalse(path.with_name(path.name + ".tmp").exists())

    def test_platform_terminal_pause_and_wall_time(self):
        self.event({}, kind="running", job=3)
        self.event({"body": "DO-NOT-LEAK"}, kind="ticket.pause_requested", job=3)
        self.event({"text": "DO-NOT-LEAK"}, kind="reply", job=3)
        records, _ = self.sampler.poll()
        turn = self.sampler.summary()["observed_turns"][0]
        self.assertTrue(turn["ended"])
        self.assertEqual(turn["wall_seconds"], 0)
        self.assertEqual(self.sampler.event_counts["pause_gate"], 1)
        self.assertEqual(records[0]["parent"], 1)
        self.assertFalse(records[0]["is_root"])
        self.assertNotIn("DO-NOT-LEAK", json.dumps(records))

    def test_platform_failure_alert_survives_replay_without_leaking_content(self):
        self.event("DO-NOT-LEAK", kind="failed", job=1)
        self.event("DO-NOT-LEAK", kind="turn.gave_up", job=1)
        self.event({"type": "result", "subtype": "success", "usage": self.usage()}, job=1)
        self.event({"text": "DO-NOT-LEAK"}, kind="reply", job=1)
        self.event({}, kind="cancelled", job=4)
        records, _ = self.sampler.poll()
        summary = self.sampler.summary()
        self.assertEqual(summary["alert_candidates"], ["observed_failed_attempt"])
        self.assertEqual(len(summary["platform_failed_attempts"]), 1)
        self.assertEqual(summary["platform_failed_attempts"][0]["job"], 1)
        self.assertEqual(summary["platform_failed_attempts"][0]["event"], "failed")
        self.assertEqual(summary["observed_turns"][0]["outcome"], "failed")
        self.assertEqual(summary["claude_terminal"]["cache_ratio_samples"], 0)
        self.assertNotIn("DO-NOT-LEAK", json.dumps([records, summary]))

    def test_rejection_alert_does_not_mark_valid_reply_failed(self):
        self.event({"reason": "DO-NOT-LEAK", "command": "DO-NOT-LEAK"}, kind="protocol.rejected", job=3)
        self.event({"text": "DO-NOT-LEAK"}, kind="reply", job=3)
        records, _ = self.sampler.poll()
        summary = self.sampler.summary()
        self.assertEqual(summary["alert_candidates"], ["observed_protocol_rejection"])
        self.assertEqual(summary["protocol_rejection_events"], 1)
        self.assertEqual(summary["observed_turns"][0]["outcome"], "success")
        self.assertEqual(summary["platform_failed_attempts"], [])
        self.assertEqual(records[0]["record"], "control")
        self.assertNotIn("DO-NOT-LEAK", json.dumps([records, summary]))
        self.assertEqual(self.sampler.poll(), ([], False))
        self.assertEqual(self.sampler.summary()["protocol_rejection_events"], 1)


    def test_failed_native_result_and_durable_gate_are_not_success_or_zero_cache(self):
        self.event({"type": "result", "subtype": "success", "is_error": True,
                    "usage": dict.fromkeys(watch.TOKEN_NAMES, 0)})
        self.event({}, kind="turn.retry_scheduled")
        self.event({}, kind="auto.retry")
        records, _ = self.sampler.poll()
        summary = self.sampler.summary()
        self.assertTrue(summary["observed_turns"][0]["native_is_error"])
        self.assertIsNone(summary["claude_terminal"]["cache_read_fraction"])
        self.assertEqual([r["event"] for r in records if r["record"] == "retry"],
                         ["turn.retry_scheduled", "auto.retry"])
        self.assertIsNone(summary["pause_state"])
        self.sampler.connection.execute("CREATE TABLE ticket_pause(ticket INTEGER PRIMARY KEY,state TEXT)")
        self.sampler.connection.execute("INSERT INTO ticket_pause VALUES(?,'paused')", (self.sampler.ticket,))
        self.assertEqual(self.sampler.summary()["pause_state"], "paused")


if __name__ == "__main__":
    unittest.main()
