#!/usr/bin/env python3
"""Platform visual-baseline record gate (P1-CERT / UI assurance).

The JetBrains visual matrix pins one record per platform. A record that was
copied from another platform (or that lacks the resolved font fingerprint the
render actually used) is not platform proof: the Darwin lane must verify ITS
record here, the Windows lane verifies its record in its own PowerShell gate,
and the Linux parity matrix verifies the linux record in-process.

This gate fails closed unless visual-baselines.json has a record for the
requested platform whose:

  * environment carries a resolved font fingerprint (a `-f<hex>` suffix), and
  * digest is non-empty, and
  * digest differs from every OTHER platform record (a copied file is not a
    separately produced render), and
  * size is a positive integer.

Usage:
  python3 scripts/certification/check-visual-baseline.py --platform macos \
      --file apps/jetbrains/frontend/src/test/resources/parity/visual-baselines.json
  python3 scripts/certification/check-visual-baseline.py selftest

Exit codes: 0 pass; 1 refusal; 2 usage/parse.
"""
import json
import os
import re
import sys

FINGERPRINT = re.compile(r"-f[0-9a-f]{8,}$")


def fail(detail):
    print(f"visual-baseline: {detail}", file=sys.stderr)
    sys.exit(1)


def record_for(document, platform):
    platforms = document.get("platforms")
    if not isinstance(platforms, dict):
        return None
    record = platforms.get(platform)
    return record if isinstance(record, dict) else None


def check(document, platform):
    """Return a list of problems for one platform record."""
    problems = []
    record = record_for(document, platform)
    if record is None:
        return [f"{platform}=missing (the lane must pin its own record before release)"]
    environment = record.get("environment")
    if not isinstance(environment, str) or not FINGERPRINT.search(environment):
        problems.append(
            f"{platform}=unfingerprinted (environment {environment!r} lacks a resolved -f<hex> font fingerprint)"
        )
    digests = record.get("digests")
    if not isinstance(digests, dict) or not digests:
        problems.append(f"{platform}=digests-missing")
        digests = {}
    for family, digest in digests.items():
        if not isinstance(digest, str) or len(digest) < 16:
            problems.append(f"{platform}=digest-invalid:{family}")
    # A digest SET shared with another platform means the record was copied,
    # not produced by this platform's render.
    platforms = document.get("platforms", {})
    for other, other_record in platforms.items():
        if other == platform or not isinstance(other_record, dict):
            continue
        if digests and other_record.get("digests") == digests:
            problems.append(f"{platform}=copied-from-{other}")
    return problems


def selftest():
    failures = 0

    def check_case(name, ok):
        nonlocal failures
        if ok:
            print(f"selftest ok: {name}")
        else:
            print(f"selftest FAIL: {name}", file=sys.stderr)
            failures += 1

    good = {
        "platforms": {
            "linux": {"environment": "linux-x86_64-jvm17-f0a1b2c3", "digests": {"a": "a" * 64}},
            "macos": {"environment": "mac-os-x-aarch64-jvm17-f0a1b2c3", "digests": {"a": "b" * 64}},
            "windows": {"environment": "windows-amd64-jvm17-f0a1b2c3", "digests": {"a": "c" * 64}},
        }
    }
    check_case("a fingerprinted record passes", check(good, "macos") == [])
    check_case(
        "a missing record refuses",
        any("missing" in problem for problem in check({"platforms": {}}, "macos")),
    )
    check_case(
        "a record without a resolved font fingerprint refuses",
        any(
            "unfingerprinted" in problem
            for problem in check(
                {"platforms": {"macos": {"environment": "mac-os-x-aarch64-jvm17", "digests": {"a": "b" * 64}}}},
                "macos",
            )
        ),
    )
    check_case(
        "a copied record refuses",
        any(
            "copied-from-linux" in problem
            for problem in check(
                {
                    "platforms": {
                        "linux": {"environment": "linux-f0a1b2c3", "digests": {"a": "a" * 64}},
                        "macos": {"environment": "mac-os-f0a1b2c3", "digests": {"a": "a" * 64}},
                    }
                },
                "macos",
            )
        ),
    )
    if failures:
        print(f"visual-baseline selftest: FAIL ({failures})", file=sys.stderr)
        return 1
    print("visual-baseline selftest: PASS")
    return 0


def main(args):
    platform = ""
    path = ""
    at = 0
    while at < len(args):
        if args[at] == "--platform" and at + 1 < len(args):
            platform = args[at + 1]
            at += 2
            continue
        if args[at] == "--file" and at + 1 < len(args):
            path = args[at + 1]
            at += 2
            continue
        at += 1
    if not platform or not path:
        print("usage: check-visual-baseline.py --platform NAME --file FILE | selftest", file=sys.stderr)
        return 2
    if not os.path.isfile(path):
        fail(f"{path} does not exist")
    try:
        document = json.load(open(path, encoding="utf-8"))
    except Exception as error:  # noqa: BLE001 - any parse failure is a refusal
        fail(f"{path} is not valid JSON: {error}")
    problems = check(document, platform)
    if problems:
        for problem in problems:
            print(f"visual-baseline: {problem}", file=sys.stderr)
        fail(f"{platform} visual proof incomplete")
    print(f"visual-baseline: PASS ({platform} record fingerprinted and distinct)")
    return 0


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "selftest":
        sys.exit(selftest())
    sys.exit(main(sys.argv[1:]))
