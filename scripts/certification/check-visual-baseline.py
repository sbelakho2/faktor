#!/usr/bin/env python3
"""Platform visual-baseline record gate (P1-CERT / UI assurance).

The JetBrains visual matrix pins one record per platform. A record that was
copied from another platform (or that lacks the resolved font fingerprint the
render actually used, or whose environment names a different platform) is not
platform proof: the Darwin lane must verify ITS record here, the Windows lane
verifies its record in its own PowerShell gate, and the Linux parity matrix
verifies the linux record in-process.

This gate fails closed unless visual-baselines.json has a record for the
requested platform whose:

  * environment names THAT platform (the `visualEnvironment()` prefix:
    `linux-`, `mac-os-`/`darwin-`, `windows-`), so a linux render can never
    stand in for another platform's record, and
  * environment carries a resolved font fingerprint (a `-f<hex>` suffix), and
  * digest is non-empty, and
  * digest differs from every OTHER platform record (a copied file is not a
    separately produced render).

Classes (`--class`):

  * `release` (the default): a MISSING record is a hard refusal — a release
    claim may never cover a platform that was not rendered.
  * `ci-evidence`: a missing platform is a typed, visible
    `not_certified-on-platform` boundary (exit 3) for CI-evidence
    certificates, which record it and never render it green. A record that
    EXISTS but is unfingerprinted/foreign/copied is NEVER a boundary: it is
    a hard refusal in both classes. The release class stays refused.

Usage:
  python3 scripts/certification/check-visual-baseline.py --platform macos \
      --file apps/jetbrains/frontend/src/test/resources/parity/visual-baselines.json
  python3 scripts/certification/check-visual-baseline.py --class ci-evidence \
      --platform windows --file <baseline>
  python3 scripts/certification/check-visual-baseline.py selftest

Exit codes: 0 pass; 1 refusal; 2 usage/parse; 3 CI-evidence platform boundary
(missing record only, only under --class ci-evidence).
"""
import json
import os
import re
import sys

FINGERPRINT = re.compile(r"-f[0-9a-f]{8,}$")

CLASS_RELEASE = "release"
CLASS_CI_EVIDENCE = "ci-evidence"
CLASSES = (CLASS_RELEASE, CLASS_CI_EVIDENCE)
BOUNDARY_EXIT = 3

# The environment prefixes the Kotlin visualEnvironment() writer produces per
# host (os.name slug): Linux -> `linux-…`, macOS -> `mac-os-x-…`, Windows ->
# `windows-…`. Aliases (`darwin-`) are accepted only as the named platform's
# own spelling; the prefix binds a record to the host that produced it.
PLATFORM_ENVIRONMENT_PREFIXES = {
    "linux": ("linux-",),
    "macos": ("mac-os-", "darwin-"),
    "windows": ("windows-",),
}


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
    prefixes = PLATFORM_ENVIRONMENT_PREFIXES.get(platform)
    if prefixes is None:
        return [
            f"{platform}=unknown-platform (expected one of "
            + ", ".join(sorted(PLATFORM_ENVIRONMENT_PREFIXES))
            + "; a record is only accepted for a release platform)"
        ]
    record = record_for(document, platform)
    if record is None:
        return [f"{platform}=missing (the lane must pin its own record before release)"]
    environment = record.get("environment")
    if not isinstance(environment, str) or not FINGERPRINT.search(environment):
        problems.append(
            f"{platform}=unfingerprinted (environment {environment!r} lacks a resolved -f<hex> font fingerprint)"
        )
    if isinstance(environment, str) and not environment.startswith(prefixes):
        problems.append(
            f"{platform}=foreign-environment (environment {environment!r} does not name the "
            f"{platform} host; prefixes {prefixes}; another platform's render is never accepted)"
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
            "linux": {"environment": "linux-x86_64-jvm17-f0a1b2c3d4e5", "digests": {"a": "a" * 64}},
            "macos": {"environment": "mac-os-x-aarch64-jvm17-f0a1b2c3d4e5", "digests": {"a": "b" * 64}},
            "windows": {"environment": "windows-amd64-jvm17-f0a1b2c3d4e5", "digests": {"a": "c" * 64}},
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
    check_case(
        "a linux render under the macos key refuses (foreign environment)",
        any(
            "foreign-environment" in problem
            for problem in check(
                {
                    "platforms": {
                        "macos": {
                            "environment": "linux-amd64-jvm17-f0a1b2c3d4e5",
                            "digests": {"a": "b" * 64},
                        }
                    }
                },
                "macos",
            )
        ),
    )
    check_case(
        "the darwin alias is accepted as this platform's own spelling",
        check(
            {
                "platforms": {
                    "macos": {
                        "environment": "darwin-arm64-jvm17-f0a1b2c3d4e5",
                        "digests": {"a": "b" * 64},
                    }
                }
            },
            "macos",
        )
        == [],
    )
    check_case(
        "an unknown platform refuses",
        any("unknown-platform" in problem for problem in check(good, "freebsd")),
    )
    # CI-evidence vs release class (the certificate's platform boundary):
    # a MISSING record is a typed boundary only for CI evidence; release
    # refuses. A record that EXISTS but is malformed is a hard failure in
    # BOTH classes — a boundary is never a mask for a broken record.
    kind, _ = classify({"platforms": {}}, "windows", CLASS_CI_EVIDENCE)
    check_case(
        "ci-evidence: a missing platform is a typed boundary",
        kind == "not_certified-on-platform",
    )
    kind, _ = classify({"platforms": {}}, "windows", CLASS_RELEASE)
    check_case("release: a missing platform refuses", kind == "failure")
    kind, _ = classify(
        {"platforms": {"windows": {"environment": "windows-amd64-jvm17", "digests": {"a": "b" * 64}}}},
        "windows",
        CLASS_CI_EVIDENCE,
    )
    check_case(
        "ci-evidence: an unfingerprinted record is a hard failure, never a boundary",
        kind == "failure",
    )
    kind, _ = classify(good, "windows", CLASS_CI_EVIDENCE)
    check_case("ci-evidence: a certified record stays certified", kind == "certified")
    kind, _ = classify(
        {"schema": "faktor-parity-visual-baselines/v2", "panelDigests": {}},
        "windows",
        CLASS_CI_EVIDENCE,
    )
    check_case(
        "ci-evidence: a v2 baseline without platform records is not a boundary",
        kind == "failure",
    )
    if failures:
        print(f"visual-baseline selftest: FAIL ({failures})", file=sys.stderr)
        return 1
    print("visual-baseline selftest: PASS")
    return 0


def classify(document, platform, class_name):
    """Classify one platform's record for one gate class.

    Returns `(kind, problems)` with `kind` in:

      * `"certified"` — no problems;
      * `"not_certified-on-platform"` — the record is MISSING and the class
        is CI-evidence: a typed platform boundary, never a green result;
      * `"failure"` — every other problem (including a missing record in the
        release class, and ANY malformed/unfingerprinted/foreign/copied
        record in every class).
    """
    problems = check(document, platform)
    if not problems:
        return ("certified", [])
    missing_record = (
        class_name == CLASS_CI_EVIDENCE
        and platform in PLATFORM_ENVIRONMENT_PREFIXES
        and isinstance(document.get("platforms"), dict)
        and record_for(document, platform) is None
        and all(problem.startswith(f"{platform}=missing") for problem in problems)
    )
    if missing_record:
        return ("not_certified-on-platform", problems)
    return ("failure", problems)


def main(args):
    platform = ""
    path = ""
    class_name = CLASS_RELEASE
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
        if args[at] == "--class" and at + 1 < len(args):
            class_name = args[at + 1]
            at += 2
            continue
        at += 1
    if not platform or not path:
        print(
            "usage: check-visual-baseline.py --platform NAME --file FILE "
            "[--class release|ci-evidence] | selftest",
            file=sys.stderr,
        )
        return 2
    if class_name not in CLASSES:
        print(
            f"usage: --class must be one of {', '.join(CLASSES)} (got {class_name!r})",
            file=sys.stderr,
        )
        return 2
    if not os.path.isfile(path):
        fail(f"{path} does not exist")
    try:
        document = json.load(open(path, encoding="utf-8"))
    except Exception as error:  # noqa: BLE001 - any parse failure is a refusal
        fail(f"{path} is not valid JSON: {error}")
    kind, problems = classify(document, platform, class_name)
    if kind == "certified":
        print(f"visual-baseline: PASS ({platform} record fingerprinted and distinct)")
        return 0
    if kind == "not_certified-on-platform":
        print(
            f"visual-baseline: {platform}=not_certified-on-platform "
            "(CI-evidence platform boundary: no committed record for this "
            "platform; the release class still refuses via "
            "scripts/check-visual-platforms.mjs --release)"
        )
        return BOUNDARY_EXIT
    for problem in problems:
        print(f"visual-baseline: {problem}", file=sys.stderr)
    fail(f"{platform} visual proof incomplete")
    return 1  # unreachable (fail exits); kept for clarity


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "selftest":
        sys.exit(selftest())
    sys.exit(main(sys.argv[1:]))
