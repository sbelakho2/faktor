#!/bin/sh
# Usage: backup.sh <src>
# Print the checksum of a temporary copy of <src>. The copy is made inside a
# fresh private (mode 0700) temporary directory with an unpredictable name, so
# a pre-planted predictable path can never be written through.
set -eu
src="$1"
dir=$(mktemp -d "${TMPDIR:-/tmp}/backup.XXXXXX")
trap 'rm -rf "$dir"' EXIT
work="$dir/data"
cp "$src" "$work"
cksum < "$work"
