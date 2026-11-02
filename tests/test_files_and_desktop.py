import hashlib
from pathlib import Path
import tempfile
import unittest

from engine import Store

class StorageTests(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory()
        self.root=Path(self.tmp.name)
        self.project=self.root/"project"
        self.project.mkdir()
        self.store=Store(self.root/"state")

    def tearDown(self):
        self.store.db.close()
        self.tmp.cleanup()

    def test_content_addressed_binary_replay(self):
        tid=self.store.create("snapshot",self.project,"codex",False)
        job=self.store.rows("SELECT * FROM jobs WHERE ticket=?",(tid,))[0]
        with self.store.tx() as db:
            db.execute("UPDATE jobs SET state='running' WHERE id=?",(job["id"],))
        body=bytes(range(256))*200
        path=self.project/"示例.bin"
        path.write_bytes(body)
        cmd=dict(id="abc",to="deepseek",body="核对字节",files=[path.name])
        jid=self.store.dispatch(job,cmd)
        path.write_bytes(b"new working tree contents")
        self.store.dispatch(job,cmd)
        with self.store.tx() as db:
            child=db.execute("SELECT * FROM jobs WHERE id=?",(jid,)).fetchone()
            manifest=self.store.envelope(db,child)["files"][0]
        self.assertEqual(Path(manifest["object_path"]).read_bytes(),body)
        self.assertEqual((manifest["bytes"],manifest["sha256"]),(len(body),hashlib.sha256(body).hexdigest()))
        self.assertEqual(len(self.store.rows("SELECT * FROM job_files")),1)
        outside=self.root/"private"
        outside.write_bytes(b"outside")
        with self.assertRaises(ValueError):
            self.store.dispatch(job,{**cmd,"id":"outside","files":["../private"]})
        self.assertEqual(len(self.store.rows("SELECT * FROM jobs")),2)
