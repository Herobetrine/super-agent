"""Visible block boundaries never change provider text or its native events."""
from contextlib import ExitStack
import json
import tempfile
import threading
import unittest
from unittest.mock import patch

from adapters import antigravity, claude, deepseek


class VisibleBoundariesTest(unittest.TestCase):
    def replay(self, adapter, records, patches, **options):
        native, visible = [], []
        def emit(kind,data):
            if kind == "native":
                native.append(data)
            if kind in ("message.boundary","delta"):
                visible.append((kind,data))
        def stream(argv,cwd,body,receive,cancel,**opts):
            for record in records:
                receive(json.dumps(record,ensure_ascii=False).encode("utf-8"))
        with tempfile.TemporaryDirectory() as tmp, ExitStack() as ctx:
            for p in [*patches,patch.object(adapter,"stream_lines",stream)]:
                ctx.enter_context(p)
            result,_ = adapter.run("fixture",tmp,emit,threading.Event(),**options)
        self.assertEqual(native,records)
        self.assertEqual(result,"文\r\n ")
        return visible

    def test_claude_block_stop_not_every_delta(self):
        def chunk(text):
            return {"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":text}}}
        stop = {"type":"stream_event","event":{"type":"content_block_stop","index":0}}
        rows=[chunk("SA_ROLE "),chunk('{"role":"审"}'),stop,chunk("文\r\n "),stop,
              {"type":"result","subtype":"success","result":"文\r\n ","session_id":"fixture"}]
        visible=self.replay(claude,rows,[patch.object(claude,"_executable",return_value="fixture"),
            patch.object(claude,"_resolve_selection",return_value=("https://unit.invalid","fixture","ANTHROPIC_API_KEY","x"))],
            selection={"mode":"cli"})
        self.assertEqual(visible,[("delta","SA_ROLE "),("delta",'{"role":"审"}'),("message.boundary",None),
                                  ("delta","文\r\n "),("message.boundary",None)])

    def test_dsh_committed_blocks(self):
        route=dict(official=False,endpoint="https://unit.invalid",key="fixture",models={"x":{"id":"x"}},
                   api="openai-responses",reasoning=None,source="fixture")
        rows=[{"type":"session","sessionId":"id"},{"type":"text","text":"A"},{"type":"text","text":"文\r\n "},
              {"type":"final","text":"文\r\n "}]
        visible=self.replay(deepseek,rows,[patch.object(deepseek,"_command",return_value=["fixture"]),
            patch.object(deepseek.catalog,"resolve",return_value=("p","x",route))])
        self.assertEqual(visible,[("delta","A"),("message.boundary",None),("delta","文\r\n "),("message.boundary",None)])

    def test_agy_done_includes_its_last_delta(self):
        rows=[{"event":"step_update","step_update":{"step_type":"agent_response","state":state,"text_delta":part}}
              for state,part in (("ACTIVE","文"),("ACTIVE","\r"),("DONE","\n "))]
        rows.append({"event":"result","result":{"status":"SUCCESS","response":"文\r\n ","conversation_id":"fixture"}})
        visible=self.replay(antigravity,rows,[patch.object(antigravity.shutil,"which",return_value="fixture"),
            patch.object(antigravity,"_reader_env",return_value={})])
        self.assertEqual(visible,[("delta","文"),("delta","\r"),("delta","\n "),("message.boundary",None)])
