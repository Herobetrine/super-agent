import re, subprocess, sys

doc = open("docs/USAGE.md", encoding="utf-8").read()
for flag in ("--name", "--shout"):
    assert flag in doc, f"undocumented {flag}"
usage = doc.split("## 用法", 1)[1].split("## 示例", 1)[0]
assert "--shout" in usage and "--name" in usage, "用法一节要说明全部参数"
examples = re.findall(r"```(?:console|bash|sh|text)?\n(.*?)```", doc.split("## 示例", 1)[1], re.S)
assert len(examples) >= 1
for block in examples:
    lines = block.strip().splitlines()
    cmd = lines[0].lstrip("$ ").split()
    out = subprocess.run([sys.executable] + cmd[1:], capture_output=True, text=True).stdout.strip()
    assert out == "\n".join(lines[1:]).strip(), (cmd, out)
print("ok")
