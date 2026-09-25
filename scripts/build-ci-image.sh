#!/bin/sh
# Build the dedicated Faktor CI image (docker/faktor-ci/Dockerfile) and print
# its digest. The Dockerfile pins the base image by multi-arch index digest,
# the Ubuntu noble snapshot (snapshot.ubuntu.com, fixed timestamp) and every
# apt package by exact version; the build re-verifies the pins with
# dpkg-query and fails on drift.
#
# Usage:
#   bash scripts/build-ci-image.sh [--platform linux/amd64] [--tag TAG]
#                                  [--registry HOST/PATH] [--push]
#                                  [--record-digest FILE]
#   bash scripts/build-ci-image.sh --verify-runtime
#   bash scripts/build-ci-image.sh --selftest
#
# Registry / push:
#   * Without --registry (or FAKTOR_CI_IMAGE_REGISTRY) the image is loaded
#     into the local docker daemon and the script prints the local digest.
#     `.woodpecker/**` references that digest as
#     `image: faktor-ci@sha256:<digest>` with `pull: false`; the required
#     `ci-image` pre-step runs ON that digest and fails closed when the image
#     was not produced locally (no apt fallback).
#   * With --registry / FAKTOR_CI_IMAGE_REGISTRY (and optionally --push) the
#     image is pushed and the printed `<ref>@sha256:<digest>` can be
#     referenced directly by `.woodpecker/**` steps.
#   * --record-digest FILE writes the built digest to FILE (the digest record
#     read by `scripts/check-ci-image-pins.sh` and by the trusted attestation
#     step for `build_environment_digest`). The lanes commit the record at
#     `docker/faktor-ci/image-digest.txt`.
#
# --verify-runtime runs INSIDE a built Faktor CI image (no docker needed): it
# checks the platform, that every exact `pkg=version` pin in the Dockerfile is
# satisfied by the running image, that the pinned tools (including git and the
# JDK 17 javac the JetBrains lanes need) are on PATH and that apt is pointed
# at the fixed snapshot only. This is the required CI pre-step's check.
#
# Exit codes: 0 built (or selftest/verify-runtime passed); 2 usage error;
# other = docker failure or verification failure.
set -eu

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
ROOT="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)"
CONTEXT="$ROOT/docker/faktor-ci"
DOCKERFILE="$CONTEXT/Dockerfile"

PLATFORM="${FAKTOR_CI_IMAGE_PLATFORM:-linux/amd64}"
TAG="${FAKTOR_CI_IMAGE_TAG:-faktor-ci}"
REGISTRY="${FAKTOR_CI_IMAGE_REGISTRY:-}"
PUSH=0
SELFTEST=0
VERIFY_RUNTIME=0
RECORD_DIGEST=""

usage() {
    sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
}

while [ "$#" -gt 0 ]; do
    case "$1" in
    --platform)
        [ "$#" -ge 2 ] || { echo "build-ci-image: --platform needs a value" >&2; exit 2; }
        PLATFORM="$2"
        shift 2
        ;;
    --tag)
        [ "$#" -ge 2 ] || { echo "build-ci-image: --tag needs a value" >&2; exit 2; }
        TAG="$2"
        shift 2
        ;;
    --registry)
        [ "$#" -ge 2 ] || { echo "build-ci-image: --registry needs a value" >&2; exit 2; }
        REGISTRY="$2"
        PUSH=1
        shift 2
        ;;
    --push)
        PUSH=1
        shift
        ;;
    --record-digest)
        [ "$#" -ge 2 ] || { echo "build-ci-image: --record-digest needs a file" >&2; exit 2; }
        RECORD_DIGEST="$2"
        shift 2
        ;;
    --verify-runtime)
        VERIFY_RUNTIME=1
        shift
        ;;
    --selftest)
        SELFTEST=1
        shift
        ;;
    -h | --help)
        usage
        exit 0
        ;;
    *)
        echo "build-ci-image: unknown argument: $1" >&2
        exit 2
        ;;
    esac
done

# Runs inside a built Faktor CI image: re-verify every exact pkg=version pin
# from the Dockerfile against the running image, the pinned tool set and the
# snapshot-only apt sources. Any drift means the image is not the pinned CI
# image and the CI pre-step fails closed.
verify_runtime() {
    rc=0
    os="$(uname -s)"
    arch="$(uname -m)"
    if [ "$os" != "Linux" ]; then
        echo "build-ci-image verify-runtime: FAIL: expected Linux, running on $os" >&2
        rc=1
    fi
    if [ "$arch" != "x86_64" ]; then
        echo "build-ci-image verify-runtime: FAIL: expected linux/amd64 (x86_64), running on $arch" >&2
        rc=1
    fi
    for tool in cc gcc make pkg-config python3 ps kotlinc java javac git curl unzip; do
        command -v "$tool" >/dev/null 2>&1 || {
            echo "build-ci-image verify-runtime: FAIL: required tool '$tool' is not on PATH" >&2
            rc=1
        }
    done
    specs="$(grep -oE '[a-z0-9][a-z0-9+.-]*=[0-9][^ \;]*' "$DOCKERFILE" | sort -u || true)"
    if [ -z "$specs" ]; then
        echo "build-ci-image verify-runtime: FAIL: no exact pkg=version pins found in $DOCKERFILE" >&2
        rc=1
    fi
    for spec in $specs; do
        pkg="${spec%%=*}"
        want="${spec#*=}"
        got="$(dpkg-query -W -f='${Version}' "$pkg" 2>/dev/null || true)"
        if [ "$got" != "$want" ]; then
            echo "build-ci-image verify-runtime: FAIL: version drift: $pkg = ${got:-missing} (want $want)" >&2
            rc=1
        fi
    done
    if [ "$rc" -eq 0 ]; then
        echo "build-ci-image verify-runtime: every pinned package/tool matches docker/faktor-ci/Dockerfile"
    fi
    if [ -f /etc/apt/sources.list.d/faktor-snapshot.list ] && grep -q 'snapshot.ubuntu.com/ubuntu/' /etc/apt/sources.list.d/faktor-snapshot.list; then
        echo "build-ci-image verify-runtime: apt sources are the fixed Ubuntu snapshot"
    else
        echo "build-ci-image verify-runtime: FAIL: snapshot-only apt sources missing" >&2
        rc=1
    fi
    if [ "$rc" -eq 0 ]; then
        echo "build-ci-image verify-runtime: PASS"
    else
        echo "build-ci-image verify-runtime: FAIL" >&2
    fi
    return "$rc"
}

selftest() {
    rc=0
    if [ ! -f "$DOCKERFILE" ]; then
        echo "build-ci-image selftest: FAIL: $DOCKERFILE missing" >&2
        return 1
    fi
    # Every mutable input must be pinned in the Dockerfile.
    if grep -qE '^FROM .*@sha256:[0-9a-f]{64}$' "$DOCKERFILE"; then
        echo "selftest ok: base image is digest-pinned"
    else
        echo "build-ci-image selftest: FAIL: base image lacks @sha256:" >&2
        rc=1
    fi
    if grep -q 'snapshot.ubuntu.com/ubuntu/' "$DOCKERFILE" && grep -q 'SNAPSHOT=2026' "$DOCKERFILE"; then
        echo "selftest ok: apt sources are snapshot-pinned"
    else
        echo "build-ci-image selftest: FAIL: apt snapshot pin missing" >&2
        rc=1
    fi
    staged="$(sed -n '/apt-get install -y --no-install-recommends \\/,/;/p' "$DOCKERFILE")"
    specs="$(printf '%s\n' "$staged" | grep -oE '[a-z0-9][a-z0-9+.-]*=[^ \\;]+' || true)"
    if [ -n "$specs" ]; then
        echo "selftest ok: $(printf '%s\n' "$specs" | wc -l | tr -d ' ') exact-version package pin(s) present"
    else
        echo "build-ci-image selftest: FAIL: no exact pkg=version pins found" >&2
        rc=1
    fi
    if grep -q 'version drift' "$DOCKERFILE"; then
        echo "selftest ok: built image re-verifies the pins with dpkg-query"
    else
        echo "build-ci-image selftest: FAIL: no post-install version verification" >&2
        rc=1
    fi
    if [ "$rc" -eq 0 ]; then
        echo "build-ci-image selftest: PASS"
    else
        echo "build-ci-image selftest: FAIL" >&2
    fi
    return "$rc"
}

if [ "$VERIFY_RUNTIME" -eq 1 ]; then
    verify_runtime
    exit $?
fi

if [ "$SELFTEST" -eq 1 ]; then
    selftest
    exit $?
fi

command -v docker >/dev/null 2>&1 || {
    echo "build-ci-image: docker is required" >&2
    exit 2
}

if [ -n "$REGISTRY" ]; then
    IMAGE="$REGISTRY/faktor-ci:$TAG"
else
    IMAGE="faktor-ci:$TAG"
fi

META="$(mktemp "${TMPDIR:-/tmp}/faktor-ci-image-meta.XXXXXX")"
rm -f "$META"
trap 'rm -f "$META"' EXIT INT TERM

if [ "$PUSH" -eq 1 ]; then
    [ -n "$REGISTRY" ] || { echo "build-ci-image: --push needs --registry (or FAKTOR_CI_IMAGE_REGISTRY)" >&2; exit 2; }
    echo "build-ci-image: building and pushing $IMAGE (platform $PLATFORM)"
    docker buildx build --platform "$PLATFORM" --file "$DOCKERFILE" \
        --tag "$IMAGE" --push --metadata-file "$META" "$CONTEXT"
else
    echo "build-ci-image: building $IMAGE (platform $PLATFORM; local load)"
    docker buildx build --platform "$PLATFORM" --file "$DOCKERFILE" \
        --tag "$IMAGE" --load --metadata-file "$META" "$CONTEXT"
fi

DIGEST="$(python3 - "$META" <<'PY'
import json
import sys

with open(sys.argv[1]) as fh:
    meta = json.load(fh)
print(meta.get("containerimage.digest", ""))
PY
)" || DIGEST=""

if [ -z "$DIGEST" ]; then
    INSPECT="$(docker image inspect "$IMAGE" --format '{{index .RepoDigests 0}}' 2>/dev/null || true)"
    DIGEST="${INSPECT#*@}"
fi

[ -n "$DIGEST" ] || { echo "build-ci-image: built but no digest could be read" >&2; exit 1; }

echo "build-ci-image: image digest: $IMAGE@$DIGEST"
if [ "$PUSH" -eq 1 ]; then
    echo "build-ci-image: pushed; reference .woodpecker steps as image: <registry>/faktor-ci@$DIGEST WITHOUT 'pull: false' so agents pull the published digest, and re-record docker/faktor-ci/image-digest.txt"
else
    echo "build-ci-image: local-only digest (not pullable by CI agents); reference the lanes as image: faktor-ci@$DIGEST with pull: false so the required ci-image pre-step fails closed when the image was not produced locally; set FAKTOR_CI_IMAGE_REGISTRY (or --registry) to publish a pullable image instead"
fi

if [ -n "$RECORD_DIGEST" ]; then
    {
        printf '%s\n' "# Faktor CI image digest record (linux/amd64)."
        printf '%s\n' "# Built from docker/faktor-ci/Dockerfile by scripts/build-ci-image.sh;"
        printf '%s\n' "# read by scripts/check-ci-image-pins.sh and the trusted attestation step"
        printf '%s\n' "# (build_environment_digest). Rebuild + re-record deliberately; do not hand-edit."
        printf '%s\n' "$DIGEST"
    } >"$RECORD_DIGEST"
    echo "build-ci-image: recorded digest in $RECORD_DIGEST"
fi
