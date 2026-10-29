#!/usr/bin/env bash
# Mirror the canonical brand assets from ../../branding/ into public/ so
# Starlight can serve them at /mark.png, /wordmark.png, /favicon.png.
# Also mirror canonical dashboard recordings and their text tracks.
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

# Retired copies are generated; preserve the historical canonical assets in Git.
rm -rf "$DST/assets/dashboard-task-1.5" "$DST/assets/required-review-1.5"

FLOW_SOURCE="$ROOT/sites/marketing/assets/demo/dashboard-flow-1.5.3"
FLOW_DESTINATION="$DST/assets/dashboard-flow-1.5.3"
mkdir -p "$FLOW_DESTINATION"
for clip in dashboard-flow review receipt; do
  for suffix in mp4 jpg captions.vtt chapters.vtt; do
    cp -v "$FLOW_SOURCE/$clip.$suffix" "$FLOW_DESTINATION/$clip.$suffix"
  done
done
echo "Synced canonical dashboard flow media → $FLOW_DESTINATION"

cp -v "$ROOT/sites/marketing/components/ax-video-chapters.js" "$DST/assets/ax-video-chapters.js"
