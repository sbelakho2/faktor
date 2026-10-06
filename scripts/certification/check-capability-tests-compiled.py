#!/usr/bin/env python3
"""Compiled capability-proof enumeration gate (audit item 3).

Source-text `fn <name>(` matching can be satisfied by comments, dead cfg
sections or non-built targets. This gate runs in a CARGO-BEARING lane with
the test targets already built:

    cargo test --workspace --all-features -- --list \
        | python3 scripts/certification/check-capability-tests-compiled.py

It parses the REAL libtest inventory from stdin and requires every `proven`
Rust capability's unit proof to be a listed test EXACTLY ONCE. It also
verifies tests/production-crates.json is a byte-fresh snapshot of the local
non-test packages from `cargo metadata --no-deps` (the node-only certificate
lane trusts that snapshot, so staleness must be caught here).

Usage:
  ... --list | check-capability-tests-compiled.py [--manifest PATH]
  check-capability-tests-compiled.py selftest
"""
import json
import re
import subprocess
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "tests" / "invariant-coverage.json"
SNAPSHOT = ROOT / "tests" / "production-crates.json"
LISTED = re.compile(r"^(\S+): test$")


def parse_listed(text):
    names = []
    for line in text.splitlines():
        match = LISTED.match(line.strip())
        if match:
            names.append(match.group(1))
    return names


def rust_units(manifest):
    """(capability id, unit) for every proven Rust proof (source override or crate)."""
    units = []
    for entry in manifest.get("entries", []):
        for row in entry.get("capabilities", []):
            if row.get("class") == "gap" or "delegate" in row:
                continue
            unit = row.get("unit")
            if isinstance(unit, str) and unit:
                units.append((row.get("id", "?"), unit))
    for surface in manifest.get("surfaces", []):
        for row in surface.get("capabilities", []):
            # IDE surface proofs are harness-emitted titles, not libtest names.
            if row.get("class") == "gap" or "delegate" in row:
                continue
    return units


def check(manifest_path=MANIFEST, listed_text=None, snapshot_path=SNAPSHOT, run_metadata=True):
    problems = []
    if listed_text is None:
        listed_text = sys.stdin.read()
    listed = parse_listed(listed_text)
    if not listed:
        problems.append(
            "compiled-test-inventory-empty: no libtest lines on stdin (run with `cargo test -- --list`)"
        )
    counts = Counter(listed)
    manifest = json.loads(Path(manifest_path).read_text())
    for capability, unit in rust_units(manifest):
        exact = counts.get(unit, 0)
        suffix = sum(1 for name, count in counts.items() if count and name.endswith(f"::{unit}"))
        total = exact or suffix
        if total != 1:
            problems.append(
                f"uncompiled-unit: {capability} proof {unit!r} appears {total} time(s) in the compiled test inventory (need exactly 1)"
            )
    if run_metadata:
        metadata = json.loads(
            subprocess.check_output(
                ["cargo", "metadata", "--no-deps", "--format-version", "1"],
                cwd=ROOT,
                text=True,
            )
        )
        live = sorted(
            pkg["name"]
            for pkg in metadata["packages"]
            if pkg["source"] is None and not pkg["name"].startswith("faktor-tests-")
        )
        snapshot = json.loads(Path(snapshot_path).read_text())
        if sorted(snapshot.get("crates", [])) != live:
            problems.append(
                f"stale-crate-snapshot: tests/production-crates.json does not match cargo metadata "
                f"(snapshot {len(snapshot.get('crates', []))} vs live {len(live)}); run "
                f"`node scripts/check-invariant-coverage.mjs --refresh-crates`"
            )
    return problems, len(listed)


def selftest():
    failures = 0

    def check_case(name, ok):
        nonlocal failures
        if ok:
            print(f"selftest ok: {name}")
        else:
            print(f"selftest FAIL: {name}", file=sys.stderr)
            failures += 1

    import tempfile

    with tempfile.TemporaryDirectory() as tmp:
        manifest = Path(tmp) / "manifest.json"
        manifest.write_text(
            json.dumps(
                {
                    "entries": [
                        {
                            "crate": "faktor-alpha",
                            "class": "release-critical",
                            "capabilities": [
                                {"id": "alpha.one", "unit": "proof_one", "mutation": "INV-A"}
                            ],
                        }
                    ],
                    "surfaces": [],
                }
            )
        )
        snapshot = Path(tmp) / "snapshot.json"
        snapshot.write_text(json.dumps({"crates": ["faktor-alpha"]}))
        good = "runtime::tests::proof_one: test\nruntime::tests::other: test\n"
        problems, count = check(manifest, good, snapshot, run_metadata=False)
        check_case("a listed proof passes", problems == [] and count == 2)
        bad = "runtime::tests::other: test\n"
        problems, _ = check(manifest, bad, snapshot, run_metadata=False)
        check_case(
            "an unlisted proof fails",
            any("uncompiled-unit" in problem for problem in problems),
        )
        duplicated = "a::proof_one: test\nb::proof_one: test\n"
        problems, _ = check(manifest, duplicated, snapshot, run_metadata=False)
        check_case(
            "a duplicated proof fails",
            any("uncompiled-unit" in problem for problem in problems),
        )
        problems, _ = check(manifest, "", snapshot, run_metadata=False)
        check_case(
            "an empty inventory fails",
            any("inventory-empty" in problem for problem in problems),
        )
    if failures:
        print(f"check-capability-tests-compiled selftest: FAIL ({failures})", file=sys.stderr)
        return 1
    print("check-capability-tests-compiled selftest: PASS")
    return 0


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "selftest":
        sys.exit(selftest())
    problems, count = check()
    if problems:
        for problem in problems:
            print(f"capability-tests-compiled: {problem}", file=sys.stderr)
        sys.exit(1)
    print(f"capability-tests-compiled: PASS ({count} compiled tests listed; every proven unit enumerated)")
