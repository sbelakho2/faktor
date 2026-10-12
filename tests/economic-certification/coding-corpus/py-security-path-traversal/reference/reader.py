"""Read a named file from the workspace data directory."""
import os
import sys

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "data")


def resolve(name):
    """Resolve `name` inside ROOT, refusing absolute and escaping paths."""
    if os.path.isabs(name):
        raise ValueError("absolute paths are not allowed")
    root = os.path.realpath(ROOT)
    candidate = os.path.realpath(os.path.join(root, name))
    if candidate != root and not candidate.startswith(root + os.sep):
        raise ValueError("path escapes the data directory")
    return candidate


def main():
    if len(sys.argv) != 2:
        print("usage: reader.py <name>", file=sys.stderr)
        return 2
    try:
        path = resolve(sys.argv[1])
    except ValueError as exc:
        print(f"refused: {exc}", file=sys.stderr)
        return 1
    if not os.path.isfile(path):
        print(f"not found: {sys.argv[1]}", file=sys.stderr)
        return 1
    with open(path, "r", encoding="utf-8") as fh:
        sys.stdout.write(fh.read())
    return 0


if __name__ == "__main__":
    sys.exit(main())
