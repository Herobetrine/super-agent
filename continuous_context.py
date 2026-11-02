"""Bound an agent's working input while preserving its exact original JSON packet."""
import hashlib
import json
import os
from pathlib import Path
import tempfile


_JSON = json.JSONEncoder(ensure_ascii=False, separators=(",", ":"), allow_nan=False)
_CONTROL = ("ticket", "job", "turn", "depth", "agent", "sender", "project", "role",
            "permission_profile", "allow_lead_routing", "allowed_child_permissions",
            "harness_capabilities", "selected_execution", "claude_route",
            "can_assign_write", "this_turn_can_write")
_HISTORY = ("user_updates", "reports", "notes", "roster")
_CURRENT = ("objective", "goal", "assignment", "autonomy", "next_step", "previous_reply", "files", "delegate_models")
_TEXT = ("result", "body", "assignment", "objective", "milestone", "learning", "summary", "text")
_GAP = " […中段省略，见 full_context…] "
_SKIP = object()


def _chunks(value):
    for token in _JSON.iterencode(value):
        for i in range(0, len(token), 65536):
            yield token[i:i + 65536].encode("utf-8")


def _size(value, limit=None):
    if limit is not None and isinstance(value, str) and len(value) > limit:
        return limit + 1
    total = 0
    for chunk in _chunks(value):
        total += len(chunk)
        if limit is not None and total > limit:
            return limit + 1
    return total


def _matches(path, expected_size, expected_hash):
    try:
        if path.is_symlink() or path.stat().st_size != expected_size:
            return False
        digest = hashlib.sha256()
        with path.open("rb") as source:
            while chunk := source.read(262144):
                digest.update(chunk)
        return digest.hexdigest() == expected_hash
    except FileNotFoundError:
        return False


def _archive(packet, store_path):
    root = Path(store_path).resolve()
    directory = root / "context-objects"
    directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    if directory.resolve() != directory:
        raise ValueError("上下文存储目录不能重定向到其他位置")
    digest = hashlib.sha256()
    count = 0
    stream = tempfile.NamedTemporaryFile(dir=directory, prefix=".pending-", delete=False)
    tmp = Path(stream.name)
    try:
        with stream:
            for chunk in _chunks(packet):
                stream.write(chunk)
                digest.update(chunk)
                count += len(chunk)
            stream.flush()
            os.fsync(stream.fileno())
        key = digest.hexdigest()
        dest = directory / (key + ".json")
        if not _matches(dest, count, key):
            try:
                os.replace(tmp, dest)
            except OSError:
                # Another writer may have published the same immutable object
                # while a Windows reader holds the destination open.
                if not _matches(dest, count, key):
                    raise
        return {"object_path": str(dest), "sha256": key, "bytes": count,
            "view": "本轮为节选。历史列表优先保留最新项，长文本标注省略处。需要缺失字段、完整指令或回报时，按路径读取 UTF-8 JSON 原文并核对摘要。"}
    finally:
        tmp.unlink(missing_ok=True)


def _excerpt(text, budget):
    if _size(text, budget) <= budget:
        return text
    if _size(_GAP) + 8 > budget:
        return _SKIP
    # UTF-8 and JSON escapes have variable byte widths. Measure the encoded
    # head/tail pair, leaving the original Unicode code points intact.
    low, high = 0, min(len(text), budget)
    while low < high:
        n = (low + high + 1) // 2
        candidate = text[:(n+1)//2] + _GAP + text[len(text)-n//2:]
        if _size(candidate, budget) <= budget:
            low = n
        else:
            high = n - 1
    return text[:(low+1)//2] + _GAP + text[len(text)-low//2:]


def _field_cost(name):
    # Encoded key, colon and comma; all candidate objects already have fields.
    return _size(name) + 2


def _preview(value, budget):
    if _size(value, budget) <= budget:
        return value
    if isinstance(value, str):
        return _excerpt(value, budget)
    if not isinstance(value, dict):
        return _SKIP
    # A large report still identifies its sender, job and terminal status.
    # Only prose fields can be shortened. Route IDs, roles and scopes stay exact.
    result = {}
    left = budget - 2
    for k, v in value.items():
        if k not in _TEXT:
            cost = _field_cost(k) + _size(v, max(0,left))
            if cost <= left:
                result[k] = v
                left -= cost
    texts = [k for k in _TEXT if isinstance(value.get(k), str)]
    for index, k in enumerate(texts):
        cap = left // (len(texts) - index) - _field_cost(k)
        v = _excerpt(value[k], cap) if cap > 0 else _SKIP
        if v is not _SKIP:
            result[k] = v
            left -= _field_cost(k) + _size(v)
    return result or _SKIP


def compact_packet(packet, guide, store_path, max_bytes=6*1024*1024):
    """Return the same small packet, or a byte-budgeted projection with a verified archive."""
    if not isinstance(packet, dict) or not isinstance(guide, str) or type(max_bytes) is not int:
        raise TypeError("工单输入、guide 或字节上限格式无效")
    budget = max_bytes - len(guide.encode("utf-8")) - 1
    if _size(packet, budget) <= budget and budget >= 0:
        return packet
    ref = _archive(packet, store_path)
    result = {k: packet[k] for k in _CONTROL if k in packet}
    result["full_context"] = ref
    for k in _HISTORY:
        if isinstance(packet.get(k), list):
            result[k] = []
    left = budget - _size(result)
    if left < 0:
        raise ValueError(f"上限不足以容纳完整 guide、权限和上下文引用。原文已保存于 {ref['object_path']}")

    # Short current fields fit first. A single huge instruction cannot crowd
    # out a recent report or the newest human correction.
    candidates = [(k, packet[k], False) for k in _CURRENT if k in packet]
    candidates += [(k, packet[k][-1], True) for k in _HISTORY if result.get(k) == [] and packet[k]]
    candidates.sort(key=lambda item: _size(item[1], left))
    for i, (k, v, history) in enumerate(candidates):
        overhead = 0 if history else _field_cost(k)
        cap = left // (len(candidates)-i) - overhead
        if cap <= 0:
            continue
        part = _preview(v, cap)
        if part is not _SKIP:
            result[k] = [part] if history else part
            left -= overhead + _size(part)

    # Fill the balance with exact older rows, newest first within each list.
    for k in _HISTORY:
        if not result.get(k):
            continue
        older = []
        rows = packet[k]
        for i in range(len(rows)-2, -1, -1):
            cost = 1 + _size(rows[i], left)
            if cost > left:
                part = _preview(rows[i], left - 1) if left > 1 else _SKIP
                if part is not _SKIP:
                    older.append(part)
                    left -= 1 + _size(part)
                break
            older.append(rows[i])
            left -= cost
        result[k][:0] = reversed(older)
    for k, v in packet.items():
        if k in result or k in _HISTORY:
            continue
        cost = _field_cost(k) + _size(v, left)
        if cost <= left:
            result[k] = v
            left -= cost
    return result
