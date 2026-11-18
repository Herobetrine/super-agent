import subprocess, sys
sys.path.insert(0, ".")
from textstats.words import word_count
from textstats.lines import line_stats

assert word_count("") == {}
assert word_count("Hello, hello!") == {"hello": 2}, word_count("Hello, hello!")
assert line_stats("") == {"lines": 0, "blank": 0, "longest": 0}
assert line_stats("  \nabc") == {"lines": 2, "blank": 1, "longest": 3}
out = subprocess.run([sys.executable, "-m", "textstats.cli"], input="b a\na\n",
                     capture_output=True, text=True, encoding="utf-8").stdout.strip().splitlines()
# Ties are ordered by word.
assert out == ["lines=2 blank=0 longest=3", "a=2", "b=1"], out
print("ok")
