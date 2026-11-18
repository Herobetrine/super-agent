import sys
sys.path.insert(0, ".")
from pricing import total

checks = [
    (total([], 25), 0.0),
    (total([(5.0, 1)], 100), 0.0),
    (total([(0.1, 3)], 0), 0.3),
    (total([(100.0, 1), (50.0, 2)], 50), 100.0),
]
bad = [i for i, (got, want) in enumerate(checks) if got != want]
print("failed cases:", bad)
sys.exit(1 if bad else 0)
