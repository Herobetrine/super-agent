import csv


def summarize(path):
    """Return {"rows": n, "total": sum of the amount column} for a CSV with a header."""
    rows = 0
    total = 0.0
    with open(path, newline="", encoding="utf-8") as f:
        for row in csv.DictReader(f):
            rows += 1
            total += float(row["amount"])
    return {"rows": rows, "total": total}
