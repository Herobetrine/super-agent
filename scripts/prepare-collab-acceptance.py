"""Prepare one acceptance run: a small git project, a release-policy check and a scenario file.

Usage: python scripts/prepare-collab-acceptance.py <deps|deps-alias|queue|checks|bounce|ask> <new-run-dir> --agent antigravity --model gemini-4-hydrogen-high --provider google
"""
import argparse
import json
import pathlib
import subprocess
import sys

FILES = {
    ".gitignore": "__pycache__/\n*.pyc\n",
    "README.md": """# textkit

Small text helpers.

- `slugify(text)`: lower-case, hyphen-separated identifier.

## Scripts

- `python scripts/export_csv.py <in.txt> <out.csv>` writes one slug per input line as CSV.
- `python scripts/export_tsv.py <in.txt> <out.tsv>` writes one slug per input line as TSV.

## Tests

    python -m unittest discover -s tests -q
""",
    "CHANGELOG.md": """# Changelog

## Unreleased

- `slugify`: collapse runs of separators.
""",
    "textkit/__init__.py": '''"""Small text helpers."""

from .slug import slugify

__all__ = ["slugify"]
''',
    "textkit/slug.py": '''"""Slugs."""

import re

_SEPARATORS = re.compile(r"[^a-z0-9]+")


def slugify(text: str) -> str:
    """Return a lower-case identifier with words joined by hyphens."""
    return _SEPARATORS.sub("-", text.lower()).strip("-")
''',
    "tests/__init__.py": "",
    "tests/test_slug.py": '''import unittest

from textkit import slugify


class SlugTest(unittest.TestCase):
    def test_words_are_joined_by_hyphens(self) -> None:
        self.assertEqual(slugify("Hello, World!"), "hello-world")

    def test_separator_runs_collapse(self) -> None:
        self.assertEqual(slugify("a  --  b"), "a-b")


if __name__ == "__main__":
    unittest.main()
''',
    "scripts/export_csv.py": '''"""Write one slug per input line as CSV."""

import csv
import sys

from textkit import slugify


def main(source: str, target: str) -> None:
    with open(source, encoding="utf-8") as lines, open(target, "w", encoding="utf-8", newline="") as out:
        writer = csv.writer(out)
        for line in lines:
            writer.writerow([line.strip(), slugify(line)])


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
''',
    "scripts/export_tsv.py": '''"""Write one slug per input line as TSV."""

import csv
import sys

from textkit import slugify


def main(source: str, target: str) -> None:
    with open(source, encoding="utf-8") as lines, open(target, "w", encoding="utf-8", newline="") as out:
        writer = csv.writer(out, delimiter="\\t")
        for line in lines:
            writer.writerow([line.strip(), slugify(line)])


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
''',
}

# Lives outside the project: the operator's release rule, which the repository does not state.
POLICY = '''"""Release policy: every name exported by textkit has a CHANGELOG entry."""
import ast
import pathlib
import sys

root = pathlib.Path.cwd()
tree = ast.parse((root / "textkit" / "__init__.py").read_text(encoding="utf-8"))
names = []
for node in ast.walk(tree):
    if isinstance(node, ast.Assign) and any(getattr(t, "id", None) == "__all__" for t in node.targets):
        names = [e.value for e in node.value.elts]
changelog = (root / "CHANGELOG.md").read_text(encoding="utf-8")
missing = [n for n in names if f"`{n}`" not in changelog]
if missing:
    print("release policy: CHANGELOG.md has no entry for exported name(s): " + ", ".join(missing))
    print("Add a line under '## Unreleased' that names each one in backticks.")
    sys.exit(1)
print("release policy ok")
'''

SCENARIOS = {
    "deps-alias": {
        "goal": "这是一次平台协作控制的定向真实验证。给 textkit 增加 `truncate_words(text, limit, suffix='…')`，"
        "按空白分词，结果用单个空格连接，超出 limit 个单词时截断并追加 suffix。第一阶段 limit 只需支持正数。"
        "实现放在 textkit/truncate.py 并从包导出，补测试与变更记录。"
        "请用 SA_SEND 消息 id 为 impl 派一个 antigravity 写入协作者实现；"
        "同轮再用 SA_SEND 消息 id 为 review-initial、write:false、after:[\"impl\"] 派另一个独立审查者。"
        "审查者只读审查并运行测试。主控核对实际回执和产物后结束第一阶段，等待追加需求。",
        "follow": [
            "现在追加边界需求：limit 为 0 返回空字符串且不追加 suffix，负数抛 ValueError，"
            "请让刚才 impl 实现者在原会话接着修改代码和测试。"
            "为了验证续派消息 ID 别名，这轮必须使用 SA_SEND id 为 resume-impl、continue 为该实现者任务编号、"
            "body 写明需求；该续派控制只写 id、continue、body 三个字段。"
            "再派一个 NEW 独立只读审查任务，SA_SEND id 为 review-followup，to:antigravity，write:false，"
            "after:[\"resume-impl\"]。after 必须用这个续派消息 ID 字符串。"
            "审查新的零和负数边界，跑完整测试。确认真实续派及这次复审都完成才汇总。"
        ],
        "required_events": ["continued"],
        "required_job_keys": ["impl", "review-initial", "review-followup"],
        "required_continuation_keys": ["resume-impl"],
        "checks": [["python", "-m", "unittest", "discover", "-s", "tests", "-q"]],
    },
    "queue": {
        "goal": "这是一次隔离写入及候选验证队列的定向真实验证。请只派一个 antigravity 写入协作者，"
        "SA_SEND 消息 id 为 queue-impl。它在 execution_directory 的隔离工作树中实现 textkit.reverse_words(text)，"
        "按空白分词、倒序并以单个空格连接，空白输入返回空字符串。实现放在 textkit/reverse.py，"
        "从包导出，新增至少三个单元测试并更新 CHANGELOG.md。"
        "主控自身不要修改原项目，也不要直接执行 git merge、cherry-pick 或移除工作树。"
        "工作完成后平台自动验证候选，fixture 操作者将另外确认合入。"
        "主控核对隔离任务结果并汇总完成和未核对内容，不要等待自己执行合入，也不要再派其他写入任务。",
        "checks": [["python", "-m", "unittest", "discover", "-s", "tests", "-q"]],
        "isolated_writers": True,
        "integration_queue": True,
        "confirm_integration": True,
        "required_events": ["integration.result", "integration.confirmed"],
        "required_job_keys": ["queue-impl"],
    },
    "deps": {
        "goal": "给 textkit 增加 `truncate_words(text, limit, suffix='…')`：按单词截断到最多 limit 个单词，"
        "发生截断时追加 suffix。放在 `textkit/truncate.py` 并从包里导出，补上单元测试。"
        "这个改动请派一个协作者实现，再让另一个协作者独立审查实现和测试（审查者不改文件）；"
        "审查发现的问题要修掉。完成后告诉我结果。",
        "required_events": ["continued"],
        "follow": ["需求改一下：`limit` 为 0 时返回空字符串且不加 suffix，负数抛 `ValueError`。让刚才的实现者接着改，并更新测试。"],
    },
    "checks": {
        "goal": "给 textkit 增加 `title_case(text)`：每个单词首字母大写，但 a、an、the、of、and、in、on "
        "这些小词除了在句首以外保持小写。放在 `textkit/title.py`，从包里导出，补上单元测试。"
        "这个任务很小，你自己直接做，不用派工。",
        "checks": [["python", "-m", "unittest", "discover", "-s", "tests", "-q"], ["python", "{run}/policy_check.py"]],
    },
    "bounce": {
        "required_events": ["check.bounced"],
        "goal": "给 textkit 增加 `title_case(text)`：每个单词首字母大写，但 a、an、the、of、and、in、on "
        "这些小词除了在句首以外保持小写。放在 `textkit/title.py`，从包里导出，补上单元测试。"
        "这个任务很小，你自己直接做，不用派工。"
        " 这是分阶段交付验证：第一轮只修改功能代码、导出和测试，暂不修改 CHANGELOG.md，"
        "也不要读取或运行工作区外的 policy_check.py。平台会在你本轮提交完成后运行发布检查；"
        "如果收到失败反馈，下一轮再按反馈补齐相关文件。",
        "checks": [["python", "-m", "unittest", "discover", "-s", "tests", "-q"], ["python", "{run}/policy_check.py"]],
    },
    "ask": {
        "goal": "scripts 目录里的 `export_csv.py` 和 `export_tsv.py` 功能重复。把我已经不用的那个删掉，README 里对应的说明也去掉。",
        "required_events": ["ask", "answer"],
        "answers": ["我不用的是 export_tsv.py。"],
    },
}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("scenario", choices=SCENARIOS)
    parser.add_argument("run", type=pathlib.Path)
    parser.add_argument("--agent", choices=("claude", "antigravity"), required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--provider")
    parser.add_argument("--timeout", type=int, default=900)
    args = parser.parse_args()
    if args.scenario in {"deps-alias", "queue"} and (
        args.agent != "antigravity" or args.provider != "google" or args.model != "gemini-4-hydrogen-high"
    ):
        parser.error("follow-up fixtures require exact antigravity / google / gemini-4-hydrogen-high")
    name, run = args.scenario, args.run.resolve()
    run.mkdir(parents=True)
    project = run / "project"
    for rel, body in FILES.items():
        target = project / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(body, encoding="utf-8", newline="\n")
    (run / "policy_check.py").write_text(POLICY, encoding="utf-8", newline="\n")
    git = ["git", "-C", str(project), "-c", "user.name=acceptance", "-c", "user.email=acceptance@example.invalid"]
    subprocess.run(["git", "init", "-q", "-b", "main", str(project)], check=True)
    subprocess.run(git + ["config", "user.name", "acceptance"], check=True)
    subprocess.run(git + ["config", "user.email", "acceptance@example.invalid"], check=True)
    subprocess.run(git + ["add", "-A"], check=True)
    subprocess.run(git + ["commit", "-q", "-m", "Initial textkit"], check=True)
    scenario = json.loads(json.dumps(SCENARIOS[name]).replace("{run}", run.as_posix()))
    scenario.update(project=str(project), ledger=str(run / "ledger"), agent=args.agent, model=args.model, timeout_s=args.timeout)
    if args.provider:
        scenario["provider"] = args.provider
    (run / "scenario.json").write_text(json.dumps(scenario, ensure_ascii=False, indent=2), encoding="utf-8")
    print(run / "scenario.json")


if __name__ == "__main__":
    main()
