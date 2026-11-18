"""Plain text inventory report."""
import argparse

from inventory.store import Store

SAMPLE = {"bolts": 3, "nuts": 12, "washers": 0, "anchors": 7}


def render(store, threshold=None):
    lines = ["库存"]
    for name in sorted(store.items):
        lines.append(f"{name}: {store.items[name]}")
    if threshold is not None:
        lines.append("需要补货")
        lines.extend(f"- {name}" for name in store.low_stock(threshold))
    return "\n".join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("--threshold", type=int)
    args = parser.parse_args(argv)
    print(render(Store(SAMPLE), args.threshold))


if __name__ == "__main__":
    main()
