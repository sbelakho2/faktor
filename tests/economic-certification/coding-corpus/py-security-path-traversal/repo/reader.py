"""Read a named file from the workspace data directory."""
import os
import sys

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "data")


def resolve(name):
    return os.path.join(ROOT, name)


def main():
    if len(sys.argv) != 2:
        print("usage: reader.py <name>", file=sys.stderr)
        return 2
    path = resolve(sys.argv[1])
    if not os.path.isfile(path):
        print(f"not found: {sys.argv[1]}", file=sys.stderr)
        return 1
    with open(path, "r", encoding="utf-8") as fh:
        sys.stdout.write(fh.read())
    return 0


if __name__ == "__main__":
    sys.exit(main())
