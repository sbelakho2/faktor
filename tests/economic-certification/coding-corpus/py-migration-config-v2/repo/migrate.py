"""Migrate a v1 device config to v2."""
import json
import sys


def migrate(old):
    if old.get("version") == 2:
        return old
    return {
        "version": 2,
        "endpoint": {"host": old["host"], "port": old["port"]},
        "user": old["user_name"],
        "tls": old.get("tls", True),
    }


def main():
    src, dst = sys.argv[1], sys.argv[2]
    with open(src, "r", encoding="utf-8") as fh:
        old = json.load(fh)
    with open(dst, "w", encoding="utf-8") as fh:
        json.dump(migrate(old), fh, indent=2, sort_keys=True)
        fh.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
