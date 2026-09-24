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
#   bash scripts/build-ci-image.sh --selftest
#
# Registry / push:
#   * Without --registry (or FAKTOR_CI_IMAGE_REGISTRY) the image is loaded
#     into the local docker daemon and the script prints the local digest.
#     CI lanes keep using the inline snapshot+exact-version apt steps, since
#     a local digest is not pullable by a remote agent.
#   * With --registry / FAKTOR_CI_IMAGE_REGISTRY (and optionally --push) the
#     image is pushed and the printed `<ref>@sha256:<digest>` can be
#     referenced directly by `.woodpecker/**` steps (replace the apt step and
#     annotate it `# apt-pinned: <ref@sha256:...>`).
#
# Exit codes: 0 built (or selftest passed); 2 usage error; other = docker
# failure.
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
    echo "build-ci-image: pushed; reference .woodpecker steps as image: $IMAGE@$DIGEST with '# apt-pinned: $IMAGE@$DIGEST'"
else
    echo "build-ci-image: local-only digest (not pullable by CI agents); set FAKTOR_CI_IMAGE_REGISTRY (or --registry) to publish, then switch the lanes to image@sha256:<digest>"
fi
