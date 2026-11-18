import os, sys, tempfile
sys.path.insert(0, ".")
from csvsum import summarize

with tempfile.TemporaryDirectory() as d:
    empty = os.path.join(d, "empty.csv")
    with open(empty, "w", encoding="utf-8") as f:
        f.write("name,amount\n")
    assert summarize(empty) == {"rows": 0, "total": 0.0}, summarize(empty)
    quoted = os.path.join(d, "quoted.csv")
    with open(quoted, "w", encoding="utf-8") as f:
        f.write('amount,name\n2,"a, b"\n3,c\n')
    assert summarize(quoted) == {"rows": 2, "total": 5.0}, summarize(quoted)
print("ok")
