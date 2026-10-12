#!/bin/sh
# Usage: backup.sh <src>
# Print the checksum of a temporary copy of <src>.
set -eu
src="$1"
work="${src}.backup.tmp"
cp "$src" "$work"
cksum < "$work"
rm -f "$work"
