import os, subprocess, sys
sys.path.insert(0, ".")
from inventory.store import Store
ENV = {**os.environ, "PYTHONIOENCODING": "utf-8"}

store = Store({"b": 1, "a": 2, "c": 9})
assert store.low_stock(5) == ["a", "b"], store.low_stock(5)
out = subprocess.run([sys.executable, "-m", "inventory.report", "--threshold", "5"],
                     capture_output=True, text=True, encoding="utf-8", env=ENV)
assert out.returncode == 0, out.stderr
assert "需要补货" in out.stdout, out.stdout
print("ok")
