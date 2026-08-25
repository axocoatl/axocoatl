#!/bin/sh
# Keep axocoatl-server's crates.io-safe embedded assets byte-identical to the
# canonical first-party Lattice and brand sources in this workspace.
set -eu

AXO_SYNC_SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
AXO_SYNC_REPO_ROOT="$(CDPATH= cd -- "$AXO_SYNC_SCRIPT_DIR/.." && pwd)"
AXO_SYNC_MODE=write

case "${1:-}" in
  "") ;;
  --check) AXO_SYNC_MODE=check ;;
  *)
    echo "Usage: sync-server-embedded-assets.sh [--check]" >&2
    exit 2
    ;;
esac
[ "$#" -le 1 ] || {
  echo "Usage: sync-server-embedded-assets.sh [--check]" >&2
  exit 2
}

sync_asset() {
  AXO_SYNC_SOURCE=$1
  AXO_SYNC_DESTINATION=$2
  [ -f "$AXO_SYNC_SOURCE" ] || {
    echo "Missing canonical embedded asset: $AXO_SYNC_SOURCE" >&2
    exit 1
  }
  if [ "$AXO_SYNC_MODE" = check ]; then
    [ -f "$AXO_SYNC_DESTINATION" ] && cmp -s "$AXO_SYNC_SOURCE" "$AXO_SYNC_DESTINATION" || {
      echo "Package-local embedded asset is missing or stale: $AXO_SYNC_DESTINATION" >&2
      echo "Run scripts/sync-server-embedded-assets.sh and commit the result." >&2
      exit 1
    }
  else
    cp "$AXO_SYNC_SOURCE" "$AXO_SYNC_DESTINATION"
    chmod 0644 "$AXO_SYNC_DESTINATION"
  fi
}

list_all_files() {
  AXO_SYNC_LIST_DIRECTORY=$1
  for AXO_SYNC_LIST_FILE in \
    "$AXO_SYNC_LIST_DIRECTORY"/* \
    "$AXO_SYNC_LIST_DIRECTORY"/.[!.]* \
    "$AXO_SYNC_LIST_DIRECTORY"/..?*; do
    [ -f "$AXO_SYNC_LIST_FILE" ] || continue
    basename -- "$AXO_SYNC_LIST_FILE"
  done | LC_ALL=C sort
}

list_javascript_files() {
  AXO_SYNC_LIST_DIRECTORY=$1
  for AXO_SYNC_LIST_FILE in "$AXO_SYNC_LIST_DIRECTORY"/*.js; do
    [ -f "$AXO_SYNC_LIST_FILE" ] || continue
    basename -- "$AXO_SYNC_LIST_FILE"
  done | LC_ALL=C sort
}

require_exact_files() {
  AXO_SYNC_LABEL=$1
  AXO_SYNC_EXPECTED=$2
  AXO_SYNC_ACTUAL=$3
  [ "$AXO_SYNC_ACTUAL" = "$AXO_SYNC_EXPECTED" ] || {
    echo "$AXO_SYNC_LABEL does not contain the exact reviewed file set." >&2
    echo "Expected:" >&2
    printf '%s\n' "$AXO_SYNC_EXPECTED" >&2
    echo "Found:" >&2
    printf '%s\n' "$AXO_SYNC_ACTUAL" >&2
    exit 1
  }
}

require_regular_mirror_entries() {
  AXO_SYNC_REGULAR_DIRECTORY=$1
  for AXO_SYNC_REGULAR_ENTRY in \
    "$AXO_SYNC_REGULAR_DIRECTORY"/* \
    "$AXO_SYNC_REGULAR_DIRECTORY"/.[!.]* \
    "$AXO_SYNC_REGULAR_DIRECTORY"/..?*; do
    [ -e "$AXO_SYNC_REGULAR_ENTRY" ] || [ -L "$AXO_SYNC_REGULAR_ENTRY" ] || continue
    if [ ! -f "$AXO_SYNC_REGULAR_ENTRY" ] || [ -L "$AXO_SYNC_REGULAR_ENTRY" ]; then
      echo "Refusing non-regular package-local mirror entry: $AXO_SYNC_REGULAR_ENTRY" >&2
      exit 1
    fi
  done
}

remove_unexpected_mirror_files() {
  AXO_SYNC_CLEAN_DIRECTORY=$1
  AXO_SYNC_CLEAN_EXPECTED=$2
  for AXO_SYNC_CLEAN_FILE in \
    "$AXO_SYNC_CLEAN_DIRECTORY"/* \
    "$AXO_SYNC_CLEAN_DIRECTORY"/.[!.]* \
    "$AXO_SYNC_CLEAN_DIRECTORY"/..?*; do
    [ -e "$AXO_SYNC_CLEAN_FILE" ] || continue
    [ -f "$AXO_SYNC_CLEAN_FILE" ] || {
      echo "Refusing unexpected non-file mirror entry: $AXO_SYNC_CLEAN_FILE" >&2
      exit 1
    }
    AXO_SYNC_CLEAN_NAME="$(basename -- "$AXO_SYNC_CLEAN_FILE")"
    if ! printf '%s\n' "$AXO_SYNC_CLEAN_EXPECTED" | grep -Fxq "$AXO_SYNC_CLEAN_NAME"; then
      rm -- "$AXO_SYNC_CLEAN_FILE"
    fi
  done
}

AXO_SYNC_LATTICE_SOURCE="$AXO_SYNC_REPO_ROOT/packages/lattice/src"
AXO_SYNC_LATTICE_DESTINATION="$AXO_SYNC_REPO_ROOT/axocoatl-server/static/lattice"
AXO_SYNC_BRAND_SOURCE="$AXO_SYNC_REPO_ROOT/branding"
AXO_SYNC_BRAND_DESTINATION="$AXO_SYNC_REPO_ROOT/axocoatl-server/static/brand"
AXO_SYNC_EXPECTED_LATTICE="$(
  printf '%s\n' \
    index.js lattice.js node.js handle.js edge.js minimap.js controls.js \
    viewport.js selection.js geometry.js history.js layout.js | LC_ALL=C sort
)"
AXO_SYNC_EXPECTED_BRAND="$(
  printf '%s\n' \
    mark.png favicon.png wordmark.png wordmark-ink.png wordmark-vellum.png \
    colors.json mcp-catalog.json | LC_ALL=C sort
)"

if [ "$AXO_SYNC_MODE" = write ]; then
  mkdir -p "$AXO_SYNC_LATTICE_DESTINATION" "$AXO_SYNC_BRAND_DESTINATION"
fi

require_regular_mirror_entries "$AXO_SYNC_LATTICE_DESTINATION"
require_regular_mirror_entries "$AXO_SYNC_BRAND_DESTINATION"

if [ "$AXO_SYNC_MODE" = write ]; then
  remove_unexpected_mirror_files \
    "$AXO_SYNC_LATTICE_DESTINATION" "$AXO_SYNC_EXPECTED_LATTICE"
  remove_unexpected_mirror_files \
    "$AXO_SYNC_BRAND_DESTINATION" "$AXO_SYNC_EXPECTED_BRAND"
fi

for AXO_SYNC_FILE in \
  index.js lattice.js node.js handle.js edge.js minimap.js controls.js \
  viewport.js selection.js geometry.js history.js layout.js; do
  sync_asset \
    "$AXO_SYNC_LATTICE_SOURCE/$AXO_SYNC_FILE" \
    "$AXO_SYNC_LATTICE_DESTINATION/$AXO_SYNC_FILE"
done

for AXO_SYNC_FILE in \
  mark.png favicon.png wordmark.png wordmark-ink.png wordmark-vellum.png \
  colors.json mcp-catalog.json; do
  sync_asset \
    "$AXO_SYNC_BRAND_SOURCE/$AXO_SYNC_FILE" \
    "$AXO_SYNC_BRAND_DESTINATION/$AXO_SYNC_FILE"
done

require_exact_files \
  "Canonical Lattice source" \
  "$AXO_SYNC_EXPECTED_LATTICE" \
  "$(list_javascript_files "$AXO_SYNC_LATTICE_SOURCE")"
require_exact_files \
  "Package-local Lattice mirror" \
  "$AXO_SYNC_EXPECTED_LATTICE" \
  "$(list_all_files "$AXO_SYNC_LATTICE_DESTINATION")"
require_exact_files \
  "Package-local brand mirror" \
  "$AXO_SYNC_EXPECTED_BRAND" \
  "$(list_all_files "$AXO_SYNC_BRAND_DESTINATION")"

if [ "$AXO_SYNC_MODE" = check ]; then
  echo "Server embedded asset mirrors: PASS"
else
  echo "Server embedded asset mirrors synchronized."
fi
