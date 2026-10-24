#!/usr/bin/env bash
# Screenshot parity gate for the Faktor website.
#
# Baselines are reviewed WebP screenshots at the design breakpoints. They are
# derived from the implementation and reviewed side-by-side with the PDF page
# references in site/visual/pdf/ and site/visual/parity/. Set
# UPDATE_VISUAL_BASELINES=1 to intentionally refresh them after a design
# change and review the resulting parity montages.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
port="${FAKTOR_VISUAL_PORT:-8099}"
base="http://127.0.0.1:${port}"
data="$(mktemp -d)"
current="site/visual/current"
tolerance="${VISUAL_TOLERANCE:-0.02}"
mkdir -p "$current" site/visual/baselines
cleanup() {
  if [ -n "${server_pid:-}" ]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
    server_pid=""
  fi
  rm -rf "$data"
  zola --root site build >/dev/null 2>&1 || true
}
trap cleanup EXIT
command -v zola >/dev/null || { echo "visual: zola is required" >&2; exit 1; }
command -v npx >/dev/null || { echo "visual: npx (Playwright) is required" >&2; exit 1; }
command -v magick >/dev/null || { echo "visual: ImageMagick (magick) is required" >&2; exit 1; }
zola --root site build --base-url "$base" >/dev/null
FAKTOR_SITE_DATA_DIR="$data" FAKTOR_SITE_BASE_URL="$base" FAKTOR_SITE_BIND="127.0.0.1:${port}" \
  FAKTOR_SITE_COOKIE_SECURE=0 RUST_LOG=warn \
  cargo run --quiet -p faktor-site -- serve >"$data/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 240); do
  if curl -fsS "$base/healthz" >/dev/null 2>&1; then break; fi
  if ! kill -0 "$server_pid" 2>/dev/null; then echo "visual: server exited" >&2; cat "$data/server.log" >&2; exit 1; fi
  sleep 0.5
done
curl -fsS "$base/healthz" >/dev/null || { echo "visual: server did not become ready" >&2; exit 1; }

pairs=(
  "home|/" "approach|/approach/" "documentation|/documentation/"
  "evidence|/evidence/" "outcomes|/outcomes/" "workflow|/workflow/"
  "patterns|/patterns/" "identity|/identity/" "research|/research/"
  "editor|/editor/" "plans|/plans/" "login|/login/"
  "signup|/signup/" "prior|/prior/"
  "prior-approach|/prior/approach/" "prior-identity|/prior/identity/"
)
widths=("1440|1080" "1024|768" "390|844")
fail=0
count=0
for pair in "${pairs[@]}"; do
  name="${pair%%|*}"; route_path="${pair#*|}"
  for wh in "${widths[@]}"; do
    width="${wh%%|*}"; height="${wh#*|}"
    npx -y playwright@1.59.0 screenshot --browser chromium --viewport-size="${width},${height}" \
      --wait-for-timeout=250 "$base$route_path" "$current/$name-$width.png" >/dev/null 2>&1
    magick "$current/$name-$width.png" -quality 82 "$current/$name-$width.webp"
    baseline="site/visual/baselines/$name-$width.webp"
    if [ "${UPDATE_VISUAL_BASELINES:-0}" = "1" ]; then
      cp "$current/$name-$width.webp" "$baseline"
      continue
    fi
    [ -f "$baseline" ] || { echo "visual: missing baseline $baseline" >&2; fail=1; continue; }
    metric="$(magick compare -metric RMSE "$current/$name-$width.webp" "$baseline" null: 2>&1 || true)"
    value="$(printf '%s' "$metric" | sed -n 's/.*(\([0-9.]*\)).*/\1/p')"
    if [ -z "$value" ]; then
      echo "visual: could not read RMSE for $name-$width: $metric" >&2; fail=1; continue
    fi
    if ! awk -v v="$value" -v t="$tolerance" 'BEGIN { exit (v <= t ? 0 : 1) }'; then
      echo "visual: $name-$width differs: RMSE $value > $tolerance" >&2
      magick compare "$current/$name-$width.webp" "$baseline" "$current/diff-$name-$width.webp" 2>/dev/null || true
      fail=1
    fi
    count=$((count + 1))
  done
done
[ -f site/visual/pdf-reference.webp ] || { echo "visual: PDF reference contact sheet is missing" >&2; fail=1; }
mkdir -p site/visual/parity
pdf_tolerance="${PDF_VISUAL_TOLERANCE:-0.55}"
pdf_pairs=(
  "home|02" "approach|10" "documentation|13" "evidence|17"
  "outcomes|20" "workflow|23" "patterns|26" "identity|29"
  "research|34" "editor|36" "prior|51" "prior-approach|43"
  "prior-identity|46"
)
for pair in "${pdf_pairs[@]}"; do
  name="${pair%%|*}"; page="${pair#*|}"
  magick "$current/$name-1440.png" -resize 720x540 "$current/pdf-$name.webp"
  montage="site/visual/parity/$name.webp"
  if [ "${UPDATE_VISUAL_BASELINES:-0}" = "1" ]; then
    magick "site/visual/pdf/page-$page.webp" "$current/pdf-$name.webp" +append -background white -gravity center -extent 1480x560 -quality 82 "$montage"
  fi
  [ -f "$montage" ] || { echo "visual: missing parity montage $montage" >&2; fail=1; continue; }
  metric="$(magick compare -metric RMSE "$current/pdf-$name.webp" "site/visual/pdf/page-$page.webp" null: 2>&1 || true)"
  value="$(printf '%s' "$metric" | sed -n 's/.*(\([0-9.]*\)).*/\1/p')"
  if [ -z "$value" ]; then
    echo "visual: could not read PDF RMSE for $name: $metric" >&2; fail=1; continue
  fi
  echo "visual: PDF structural $name page $page RMSE $value"
  if ! awk -v v="$value" -v t="$pdf_tolerance" 'BEGIN { exit (v <= t ? 0 : 1) }'; then
    echo "visual: PDF structural parity $name exceeds $pdf_tolerance (RMSE $value)" >&2
    fail=1
  fi
done
if [ "${UPDATE_VISUAL_BASELINES:-0}" = "1" ]; then
  echo "visual: baselines refreshed at 1440/1024/390; review site/visual/parity/"
  exit 0
fi
[ "$fail" -eq 0 ] || exit 1
echo "visual: $count screenshots match reviewed baselines (RMSE <= $tolerance); $(( ${#pdf_pairs[@]} )) PDF page groups and side-by-side montages checked"
