import re, shlex, subprocess, sys

doc = open("docs/USAGE.md", encoding="utf-8").read()
for heading in ("## 安装", "## 用法", "## 示例"):
    assert heading in doc, f"missing {heading}"
example = doc.split("## 示例", 1)[1]
blocks = re.findall(r"```(?:console|bash|sh|text)?\n(.*?)```", example, re.S)
assert blocks, "示例需要代码块"
lines = blocks[0].strip().splitlines()
command = lines[0].lstrip("$ ").strip()
expected = "\n".join(lines[1:]).strip()
argv = shlex.split(command)
assert argv[0] == "python", command
out = subprocess.run([sys.executable] + argv[1:], capture_output=True, text=True).stdout.strip()
assert out == expected, (out, expected)
print("ok")
