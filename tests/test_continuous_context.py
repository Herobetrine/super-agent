import hashlib
import json
import os
import tempfile
import threading
import unittest
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from unittest.mock import patch

from continuous_context import compact_packet


def wire(packet):
    return json.dumps(packet, ensure_ascii=False, separators=(",", ":"), allow_nan=False).encode()


class ContinuousContext(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.store = Path(tmp.name) / "私有 上下文"
        self.guide = "规则：保留全部权限。\n第二行\n"

    def test_fitting_packet_keeps_identity_without_creating_files(self):
        packet = {"goal":"你好😀\r\n","permission_profile":"auto"}
        limit = len(self.guide.encode()) + 1 + len(wire(packet))
        self.assertIs(compact_packet(packet,self.guide,self.store,limit),packet)
        self.assertFalse(self.store.exists())

    def test_recent_reports_goals_and_utf8_archive(self):
        packet = {"permission_profile":"partial", "allowed_child_permissions":["readonly","ask"],
                  "role":"验证角色", "project":"E:/中文", "job":12,
                  "goal":"总体目标", "objective":"当前目标", "assignment":"逐字完成",
                  "reports":[{"id":i,"result":'甲\r\n"\\\t🙂' * 80} for i in range(60)],
                  "user_updates":["旧消息"] * 60 + ["最新修改  "], "previous_reply":"OK"}
        original = wire(packet)
        projected = compact_packet(packet,self.guide,self.store,2200)
        self.assertLessEqual(len(wire(projected))+len(self.guide.encode())+1,2200)
        self.assertEqual(projected["reports"][-1],packet["reports"][-1])
        self.assertEqual(projected["user_updates"][-1],"最新修改  ")
        for key in ("goal","objective","assignment","role","permission_profile","allowed_child_permissions"):
            self.assertEqual(projected[key],packet[key])
        self.assertEqual(wire(packet),original)
        ref = projected["full_context"]
        path = Path(ref["object_path"])
        self.assertEqual(path.parent,(self.store/"context-objects").resolve())
        self.assertEqual(path.read_bytes(),original)
        self.assertEqual(ref["bytes"],len(original))
        self.assertEqual(ref["sha256"],hashlib.sha256(original).hexdigest())
        self.assertEqual(json.loads(path.read_bytes()),packet)

    def test_large_individual_fields_keep_head_tail_and_latest_correction(self):
        packet = {"goal":"BEGIN"+"😀\u0001"*250_000+"END","objective":"fix",
                  "user_updates":["prior","LATEST"], "reports":[{"id":42,"state":"failed",
                  "result":"START"+"证"*10000+"FINISH"}]}
        projected = compact_packet(packet,self.guide,self.store,1800)
        self.assertLessEqual(len(wire(projected))+len(self.guide.encode())+1,1800)
        self.assertTrue(projected["goal"].startswith("BEGIN"))
        self.assertTrue(projected["goal"].endswith("END"))
        self.assertEqual(projected["user_updates"][-1],"LATEST")
        report = projected["reports"][0]
        self.assertEqual(report["id"],42)
        self.assertTrue(report["result"].startswith("START"))
        self.assertTrue(report["result"].endswith("FINISH"))

    def test_concurrent_readers_see_only_complete_objects_and_corruption_is_repaired(self):
        packet = {"goal":"é\t"*35_000,"role":"核验者"}
        expected = wire(packet)
        def publish(_):
            view = compact_packet(packet,"规则",self.store,1100)
            dest = Path(view["full_context"]["object_path"])
            self.assertEqual(dest.read_bytes(),expected)
            return dest
        with ThreadPoolExecutor(max_workers=6) as pool:
            files = set(pool.map(publish,range(18)))
        self.assertEqual(len(files),1)
        dest, = files
        dest.write_bytes(b"x" * len(expected))
        self.assertEqual(publish(None),dest)
        self.assertEqual(list(dest.parent.iterdir()),[dest])

    def test_impossible_budget_archives_before_failing_and_io_failure_returns_no_pointer(self):
        packet = {"role":"不可缩写","goal":"x"*1200}
        with self.assertRaisesRegex(ValueError,"权限"):
            compact_packet(packet,self.guide,self.store,10)
        dest, = (self.store/"context-objects").glob("*.json")
        self.assertEqual(dest.read_bytes(),wire(packet))
        with patch("continuous_context.os.replace",side_effect=OSError("simulated full disk")):
            with self.assertRaises(OSError):
                compact_packet({**packet,"goal":"y"*1200},self.guide,self.store,500)
        self.assertEqual(list(dest.parent.iterdir()),[dest])

    def test_final_path_becomes_visible_after_complete_publication(self):
        packet = {"goal":"中文字" * 9000}
        data = wire(packet)
        dest = self.store / "context-objects" / (hashlib.sha256(data).hexdigest() + ".json")
        pending, release = threading.Event(), threading.Event()
        replace = os.replace
        def delayed_publish(staged, target):
            self.assertEqual(Path(staged).read_bytes(),data)
            pending.set()
            if not release.wait(3):
                raise TimeoutError("test publication timeout")
            replace(staged,target)
        with ThreadPoolExecutor(max_workers=1) as pool, patch("continuous_context.os.replace",side_effect=delayed_publish):
            job = pool.submit(compact_packet,packet,"",self.store,750)
            try:
                self.assertTrue(pending.wait(3))
                self.assertFalse(dest.exists())
            finally:
                release.set()
            self.assertEqual(Path(job.result()["full_context"]["object_path"]),dest)
        self.assertEqual(dest.read_bytes(),data)
