"""Snapshot explicitly addressed project files by content hash outside the project."""
import hashlib
import os
import tempfile
from pathlib import Path

LIMIT = 64 * 1024 * 1024


def snapshot(project, names, directory):
    if not isinstance(names,list) or len(names)>16:
        raise ValueError("files 必须是最多 16 个项目相对路径")
    root=Path(project).resolve(strict=True)
    directory.mkdir(parents=True,exist_ok=True)
    manifest=[]
    total=0
    for name in names:
        if not isinstance(name,str) or not name or Path(name).anchor:
            raise ValueError("附件只接受项目相对路径")
        source=(root/name).resolve(strict=True)
        source.relative_to(root)
        if not source.is_file():
            raise ValueError("附件必须是普通文件")
        sha=hashlib.sha256()
        size=0
        tmp=None
        try:
            with source.open("rb") as stream, tempfile.NamedTemporaryFile(dir=directory,delete=False) as out:
                tmp=Path(out.name)
                while chunk:=stream.read(262144):
                    size+=len(chunk)
                    total+=len(chunk)
                    if total>LIMIT:
                        raise ValueError("单条消息附件总量超过 64 MiB，请拆分")
                    sha.update(chunk)
                    out.write(chunk)
                out.flush()
                os.fsync(out.fileno())
            key=sha.hexdigest()
            dest=directory/key
            os.replace(tmp,dest)
            manifest.append(dict(name=name,bytes=size,sha256=key,object_path=str(dest.resolve())))
        finally:
            if tmp:
                tmp.unlink(missing_ok=True)
    return manifest
