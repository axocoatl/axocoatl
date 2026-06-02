#!/usr/bin/env bash
# Mirror the canonical brand assets from ../../branding/ into public/ so
# Starlight can serve them at /mark.png, /wordmark.png, /favicon.png.
# Run before `npm run build` (or `npm run dev`).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DST="$(cd "$(dirname "$0")" && pwd)/public"
mkdir -p "$DST"
cp -v "$ROOT/branding/mark.png"          "$DST/"
cp -v "$ROOT/branding/wordmark.png"      "$DST/"
cp -v "$ROOT/branding/favicon.png"       "$DST/"
cp -v "$ROOT/branding/colors.json"       "$DST/"
echo "Synced brand assets → $DST"
