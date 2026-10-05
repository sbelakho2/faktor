#!/bin/sh
# Universal lane-marker entry point for every CI lane image.
#
# Lane images deliberately differ: the rust and Faktor CI images carry
# python3 but NOT node, while the node/playwright images carry both. This
# launcher runs the canonical Node implementation when node exists and the
# byte-for-byte parity Python implementation otherwise, so every lane emits
# the marker JSON and its per-lane HMAC binding through ONE helper contract
# (`write --lane ... --status ... --artifact ...`), never hand-rolled printf.
#
# Secrets are per lane: the helper reads `faktor_lane_token_<lane>` (lane
# lowercased with non-alphanumerics as `_`; uppercase form accepted) from the
# step environment. When the token is absent the marker is emitted without
# `auth` and `verify-markers` records the lane as `unattested`.
set -eu

DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

if command -v node >/dev/null 2>&1; then
    exec node "$DIR/lane-marker.mjs" "$@"
fi
if command -v python3 >/dev/null 2>&1; then
    exec python3 "$DIR/lane-marker.py" "$@"
fi
echo "lane-marker: neither node nor python3 is available" >&2
exit 1
