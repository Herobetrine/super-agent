"""Plain text inventory report."""
import argparse

from inventory.store import Store

SAMPLE = {"bolts": 3, "nuts": 12, "washers": 0, "anchors": 7}


def render(store):
    lines = ["库存"]
    for name in sorted(store.items):
        lines.append(f"{name}: {store.items[name]}")
    return "\n".join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.parse_args(argv)
    print(render(Store(SAMPLE)))


if __name__ == "__main__":
    main()
