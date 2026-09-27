#!/usr/bin/env bash
# License / bans / sources policy gate (audit items 20-21).
#
# Single source of truth: root `deny.toml` (cargo-deny v2 format). This
# script evaluates the SAME policy against `cargo metadata --locked` with
# python3, so the trusted CI static lane enforces it without installing
# cargo-deny; when a cargo-deny binary IS on PATH the script additionally
# runs `cargo deny --offline check licenses bans sources` and requires it
# to pass, so the two engines cannot silently drift.
#
# Checks:
#   1. every workspace member resolves to license Apache-2.0 (via
#      `license.workspace = true`) and declares no license-file;
#   2. every external package's SPDX expression is satisfied by the
#      `[licenses] allow` list (OR = one allowed branch, AND = all);
#   3. every package source is an allowed registry (no unknown registries,
#      no git/other sources given the empty `[sources] allow-git`);
#   4. no workspace dependency on an EXTERNAL crate uses a `*` requirement
#      (path edges are version-less by design and excluded);
#   5. root LICENSE/NOTICE exist (the Apache-2.0 text is pinned by the
#      static-authority scan).
#
# Exit 0 only when every check passes; every violation is printed.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 2

command -v python3 >/dev/null 2>&1 || {
    echo "check-licenses: python3 is required (cargo metadata post-processing)" >&2
    exit 2
}

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/faktor-licenses.XXXXXX")" || exit 2
trap 'rm -rf "$TMP_DIR"' EXIT

if ! cargo metadata --format-version 1 --locked >"$TMP_DIR/metadata.json" 2>"$TMP_DIR/metadata.err"; then
    echo "check-licenses: cargo metadata --locked failed:" >&2
    sed 's/^/  /' "$TMP_DIR/metadata.err" >&2
    exit 2
fi

python3 - "$ROOT" "$TMP_DIR/metadata.json" <<'PY'
import json
import pathlib
import re
import sys
import tomllib

root = pathlib.Path(sys.argv[1])
metadata_path = pathlib.Path(sys.argv[2])
deny = tomllib.loads((root / "deny.toml").read_text(encoding="utf-8"))
meta = json.loads(metadata_path.read_text(encoding="utf-8"))

violations = []


def normalize(lic: str) -> str:
    return " ".join(lic.upper().split())


# ---------------------------------------------------------------- policy --
allow = set()
for entry in deny.get("licenses", {}).get("allow", []):
    if isinstance(entry, str):
        allow.add(normalize(entry))
    elif isinstance(entry, dict):
        name = normalize(entry["name"])
        exceptions = entry.get("exceptions", [])
        if exceptions:
            for exception in exceptions:
                allow.add(f"{name} WITH {normalize(exception)}")
        else:
            allow.add(name)
    else:
        violations.append(f"deny.toml [licenses] allow entry has unknown shape: {entry!r}")

token_re = re.compile(r"\s*(\(|\)|AND|OR|WITH|/|[A-Za-z0-9.+-]+)")


def parse_expression(expression: str):
    tokens = []
    pos = 0
    while pos < len(expression):
        match = token_re.match(expression, pos)
        if match is None:
            return None, {f"unparseable SPDX expression {expression!r}"}
        tokens.append(match.group(1).upper() if match.group(1).isalpha() else match.group(1))
        pos = match.end()
    index = 0

    def peek():
        return tokens[index] if index < len(tokens) else None

    def take():
        nonlocal index
        value = peek()
        index += 1
        return value

    def primary():
        token = take()
        if token == "(":
            node, missing = expr()
            if take() != ")":
                return None, {f"unbalanced parentheses in {expression!r}"}
        elif token is None or token in (")", "AND", "OR", "WITH", "/"):
            return None, {f"unexpected token in {expression!r}"}
        else:
            node, missing = ("lic", token), set()
        if peek() == "WITH":
            take()
            exception = take()
            if node is None or node[0] != "lic" or exception is None or exception in (")", "AND", "OR", "WITH", "/"):
                return None, {f"malformed WITH clause in {expression!r}"}
            node = ("lic", f"{node[1]} WITH {exception}")
        return node, missing

    def term():
        node, missing = primary()
        if node is None:
            return None, missing
        while peek() == "AND":
            take()
            right, right_missing = primary()
            if right is None:
                return None, right_missing
            node, missing = ("and", node, right), missing | right_missing
        return node, missing

    def expr():
        node, missing = term()
        if node is None:
            return None, missing
        while peek() in ("OR", "/"):
            take()
            right, right_missing = term()
            if right is None:
                return None, right_missing
            node, missing = ("or", node, right), missing | right_missing
        return node, missing

    node, missing = expr()
    if node is None or index != len(tokens):
        return None, missing or {f"unparseable SPDX expression {expression!r}"}
    return node, missing


def evaluate(node):
    kind = node[0]
    if kind == "lic":
        return normalize(node[1]) in allow, {normalize(node[1])}
    if kind == "or":
        left_ok, left_missing = evaluate(node[1])
        right_ok, right_missing = evaluate(node[2])
        if left_ok or right_ok:
            return True, set()
        return False, left_missing | right_missing
    left_ok, left_missing = evaluate(node[1])
    right_ok, right_missing = evaluate(node[2])
    if left_ok and right_ok:
        return True, set()
    return False, left_missing | right_missing


packages = meta.get("packages", [])
workspace = [package for package in packages if package.get("source") is None]
external = [package for package in packages if package.get("source") is not None]

# 1. workspace license policy.
for package in workspace:
    license_value = package.get("license")
    if normalize(license_value or "") != "APACHE-2.0":
        violations.append(
            f"workspace member {package['name']} must resolve to Apache-2.0 "
            f"(license.workspace = true); found {license_value!r}"
        )
    if package.get("license_file"):
        violations.append(
            f"workspace member {package['name']} declares license-file {package['license_file']!r}; "
            "use the workspace SPDX license"
        )

# 2. license expressions.
for package in external:
    license_value = package.get("license")
    if not license_value:
        violations.append(
            f"{package['name']} {package['version']}: no declared license "
            f"(license-file={package.get('license_file')!r})"
        )
        continue
    node, missing = parse_expression(license_value)
    if node is None:
        violations.append(f"{package['name']} {package['version']}: {'; '.join(sorted(missing))}")
        continue
    ok, missing = evaluate(node)
    if not ok:
        violations.append(
            f"{package['name']} {package['version']}: license {license_value!r} is not allowed "
            f"(unmet: {', '.join(sorted(missing))})"
        )

# 3. sources.
sources_cfg = deny.get("sources", {})
allowed_registries = {url.rstrip("/") for url in sources_cfg.get("allow-registry", [])}
allowed_git = sources_cfg.get("allow-git", [])
if sources_cfg.get("unknown-registry", "warn") != "deny":
    violations.append("deny.toml [sources] unknown-registry must be \"deny\"")
if sources_cfg.get("unknown-git", "warn") != "deny":
    violations.append("deny.toml [sources] unknown-git must be \"deny\"")
for package in external:
    source = package.get("source") or ""
    if source.startswith("registry+"):
        url = source[len("registry+"):].rstrip("/")
        if url not in allowed_registries:
            violations.append(f"{package['name']} {package['version']}: unknown registry {url!r}")
    elif source.startswith("git+"):
        url = source[len("git+"):]
        if not any(url == entry or url.startswith(entry.rstrip("/") + "/") or url.startswith(entry.rstrip("/") + "?") for entry in allowed_git):
            violations.append(f"{package['name']} {package['version']}: git source {url!r} is not allowlisted")
    elif source:
        violations.append(f"{package['name']} {package['version']}: unknown source kind {source!r}")

# 4. wildcard requirements on external deps only.
for package in workspace:
    for dependency in package.get("dependencies", []):
        if dependency.get("req") == "*" and dependency.get("source"):
            violations.append(
                f"{package['name']} depends on {dependency['name']} with a wildcard requirement (*)"
            )

# 5. root license artifacts.
for required in ("LICENSE", "NOTICE", "deny.toml"):
    path = root / required
    if not path.is_file() or path.stat().st_size == 0:
        violations.append(f"required root artifact {required!r} is missing or empty")

if violations:
    print("check-licenses: POLICY VIOLATIONS:", file=sys.stderr)
    for violation in violations:
        print(f"  - {violation}", file=sys.stderr)
    sys.exit(1)

print(
    f"check-licenses: {len(workspace)} workspace members (Apache-2.0) and "
    f"{len(external)} external packages satisfy the deny.toml licenses/bans/sources policy"
)
PY
RC=$?
if [ "$RC" -ne 0 ]; then
    exit "$RC"
fi

if command -v cargo-deny >/dev/null 2>&1; then
    echo "check-licenses: cargo-deny present; re-checking with the reference engine"
    if ! cargo deny --offline check licenses bans sources; then
        echo "check-licenses: cargo deny check failed (run 'cargo deny check licenses bans sources' without --offline for the full report)" >&2
        exit 1
    fi
fi

echo "check-licenses: OK"
