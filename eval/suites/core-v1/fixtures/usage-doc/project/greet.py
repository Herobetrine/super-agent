import argparse


def main(argv=None):
    parser = argparse.ArgumentParser(description="Print a greeting.")
    parser.add_argument("--name", default="world", help="who to greet")
    parser.add_argument("--shout", action="store_true", help="upper-case the greeting")
    args = parser.parse_args(argv)
    text = f"Hello, {args.name}!"
    print(text.upper() if args.shout else text)


if __name__ == "__main__":
    main()
