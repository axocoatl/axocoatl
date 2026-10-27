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

TASK_SOURCE="$ROOT/sites/marketing/assets/demo/dashboard-task-1.5"
TASK_DESTINATION="$DST/assets/dashboard-task-1.5"
mkdir -p "$TASK_DESTINATION"
for asset in dashboard-task.mp4 result.jpg captions.vtt chapters.vtt; do
  cp -v "$TASK_SOURCE/$asset" "$TASK_DESTINATION/$asset"
done
echo "Synced canonical dashboard task media → $TASK_DESTINATION"

REVIEW_SOURCE="$ROOT/sites/marketing/assets/demo/required-review-1.5"
REVIEW_DESTINATION="$DST/assets/required-review-1.5"
mkdir -p "$REVIEW_DESTINATION"
for asset in required-review.mp4 result.jpg captions.vtt chapters.vtt; do
  cp -v "$REVIEW_SOURCE/$asset" "$REVIEW_DESTINATION/$asset"
done
echo "Synced canonical required-review media → $REVIEW_DESTINATION"

cp -v "$ROOT/sites/marketing/components/ax-video-chapters.js" "$DST/assets/ax-video-chapters.js"
