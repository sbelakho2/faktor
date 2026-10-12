#!/bin/sh
set -eu
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cat > "$work/old.json" <<'EOF'
{"host": "db.internal", "user_name": "ada"}
EOF
python3 migrate.py "$work/old.json" "$work/new.json"
python3 - "$work/new.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as fh:
    got = json.load(fh)
want = {
    "version": 2,
    "endpoint": {"host": "db.internal", "port": 443},
    "user": "ada",
    "tls": False,
}
assert got == want, f"migration mismatch: {got!r}"
print("ok")
PY
cp "$work/new.json" "$work/before.json"
python3 migrate.py "$work/new.json" "$work/new.json"
cmp -s "$work/new.json" "$work/before.json" || {
  echo "migration is not idempotent" >&2
  exit 1
}
cat > "$work/with_tls.json" <<'EOF'
{"host": "a.example", "user_name": "bob", "port": 8443, "tls": true}
EOF
python3 migrate.py "$work/with_tls.json" "$work/with_tls_v2.json"
python3 - "$work/with_tls_v2.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as fh:
    got = json.load(fh)
assert got["endpoint"] == {"host": "a.example", "port": 8443}, got
assert got["tls"] is True, got
print("ok")
PY
