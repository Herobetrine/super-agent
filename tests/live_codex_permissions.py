"""Explicit, billable Windows Codex permission smoke test, using disposable files."""
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import json
import os
import secrets
import sys
import tempfile
import threading

sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
from adapters.codex import run


def probe(base, case):
    profile, outcome, outside = case
    directory = base / f"{profile}-{outcome}"
    project = directory / "project"
    project.mkdir(parents=True)
    target = (directory if outside else project) / "canary.txt"
    token = secrets.token_hex(10)
    command = f"Set-Content -LiteralPath '{target}' -Value '{token}' -NoNewline -Encoding ascii"
    escalation = profile == "ask" or (profile == "partial" and outside)
    receipt = {"profile":profile,"case":outcome,"outside":outside,"requests":[]}

    def emit(kind, data):
        if kind == "permission.effective":
            receipt["native"] = data

    def approve(action):
        params = action["params"]
        # This verifier can approve only one exact PowerShell invocation that
        # writes a random token in its own new disposable directory.
        actual = params.get("command","")
        exe, sep, arg = actual.partition(" -Command ")
        # The app-server presentation may quote the executable and/or its
        # argument. No other parameters/suffixes are accepted by this verifier.
        expected = {command,'"'+command+'"', '"'+command.replace("\\","\\\\")+'"'}
        match = (sep and Path(exe.strip('"')).name.lower() in ("pwsh","pwsh.exe","powershell","powershell.exe")
                 and arg in expected)
        allowed = bool(outcome == "accept" and escalation and
                       action["method"] == "item/commandExecution/requestApproval" and match)
        receipt["requests"].append({"method":action["method"],"exact":bool(match),"accepted":allowed,
                                    "presentation":actual if not match else ""})
        return allowed

    emit.approve = approve
    prompt = ("Use only the exec_command tool with PowerShell and run this exact command once:\n"
              + command + "\n")
    if escalation:
        prompt += "Set sandbox_permissions=require_escalated with a brief justification. "
    prompt += "If denied, stop without retrying. Finally report the outcome."
    cancel = threading.Event()
    timer = threading.Timer(95,cancel.set)
    timer.daemon = True
    timer.start()
    try:
        selection = json.loads(os.environ["SUPERAGENT_TEST_CODEX_SELECTION"]) if "SUPERAGENT_TEST_CODEX_SELECTION" in os.environ else None
        run(prompt,str(project),emit,cancel,writable=True,permission_profile=profile,selection=selection)
        receipt["completed"] = True
    except RuntimeError as exc:
        receipt["error"] = str(exc)
    finally:
        timer.cancel()
    receipt["exists"] = target.exists()
    receipt["exact_bytes"] = target.is_file() and target.read_bytes() == token.encode()
    requests = receipt["requests"]
    if outcome == "deny":
        receipt["pass"] = bool(requests) and not receipt["exists"] and not any(r["accepted"] for r in requests)
    else:
        receipt["pass"] = receipt["exact_bytes"] and (
            any(r["accepted"] for r in requests) if escalation else not requests
        )
    print(profile,outcome,receipt["pass"],flush=True)
    return receipt


if __name__ == "__main__":
    parent = Path.home() / ".local/share/superagent/checks"
    parent.mkdir(parents=True,exist_ok=True)
    base = Path(tempfile.mkdtemp(prefix="codex-permissions-",dir=parent))
    print(base,flush=True)
    cases = [("ask","accept",False),("ask","deny",False),("partial","project",False),
             ("partial","accept",True),("full","outside",True),("auto","outside",True)]
    with ThreadPoolExecutor(max_workers=2) as pool:
        results = list(pool.map(lambda case:probe(base,case),cases))
    (base / "receipt.json").write_text(json.dumps(results,ensure_ascii=False,indent=2),encoding="utf-8")
    sys.exit(0 if all(r["pass"] for r in results) else 1)
