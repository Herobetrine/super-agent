import sys
sys.path.insert(0, ".")
from csvsum import summarize

got = summarize("data/sample.csv")
assert got == {"rows": 2, "total": 7.5}, got
print("ok")
