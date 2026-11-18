import sys

from textstats.lines import line_stats
from textstats.words import word_count


def main():
    text = sys.stdin.read()
    stats = line_stats(text)
    print(f"lines={stats['lines']} blank={stats['blank']} longest={stats['longest']}")
    for word, n in sorted(word_count(text).items(), key=lambda kv: (-kv[1], kv[0])):
        print(f"{word}={n}")


if __name__ == "__main__":
    main()
