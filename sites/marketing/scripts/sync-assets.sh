#!/usr/bin/env bash
# Copy canonical brand assets from branding/ into the marketing site's
# assets/ folder at build/deploy time. branding/ is the single source of
# truth; assets/ is gitignored. Run from sites/marketing/.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
SRC="$ROOT/branding"
SITE="$(cd "$(dirname "$0")/.." && pwd)"
DST="$SITE/assets"

mkdir -p "$DST"
cp -v "$SRC/mark.png"          "$DST/"
cp -v "$SRC/wordmark.png"      "$DST/"
cp -v "$SRC/favicon.png"       "$DST/"
cp -v "$SRC/colors.json"       "$DST/"
# Optional extras if present
[ -f "$SRC/wordmark-ink.png" ]    && cp -v "$SRC/wordmark-ink.png" "$DST/"
[ -f "$SRC/wordmark-vellum.png" ] && cp -v "$SRC/wordmark-vellum.png" "$DST/"

echo "Synced canonical brand assets → $DST"
