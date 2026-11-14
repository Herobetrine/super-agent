#!/usr/bin/env python3
"""Control the existing lock-owning desktop through its same-user local spool.

No database access, executable launching, network listening or model calls.
The data directory must be owned by the same trusted OS user as the desktop.
"""
import argparse
import json
import os
from pathlib import Path
import sys
import tempfile
import time
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data", type=Path, default=Path(os.environ.get("SUPERAGENT_DATA", Path.home() / ".local/share/superagent/control-room")))
    parser.add_argument("--timeout", type=float, default=10.0)
    parser.add_argument("command", choices=["status", "pause-after-handoff", "resume"])
    parser.add_argument("--ticket", type=int, required=True)
    args = parser.parse_args()
    if args.ticket <= 0 or not 0 < args.timeout <= 300:
        parser.error("ticket must be positive and timeout must be within (0, 300]")
    control = args.data / "control"
    try:
        ready = json.loads((control / "ready.json").read_text(encoding="utf-8"))
        if ready.get("protocol") != "super-agent-local-control" or ready.get("version") != 1:
            raise ValueError("unsupported local control protocol")
        request_id = uuid.uuid4().hex
        request = {"generation": ready["generation"], "version": 1, "id": request_id, "command": args.command.replace("-", "_"), "ticket": args.ticket}
        # Temporary files live outside requests so the bounded server scan sees only requests.
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=control, suffix=".tmp", delete=False) as temporary:
            temporary_path = Path(temporary.name)
            json.dump(request, temporary)
            temporary.flush()
            os.fsync(temporary.fileno())
        try:
            os.replace(temporary_path, control / "requests" / f"{request_id}.json")
        finally:
            temporary_path.unlink(missing_ok=True)
        response_path = control / "responses" / f"{request_id}.json"
        deadline = time.monotonic() + args.timeout
        while time.monotonic() < deadline:
            if response_path.exists():
                response = json.loads(response_path.read_text(encoding="utf-8"))
                if response.get("id") != request_id or response.get("version") != 1:
                    raise ValueError("invalid local control response")
                print(json.dumps(response, ensure_ascii=False, indent=2))
                return 0 if response.get("ok") is True else 1
            time.sleep(0.1)
        print("Local control response timed out. The request may still execute. Query status before retrying.", file=sys.stderr)
        return 2
    except (OSError, ValueError):
        print("Local control unavailable or invalid response. Verify the desktop and data directory.", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
