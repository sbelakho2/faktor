#!/usr/bin/env bash
# Refresh the PINNED IANA special-purpose address registry CSVs under
# crates/security/data/ from iana.org.
#
# This is the ONLY network-touching half of the egress address authority:
# the runtime and the generated table never fetch anything. Run this
# deliberately, review the diff, commit the CSVs, then regenerate the table:
#
#     ./scripts/update-iana-special-registry.sh
#     ./scripts/generate-iana-network-table
#     ./scripts/generate-iana-network-table --check
#
# Requirements: curl (or wget). The downloads are staged in a temp dir and
# moved into place only when the content actually parses as the expected
# registry CSV, so a truncated/misrouted download never replaces the pin.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DATA_DIR="$ROOT/crates/security/data"

IPV4_URL="https://www.iana.org/assignments/iana-ipv4-special-registry/iana-ipv4-special-registry-1.csv"
IPV6_URL="https://www.iana.org/assignments/iana-ipv6-special-registry/iana-ipv6-special-registry-1.csv"
EXPECTED_HEADER="Address Block,Name,RFC,Allocation Date,Termination Date,Source,Destination,Forwardable,Globally Reachable,Reserved-by-Protocol"

fetch() {
  local url="$1" dest="$2"
  if command -v curl >/dev/null 2>&1; then
    curl --fail --silent --show-error --location --max-time 120 -o "$dest" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget --quiet --timeout=120 -O "$dest" "$url"
  else
    echo "update-iana-special-registry: neither curl nor wget is available" >&2
    exit 1
  fi
}

validate() {
  local file="$1" label="$2"
  if ! head -n 1 "$file" | grep -qF "$EXPECTED_HEADER"; then
    echo "update-iana-special-registry: $label download does not carry the expected IANA header" >&2
    exit 1
  fi
  local rows
  rows="$(wc -l < "$file" | tr -d ' ')"
  if [ "$rows" -lt 10 ]; then
    echo "update-iana-special-registry: $label download looks truncated ($rows lines)" >&2
    exit 1
  fi
}

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

fetch "$IPV4_URL" "$stage/iana-ipv4-special.csv"
fetch "$IPV6_URL" "$stage/iana-ipv6-special.csv"
validate "$stage/iana-ipv4-special.csv" "IPv4"
validate "$stage/iana-ipv6-special.csv" "IPv6"

mkdir -p "$DATA_DIR"
mv "$stage/iana-ipv4-special.csv" "$DATA_DIR/iana-ipv4-special.csv"
mv "$stage/iana-ipv6-special.csv" "$DATA_DIR/iana-ipv6-special.csv"

echo "update-iana-special-registry: refreshed pinned CSVs under crates/security/data/"
echo "next: ./scripts/generate-iana-network-table && ./scripts/generate-iana-network-table --check"
