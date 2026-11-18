import sys
sys.path.insert(0, ".")
from slug import slugify

cases = {"Hello World": "hello-world", "a b c": "a-b-c"}
if "--strict" in sys.argv:
    cases.update({"  Trim me  ": "trim-me", "a -- b": "a-b", "--x--": "x"})
bad = {k: slugify(k) for k, v in cases.items() if slugify(k) != v}
print(bad)
sys.exit(1 if bad else 0)
