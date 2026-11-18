import os, subprocess, sys
sys.path.insert(0, ".")
from inventory.store import Store
ENV = {**os.environ, "PYTHONIOENCODING": "utf-8"}

store = Store({"zeta": 4, "alpha": 4, "mid": 5})
# Strictly below the threshold, sorted by name.
assert store.low_stock(5) == ["alpha", "zeta"], store.low_stock(5)
assert Store({}).low_stock(1) == []
out = subprocess.run([sys.executable, "-m", "inventory.report", "--threshold", "4"],
                     capture_output=True, text=True, encoding="utf-8", env=ENV).stdout
section = out.split("需要补货", 1)[1]
assert "bolts" in section and "washers" in section, out
assert "anchors" not in section and "nuts" not in section, out
plain = subprocess.run([sys.executable, "-m", "inventory.report"],
                       capture_output=True, text=True, encoding="utf-8", env=ENV).stdout
assert plain.startswith("库存"), plain
print("ok")
