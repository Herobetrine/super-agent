"""Request/response contract checks against a disposable app-server protocol peer."""
import json
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

from adapters import codex


PEER = """
import json,sys
send = lambda d: print(json.dumps(d),flush=True)
recv = lambda: json.loads(sys.stdin.readline())
recv(); send({"id":1,"result":{}}); recv()
start = recv()["params"]
policy = start["approvalPolicy"]
sandbox = {"read-only":"readOnly","workspace-write":"workspaceWrite","danger-full-access":"dangerFullAccess"}[start["sandbox"]]
send({"id":2,"result":{"thread":{"id":"thread"},"sandbox":{"type":sandbox,"networkAccess":False},
     "approvalPolicy":policy,"approvalsReviewer":"user"}})
recv(); send({"id":3,"result":{"turn":{"id":"turn"}}})
p = {"threadId":"thread","turnId":"turn","itemId":"item"}
file = {"id":"item","type":"fileChange","changes":[{"path":"f.txt","diff":"+test"}]}
send({"method":"item/started","params":dict(p,item=file)})
requests = [
 ("item/commandExecution/requestApproval",dict(p,command="test shell command")),
 ("item/commandExecution/requestApproval",dict(p,networkApprovalContext={"host":"test.invalid","protocol":"https"})),
 ("item/fileChange/requestApproval",p),
 ("item/permissions/requestApproval",dict(p,permissions={"network":{"enabled":True}})),
 ("item/fileChange/requestApproval",dict(p,itemId="unknown")),
 ("item/commandExecution/requestApproval",dict(p,turnId="stale",command="wrong turn")),
 ("item/commandExecution/requestApproval",dict(p,command="persistent only",availableDecisions=["acceptForSession"])),
]
results = []
for i,(method,params) in enumerate(requests,9):
 send({"id":i,"method":method,"params":params}); results.append(recv()["result"])
send({"method":"item/agentMessage/delta","params":dict(p,itemId="part",delta="no LF")})
send({"method":"item/completed","params":dict(p,item={"id":"part","type":"agentMessage","phase":"commentary","text":"no LF"})})
send({"method":"item/completed","params":dict(p,item={"id":"done","type":"agentMessage","text":json.dumps(results)})})
send({"method":"turn/completed","params":dict(p,turn={"id":"turn","status":"completed"})})
"""


class CodexPermissions(unittest.TestCase):
    def test_native_requests_are_scoped_to_an_exact_turn(self):
        decisions = []
        text_events = []
        def emit(kind,data):
            if kind in ("delta","message.boundary"):
                text_events.append((kind,data))
        def approve(action):
            decisions.append(action)
            return action["params"].get("command") != "test shell command"
        emit.approve = approve
        with tempfile.TemporaryDirectory() as tmp, patch.object(
                codex,"_command", return_value=[sys.executable,"-u","-c",PEER]), patch.object(
                codex.codex_choices,"select",return_value=({},[],{})):
            text,_ = codex.run("probe",tmp,emit,threading.Event(),writable=True,permission_profile="partial")
        self.assertEqual(len(decisions),4)
        self.assertEqual(text_events,[("delta","no LF"),("message.boundary",None)])
        self.assertEqual(decisions[1]["params"]["networkApprovalContext"]["host"],"test.invalid")
        self.assertEqual(decisions[2]["item"]["changes"][0]["diff"],"+test")
        self.assertEqual(json.loads(text),[
            {"decision":"decline"},{"decision":"accept"},{"decision":"accept"},
            {"permissions":{"network":{"enabled":True}},"scope":"turn"},
            {"decision":"decline"},{"decision":"decline"},{"decision":"decline"},
        ])

    def test_absent_consent_cannot_relax_a_sandbox(self):
        def emit(*args):
            pass
        for mode in ("ask","partial"):
            with self.assertRaises(RuntimeError):
                codex._policy(mode,True,emit)
            self.assertEqual(codex._policy(mode,False,emit),("read-only","never"))
        for mode in ("auto","full"):
            with self.assertRaises(RuntimeError):
                codex._policy(mode,False,emit)
            self.assertEqual(codex._policy(mode,True,emit),("danger-full-access","never"))
        with self.assertRaises(RuntimeError):
            codex._policy("typo",True,emit)
        self.assertEqual(codex._policy(None,True,emit),("workspace-write","never"))

    def test_full_native_tools_keep_config_and_only_accept_one_call(self):
        with patch.object(codex,"_executable",return_value="codex"),patch.object(codex.subprocess,"run") as scan:
            cmd = codex._command(".",["-c","model_provider=custom"],native_tools=True)
            scan.assert_not_called()
            self.assertEqual(cmd,["codex","-c","model_provider=custom","app-server","--listen","stdio://"])
        form = {"mode":"form","requestedSchema":{"type":"object","properties":{}},
                "_meta":{"codex_approval_kind":"mcp_tool_call"}}
        self.assertEqual(codex._native_tool_reply("mcpServer/elicitation/request",form,True)["action"],"accept")
        self.assertEqual(codex._native_tool_reply("mcpServer/elicitation/request",form,False)["action"],"decline")
        self.assertEqual(codex._native_tool_reply("mcpServer/elicitation/request",{**form,"mode":"url"},True)["action"],"decline")
