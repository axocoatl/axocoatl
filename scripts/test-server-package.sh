#!/usr/bin/env bash
# Prove the published axocoatl-server crate is self-contained without waiting
# for the irreversible crates.io publication job. Cargo cannot verify this one
# package by itself before the other Axocoatl 1.0 crates exist in a registry, so
# package the complete local publish set, inspect the real server archive, then
# compile that extracted archive against local path patches.
set -euo pipefail

AXO_PACKAGE_SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
AXO_PACKAGE_REPO_ROOT="$(CDPATH= cd -- "$AXO_PACKAGE_SCRIPT_DIR/.." && pwd)"
AXO_PACKAGE_ARCHIVE_CEILING=9500000
AXO_PACKAGE_WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-server-package.XXXXXX")"

cleanup() {
  if [[ -n "${AXO_PACKAGE_WORK_DIR:-}" && -d "$AXO_PACKAGE_WORK_DIR" ]]; then
    rm -rf -- "$AXO_PACKAGE_WORK_DIR"
  fi
}
trap cleanup EXIT HUP INT TERM

fail() {
  echo "server-package: $*" >&2
  exit 1
}

for AXO_PACKAGE_TOOL in cargo cmp git jq mktemp sort tar tr wc; do
  command -v "$AXO_PACKAGE_TOOL" >/dev/null 2>&1 \
    || fail "required tool is missing: $AXO_PACKAGE_TOOL"
done

cd "$AXO_PACKAGE_REPO_ROOT"
"$AXO_PACKAGE_SCRIPT_DIR/sync-server-embedded-assets.sh" --check

AXO_PACKAGE_METADATA="$(cargo metadata --locked --offline --no-deps --format-version 1)"
AXO_PACKAGE_SERVER_VERSION="$(
  jq -er '
    [.packages[] | select(.name == "axocoatl-server") | .version]
    | if length == 1 then .[0] else error("expected one axocoatl-server package") end
  ' <<<"$AXO_PACKAGE_METADATA"
)"

AXO_PACKAGE_NAMES=()
while IFS= read -r AXO_PACKAGE_NAME; do
  [[ -n "$AXO_PACKAGE_NAME" ]] && AXO_PACKAGE_NAMES+=("$AXO_PACKAGE_NAME")
done < <(
  jq -r \
    '.packages[] | select(.source == null and .publish == null) | .name' \
    <<<"$AXO_PACKAGE_METADATA" | LC_ALL=C sort
)
[[ "${#AXO_PACKAGE_NAMES[@]}" -gt 0 ]] \
  || fail "cargo metadata returned no publishable workspace packages"

AXO_PACKAGE_SELECTION_ARGS=()
AXO_PACKAGE_SERVER_SELECTED=no
for AXO_PACKAGE_NAME in "${AXO_PACKAGE_NAMES[@]}"; do
  AXO_PACKAGE_SELECTION_ARGS+=(-p "$AXO_PACKAGE_NAME")
  if [[ "$AXO_PACKAGE_NAME" == axocoatl-server ]]; then
    AXO_PACKAGE_SERVER_SELECTED=yes
  fi
done
[[ "$AXO_PACKAGE_SERVER_SELECTED" == yes ]] \
  || fail "the publishable workspace set does not contain axocoatl-server"

# CI packages a clean checkout. Local release audits often run against the
# intended dirty candidate, where Cargo requires the explicit opt-in.
AXO_PACKAGE_IS_DIRTY=no
if [[ -n "$(git status --porcelain --untracked-files=normal)" ]]; then
  AXO_PACKAGE_IS_DIRTY=yes
  echo "server-package: working tree is dirty; enabling Cargo's local audit mode"
fi

AXO_PACKAGE_TARGET="$AXO_PACKAGE_WORK_DIR/package-target"
package_workspace() {
  cargo package \
    --locked \
    --offline \
    --no-verify \
    --target-dir "$AXO_PACKAGE_TARGET" \
    "$@" \
    "${AXO_PACKAGE_SELECTION_ARGS[@]}"
}
if [[ "$AXO_PACKAGE_IS_DIRTY" == yes ]]; then
  package_workspace --allow-dirty
else
  package_workspace
fi

AXO_PACKAGE_ARCHIVE="$AXO_PACKAGE_TARGET/package/axocoatl-server-$AXO_PACKAGE_SERVER_VERSION.crate"
[[ -f "$AXO_PACKAGE_ARCHIVE" ]] \
  || fail "Cargo did not create $AXO_PACKAGE_ARCHIVE"
AXO_PACKAGE_ARCHIVE_BYTES="$(wc -c < "$AXO_PACKAGE_ARCHIVE" | tr -d '[:space:]')"
[[ "$AXO_PACKAGE_ARCHIVE_BYTES" =~ ^[0-9]+$ ]] \
  || fail "could not determine the server archive size"
(( AXO_PACKAGE_ARCHIVE_BYTES <= AXO_PACKAGE_ARCHIVE_CEILING )) \
  || fail "server crate is $AXO_PACKAGE_ARCHIVE_BYTES bytes, above the reviewed $AXO_PACKAGE_ARCHIVE_CEILING-byte ceiling"

AXO_PACKAGE_EXTRACT_ROOT="$AXO_PACKAGE_WORK_DIR/extracted"
mkdir -p "$AXO_PACKAGE_EXTRACT_ROOT"
tar -xzf "$AXO_PACKAGE_ARCHIVE" -C "$AXO_PACKAGE_EXTRACT_ROOT"
AXO_PACKAGE_EXTRACTED="$AXO_PACKAGE_EXTRACT_ROOT/axocoatl-server-$AXO_PACKAGE_SERVER_VERSION"
[[ -f "$AXO_PACKAGE_EXTRACTED/Cargo.toml" ]] \
  || fail "the extracted server crate has no Cargo.toml"

AXO_PACKAGE_LATTICE_FILES=(
  index.js lattice.js node.js handle.js edge.js minimap.js controls.js
  viewport.js selection.js geometry.js history.js layout.js
)
AXO_PACKAGE_BRAND_FILES=(
  mark.png favicon.png wordmark.png wordmark-ink.png wordmark-vellum.png
  colors.json mcp-catalog.json
)

for AXO_PACKAGE_FILE in "${AXO_PACKAGE_LATTICE_FILES[@]}"; do
  cmp -s \
    "$AXO_PACKAGE_REPO_ROOT/packages/lattice/src/$AXO_PACKAGE_FILE" \
    "$AXO_PACKAGE_EXTRACTED/static/lattice/$AXO_PACKAGE_FILE" \
    || fail "packaged Lattice asset is missing or stale: $AXO_PACKAGE_FILE"
done
for AXO_PACKAGE_FILE in "${AXO_PACKAGE_BRAND_FILES[@]}"; do
  cmp -s \
    "$AXO_PACKAGE_REPO_ROOT/branding/$AXO_PACKAGE_FILE" \
    "$AXO_PACKAGE_EXTRACTED/static/brand/$AXO_PACKAGE_FILE" \
    || fail "packaged brand asset is missing or stale: $AXO_PACKAGE_FILE"
done

# The extracted manifest correctly names registry dependencies. Patch only its
# direct Axocoatl dependencies back to this checkout so this pre-publication
# compile proves the archive contents without pretending those 1.0 crates are
# already available from crates.io. The temporary lockfile may update, so this
# check is deliberately offline but not --locked.
AXO_PACKAGE_PATCH_ARGS=()
while IFS=$'\t' read -r AXO_PACKAGE_DEPENDENCY AXO_PACKAGE_PATH_LITERAL; do
  [[ -n "$AXO_PACKAGE_DEPENDENCY" && -n "$AXO_PACKAGE_PATH_LITERAL" ]] \
    || continue
  AXO_PACKAGE_PATCH_ARGS+=(
    --config "patch.crates-io.${AXO_PACKAGE_DEPENDENCY}.path=${AXO_PACKAGE_PATH_LITERAL}"
  )
done < <(
  jq -r '
    .packages[]
    | select(.name == "axocoatl-server")
    | .dependencies[]
    | select(.path != null)
    | [.name, (.path | @json)]
    | @tsv
  ' <<<"$AXO_PACKAGE_METADATA"
)
[[ "${#AXO_PACKAGE_PATCH_ARGS[@]}" -gt 0 ]] \
  || fail "axocoatl-server has no local dependencies to patch"

AXO_PACKAGE_COMPILE_TARGET="${CARGO_TARGET_DIR:-$AXO_PACKAGE_REPO_ROOT/target}"
cargo check \
  --offline \
  --manifest-path "$AXO_PACKAGE_EXTRACTED/Cargo.toml" \
  --target-dir "$AXO_PACKAGE_COMPILE_TARGET" \
  "${AXO_PACKAGE_PATCH_ARGS[@]}"

echo "Server crate package regression: PASS ($AXO_PACKAGE_ARCHIVE_BYTES bytes)"
