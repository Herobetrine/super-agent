import sys
sys.path.insert(0, ".")
from pricing import total

cases = [
    (([(10.0, 2)], 0), 20.0),
    (([(10.0, 2)], 10), 18.0),
    (([(19.99, 3)], 15), 50.97),
]
failed = 0
for (items, discount), expected in cases:
    got = total(items, discount)
    if got != expected:
        print(f"total({items!r}, {discount}) = {got!r}, expected {expected!r}")
        failed += 1
sys.exit(1 if failed else 0)
