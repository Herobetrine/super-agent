"""Binary stdio, cooperative cancellation, and child-tree cleanup."""
import os
import subprocess
import threading
from collections import deque

CREATE_FLAGS = getattr(subprocess, "CREATE_NO_WINDOW", 0)


def stop_tree(p):
    if p.poll() is not None:
        return
    if os.name == "nt":
        subprocess.run(["taskkill", "/PID", str(p.pid), "/T", "/F"],
                       capture_output=True, creationflags=CREATE_FLAGS, timeout=15)
    else:
        p.terminate()
    if p.poll() is None:
        p.kill()


def stream_lines(argv, cwd, stdin, emit_line, cancel, *, on_stderr=None, env=None):
    if cancel.is_set():
        raise RuntimeError("任务已取消")
    p = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.PIPE, creationflags=CREATE_FLAGS)
    ended = threading.Event()
    errors = deque(maxlen=12)
    faults = []

    def watch():
        while not ended.wait(0.1):
            if cancel.is_set():
                stop_tree(p)
                return

    def send():
        try:
            with p.stdin:
                p.stdin.write(stdin)
        except (BrokenPipeError, OSError):
            pass

    def diagnostics():
        try:
            for line in p.stderr:
                errors.append(line[-512:].decode("utf-8", "replace").strip())
                if on_stderr:
                    on_stderr(line)
        except Exception as exc:
            faults.append(exc)
            stop_tree(p)

    threads = [threading.Thread(target=f, daemon=True) for f in (watch, send, diagnostics)]
    for thread in threads:
        thread.start()
    try:
        for line in p.stdout:
            emit_line(line)
        status = p.wait()
        threads[-1].join()
        if cancel.is_set():
            raise RuntimeError("任务已取消")
        if faults:
            raise faults[0]
        if status:
            raise RuntimeError(f"{os.path.basename(argv[0])} exit {status} " + " ".join(errors))
    finally:
        stop_tree(p)
        ended.set()
        for thread in threads:
            thread.join(timeout=2)
        p.stdout.close()
        p.stderr.close()
