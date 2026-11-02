"""Check approval failure paths and native flags without requiring model access."""
import json
import os
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import Mock, patch

from adapters import antigravity, claude, deepseek
from adapters.native_permissions import scope_for


class NativePermissionsTest(unittest.TestCase):
    def setUp(self):
        self.cancel = threading.Event()
        self.emit = Mock()
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.cwd = self.tmp.name

    def test_reject_before_launch_and_cancel_during_approval(self):
        with patch.object(deepseek.catalog,"resolve") as route:
            for value in (False,"yes",None):
                self.emit.approve.return_value = value
                with self.assertRaisesRegex(RuntimeError,"未获批准"):
                    deepseek.run("fixture",self.cwd,self.emit,self.cancel,writable=True,permission_profile="ask")
            def cancel_and_accept(_):
                self.cancel.set()
                return True
            self.emit.approve.side_effect = cancel_and_accept
            with self.assertRaisesRegex(RuntimeError,"取消"):
                deepseek.run("fixture",self.cwd,self.emit,self.cancel,writable=True,permission_profile="ask")
            route.assert_not_called()
        with self.assertRaises(ValueError):
            scope_for("deepseek","unexpected",False,self.cwd,self.emit,threading.Event())
        for profile in ("full","auto"):
            with self.assertRaisesRegex(RuntimeError,"冲突"):
                scope_for("deepseek",profile,False,self.cwd,self.emit,threading.Event())

    def test_low_mode_writer_capabilities(self):
        for profile in (None,"partial","ask"):
            for mod in (claude,antigravity):
                with self.assertRaisesRegex(RuntimeError,"只读"):
                    mod.run("fixture",self.cwd,self.emit,self.cancel,writable=True,permission_profile=profile)
            self.assertEqual(scope_for("deepseek",profile,False,self.cwd,self.emit,self.cancel),"read-only")
        self.emit.approve.assert_not_called()
        for profile in (None,"partial"):
            self.assertEqual(scope_for("deepseek",profile,True,self.cwd,self.emit,self.cancel,
                                       workspace_writer=True),"workspace-write")
        self.emit.approve.return_value = True
        self.assertEqual(scope_for("deepseek","ask",True,self.cwd,self.emit,self.cancel,workspace_writer=True),
                         "workspace-write")
        approval = self.emit.approve.call_args.args[0]
        self.assertEqual(approval["cwd"],str(Path(self.cwd).resolve()))
        self.assertEqual(approval["scope"],"current_turn")

    def test_dsh_full_profile_overlay_and_separate_history(self):
        route = dict(label="test",endpoint="https://example.invalid",official=False,ref="TEST_KEY",
                     key="dummy",models={"model":{"id":"model"}},source="fixture",
                     api="openai-responses",reasoning=None)
        def stream(args,cwd,body,receive,cancel,*,env):
            policy = Path(args[args.index("--patch")+1]).read_text(encoding="utf-8")
            self.assertIn("defaultPreset: danger-full-access",policy)
            self.assertIn("approval: never",policy)
            self.assertEqual(Path(env["DSH_HOME"]).name,"danger-full-access")
            self.assertEqual(env["DSH_PERMISSION_MODE"],"danger-full-access")
            receive(b'{"type":"session","sessionId":"id"}')
            receive(b'{"type":"final","text":"ok"}')
        with patch.object(deepseek.catalog,"resolve",return_value=("test","model",route)), \
             patch.object(deepseek,"_command",return_value=["dsh"]), patch.object(deepseek,"stream_lines",stream):
            for profile in ("full","auto"):
                result = deepseek.run("fixture",self.cwd,self.emit,self.cancel,writable=True,permission_profile=profile)
                self.assertEqual(result,("ok","id"))

    def test_claude_full_toolset_and_auth_pinning(self):
        def stream(args,cwd,body,receive,cancel,*,env):
            self.assertIn("bypassPermissions",args)
            self.assertFalse({"--tools","--safe-mode","--strict-mcp-config","--setting-sources"}.intersection(args))
            self.assertEqual(env["CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST"],"1")
            self.assertEqual(env["ANTHROPIC_BASE_URL"],"https://example.invalid")
            self.assertEqual(env["ANTHROPIC_AUTH_TOKEN"],"dummy")
            self.assertNotIn("dummy",args)
            receive(b'{"type":"result","subtype":"success","result":"ok","session_id":"id"}')
        with patch.object(claude,"_executable",return_value="claude"), \
             patch.object(claude,"_resolve_selection",return_value=("https://example.invalid","dummy","ANTHROPIC_AUTH_TOKEN","model")), \
             patch.object(claude,"stream_lines",stream):
            for profile in ("auto","full"):
                self.assertEqual(claude.run("fixture",self.cwd,self.emit,self.cancel,writable=True,selection={"mode":"cli"},
                                           permission_profile=profile),("ok","id"))

    def test_agy_full_uses_real_home_and_session_only_settings(self):
        def stream(args,cwd,body,receive,cancel,*,env):
            self.assertIn("--dangerously-skip-permissions",args)
            self.assertNotIn("--disable-slash-commands",args)
            self.assertNotIn("permissions",json.loads(args[args.index("--settings")+1]))
            self.assertEqual(env.get("USERPROFILE"),os.environ.get("USERPROFILE"))
            receive(b'{"event":"result","result":{"status":"SUCCESS","response":"ok","conversation_id":"id"}}')
        with patch.object(antigravity.shutil,"which",return_value="agy"), \
             patch.object(antigravity,"_reader_env",side_effect=AssertionError("restricted home")), \
             patch.object(antigravity,"stream_lines",stream):
            for profile in ("auto","full"):
                self.assertEqual(antigravity.run("fixture",self.cwd,self.emit,self.cancel,writable=True,permission_profile=profile),
                                 ("ok","id"))
