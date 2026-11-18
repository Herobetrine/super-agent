import sys
sys.path.insert(0, ".")
from slug import slugify

cases = {"Hello World": "hello-world", " Mixed  CASE -- here ": "mixed-case-here",
         "---": "", "Café au lait": "café-au-lait"}
bad = {k: slugify(k) for k, v in cases.items() if slugify(k) != v}
print(bad)
sys.exit(1 if bad else 0)
