"""Resolve a turn's native scope before starting any worker."""
from pathlib import Path


def scope_for(agent, profile, writable, cwd, emit, cancel, *, workspace_writer=False):
    if profile not in (None, "readonly", "ask", "partial", "full", "auto"):
        raise ValueError("未知的代理权限模式")
    if cancel.is_set():
        raise RuntimeError("任务已取消")
    if profile in ("full", "auto"):
        if not writable:
            raise RuntimeError("完全访问与只读任务授权冲突")
        return "danger-full-access"
    if not writable:
        return "read-only"
    if profile == "readonly":
        raise RuntimeError("只读档不允许写入")
    if not workspace_writer:
        raise RuntimeError(f"{agent} 在当前权限档仅支持只读任务。请将工作区写入交给 Codex 或 DeepSeek Harness。")
    if profile == "ask":
        approve = getattr(emit, "approve", None)
        action = {"agent":agent,"action":"workspace_write","cwd":str(Path(cwd).resolve()),
                  "scope":"current_turn","summary":"允许该代理在本轮修改所选工作区，并使用工作区沙箱执行命令？"}
        if not callable(approve) or approve(action) is not True:
            raise RuntimeError("本轮工作区写入未获批准")
        if cancel.is_set():
            raise RuntimeError("任务已取消")
    return "workspace-write"
