import subprocess, sys
sys.path.insert(0, ".")
from textstats.words import word_count
from textstats.lines import line_stats

assert word_count("a b  a\nB") == {"a": 2, "b": 2}, word_count("a b  a\nB")
assert line_stats("x\n\nyy\n") == {"lines": 3, "blank": 1, "longest": 2}, line_stats("x\n\nyy\n")
out = subprocess.run([sys.executable, "-m", "textstats.cli"], input="one two\ntwo\n",
                     capture_output=True, text=True, encoding="utf-8")
assert out.returncode == 0, out.stderr
assert out.stdout.strip().splitlines() == ["lines=2 blank=0 longest=7", "two=2", "one=1"], out.stdout
print("ok")
