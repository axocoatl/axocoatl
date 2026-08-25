#!/bin/sh
# Verify or regenerate the legal notices shipped with the Axocoatl binary.
set -eu

AXO_SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
AXO_REPO_ROOT="$(CDPATH= cd -- "$AXO_SCRIPT_DIR/.." && pwd)"
AXO_NOTICE_FILE="$AXO_REPO_ROOT/axocoatl-server/THIRD_PARTY_LICENSES.txt"
AXO_VENDOR_MANIFEST="$AXO_REPO_ROOT/licenses/vendor-assets.sha256"
AXO_VENDOR_WEB_DIR="$AXO_REPO_ROOT/licenses/vendor-web"
AXO_VENDOR_WEB_LOCK="$AXO_VENDOR_WEB_DIR/package-lock.json"
AXO_MONACO_BUILD="$AXO_VENDOR_WEB_DIR/monaco-build.json"
AXO_EXPECTED_VENDOR_COUNT=161
AXO_EXPECTED_MONACO_COUNT=151
AXO_CARGO_ABOUT_VERSION=0.9.1

fail() {
  echo "third-party-licenses: $*" >&2
  exit 1
}

usage() {
  cat >&2 <<'EOF'
Usage:
  check-third-party-licenses.sh          # verify checked-in notices
  check-third-party-licenses.sh --write  # regenerate checked-in notices
EOF
  exit 2
}

case "${1:-}" in
  "") AXO_MODE=check ;;
  --write) AXO_MODE=write ;;
  *) usage ;;
esac
[ "$#" -le 1 ] || usage

AXO_CARGO_HOME="${CARGO_HOME:-${HOME:?HOME is required}/.cargo}"
if [ -d "$AXO_CARGO_HOME/bin" ]; then
  AXO_TOOL_PATH="$AXO_CARGO_HOME/bin:$PATH"
  export PATH="$AXO_TOOL_PATH"
fi

for AXO_TOOL in cargo cargo-about npm jq find sort cmp diff grep sed mktemp; do
  command -v "$AXO_TOOL" >/dev/null 2>&1 \
    || fail "required tool is missing: $AXO_TOOL"
done

AXO_ABOUT_VERSION="$(cargo-about --version)"
[ "$AXO_ABOUT_VERSION" = "cargo-about $AXO_CARGO_ABOUT_VERSION" ] \
  || fail "expected cargo-about $AXO_CARGO_ABOUT_VERSION, got '$AXO_ABOUT_VERSION'"

AXO_WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-licenses.XXXXXX")" \
  || fail "could not create a temporary directory"

cleanup() {
  if [ -n "${AXO_WORK_DIR:-}" ] && [ -d "$AXO_WORK_DIR" ]; then
    rm -rf -- "$AXO_WORK_DIR"
  fi
}
trap cleanup 0 HUP INT TERM

sha256_file() {
  AXO_HASH_INPUT=$1
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$AXO_HASH_INPUT" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$AXO_HASH_INPUT" | awk '{print $1}'
  else
    fail "neither sha256sum nor shasum is available"
  fi
}

AXO_CURRENT_VENDOR_MANIFEST="$AXO_WORK_DIR/vendor-assets.sha256"
(
  cd "$AXO_REPO_ROOT"
  find axocoatl-server/static/vendor -type f -print | LC_ALL=C sort |
    while IFS= read -r AXO_ASSET_FILE; do
      AXO_ASSET_DIGEST="$(sha256_file "$AXO_ASSET_FILE")"
      printf '%s  %s\n' "$AXO_ASSET_DIGEST" "$AXO_ASSET_FILE"
    done
) > "$AXO_CURRENT_VENDOR_MANIFEST"

AXO_VENDOR_COUNT="$(wc -l < "$AXO_CURRENT_VENDOR_MANIFEST" | tr -d '[:space:]')"
[ "$AXO_VENDOR_COUNT" = "$AXO_EXPECTED_VENDOR_COUNT" ] \
  || fail "expected $AXO_EXPECTED_VENDOR_COUNT embedded vendor assets, found $AXO_VENDOR_COUNT"

AXO_EXPECTED_NON_MONACO="$AXO_WORK_DIR/expected-non-monaco.txt"
cat > "$AXO_EXPECTED_NON_MONACO" <<'EOF'
axocoatl-server/static/vendor/codicons/codicon.css
axocoatl-server/static/vendor/codicons/codicon.ttf
axocoatl-server/static/vendor/fonts/jetbrains-mono-var.woff2
axocoatl-server/static/vendor/fonts/space-grotesk-var.woff2
axocoatl-server/static/vendor/highlight.css
axocoatl-server/static/vendor/highlight.min.js
axocoatl-server/static/vendor/markdown-it.min.js
axocoatl-server/static/vendor/xterm-addon-fit.js
axocoatl-server/static/vendor/xterm.css
axocoatl-server/static/vendor/xterm.js
EOF
AXO_CURRENT_NON_MONACO="$AXO_WORK_DIR/current-non-monaco.txt"
(
  cd "$AXO_REPO_ROOT"
  find axocoatl-server/static/vendor -type f \
    ! -path 'axocoatl-server/static/vendor/monaco/*' -print | LC_ALL=C sort
) > "$AXO_CURRENT_NON_MONACO"
if ! cmp -s "$AXO_EXPECTED_NON_MONACO" "$AXO_CURRENT_NON_MONACO"; then
  diff -u "$AXO_EXPECTED_NON_MONACO" "$AXO_CURRENT_NON_MONACO" >&2 || true
  fail "embedded vendor asset families changed without a provenance-contract update"
fi
AXO_MONACO_COUNT="$(
  find "$AXO_REPO_ROOT/axocoatl-server/static/vendor/monaco" -type f -print |
    wc -l | tr -d '[:space:]'
)"
[ "$AXO_MONACO_COUNT" = "$AXO_EXPECTED_MONACO_COUNT" ] \
  || fail "expected the reviewed $AXO_EXPECTED_MONACO_COUNT-file Monaco payload, found $AXO_MONACO_COUNT files"

AXO_EXPECTED_VENDOR_LICENSES="$AXO_WORK_DIR/expected-vendor-licenses.txt"
find "$AXO_REPO_ROOT/licenses/vendor" -maxdepth 1 -type f -name '*.txt' -print |
  sed 's#^.*/#vendor/#' | LC_ALL=C sort -u > "$AXO_EXPECTED_VENDOR_LICENSES"
AXO_REFERENCED_VENDOR_LICENSES="$AXO_WORK_DIR/referenced-vendor-licenses.txt"
grep -Eo '`vendor/[^`]+\.txt`' "$AXO_REPO_ROOT/licenses/VENDORED_ASSETS.md" |
  sed 's/^`//; s/`$//' | LC_ALL=C sort -u > "$AXO_REFERENCED_VENDOR_LICENSES"
if ! cmp -s "$AXO_EXPECTED_VENDOR_LICENSES" "$AXO_REFERENCED_VENDOR_LICENSES"; then
  diff -u "$AXO_EXPECTED_VENDOR_LICENSES" "$AXO_REFERENCED_VENDOR_LICENSES" >&2 || true
  fail "vendor license files and provenance references are not one complete set"
fi

if ! cmp -s "$AXO_VENDOR_MANIFEST" "$AXO_CURRENT_VENDOR_MANIFEST"; then
  if [ "$AXO_MODE" = write ]; then
    cp "$AXO_CURRENT_VENDOR_MANIFEST" "$AXO_VENDOR_MANIFEST"
  else
    diff -u "$AXO_VENDOR_MANIFEST" "$AXO_CURRENT_VENDOR_MANIFEST" >&2 || true
    fail "embedded vendor assets do not match the reviewed SHA-256 inventory"
  fi
fi

[ -f "$AXO_VENDOR_WEB_LOCK" ] || fail "vendored-browser audit lockfile is missing"
[ -f "$AXO_MONACO_BUILD" ] || fail "Monaco patched-build metadata is missing"

AXO_LOCK_MONACO_VERSION="$(jq -r '.packages["node_modules/monaco-editor"].version // empty' "$AXO_VENDOR_WEB_LOCK")"
[ "$AXO_LOCK_MONACO_VERSION" = "0.56.0" ] \
  || fail "expected audited Monaco 0.56.0, found '$AXO_LOCK_MONACO_VERSION'"
AXO_LOCK_DOMPURIFY_VERSION="$(jq -r '.packages["node_modules/dompurify"].version // empty' "$AXO_VENDOR_WEB_LOCK")"
[ "$AXO_LOCK_DOMPURIFY_VERSION" = "3.4.13" ] \
  || fail "expected audited DOMPurify 3.4.13, found '$AXO_LOCK_DOMPURIFY_VERSION'"
AXO_LOCK_MARKDOWN_IT_VERSION="$(jq -r '.packages["node_modules/markdown-it"].version // empty' "$AXO_VENDOR_WEB_LOCK")"
[ "$AXO_LOCK_MARKDOWN_IT_VERSION" = "14.3.0" ] \
  || fail "expected audited markdown-it 14.3.0, found '$AXO_LOCK_MARKDOWN_IT_VERSION'"
AXO_OVERRIDE_DOMPURIFY_VERSION="$(jq -r '.overrides["monaco-editor"].dompurify // empty' "$AXO_VENDOR_WEB_DIR/package.json")"
[ "$AXO_OVERRIDE_DOMPURIFY_VERSION" = "3.4.13" ] \
  || fail "Monaco must remain overridden to audited DOMPurify 3.4.13"

AXO_BUILD_MONACO_VERSION="$(jq -r '.monaco_editor // empty' "$AXO_MONACO_BUILD")"
[ "$AXO_BUILD_MONACO_VERSION" = "$AXO_LOCK_MONACO_VERSION" ] \
  || fail "Monaco build metadata and audit lock disagree"
AXO_BUILD_DOMPURIFY_VERSION="$(jq -r '.dompurify // empty' "$AXO_MONACO_BUILD")"
[ "$AXO_BUILD_DOMPURIFY_VERSION" = "$AXO_LOCK_DOMPURIFY_VERSION" ] \
  || fail "DOMPurify build metadata and audit lock disagree"
AXO_BUILD_DOMPURIFY_FIXED="$(jq -r '.dompurify_advisory_fixed // empty' "$AXO_MONACO_BUILD")"
[ "$AXO_BUILD_DOMPURIFY_FIXED" = "$AXO_LOCK_DOMPURIFY_VERSION" ] \
  || fail "DOMPurify advisory floor and audit lock disagree"
AXO_BUILD_DOMPURIFY_AFFECTED="$(jq -r '.dompurify_advisory_affected // empty' "$AXO_MONACO_BUILD")"
[ "$AXO_BUILD_DOMPURIFY_AFFECTED" = '<=3.4.12' ] \
  || fail "DOMPurify reviewed advisory range is missing from build metadata"
AXO_BUILD_DOMPURIFY_ADVISORY="$(jq -r '.dompurify_security_advisory // empty' "$AXO_MONACO_BUILD")"
[ "$AXO_BUILD_DOMPURIFY_ADVISORY" = \
  'https://github.com/cure53/DOMPurify/security/advisories/GHSA-55q2-fjhq-7xh7' ] \
  || fail "DOMPurify reviewed advisory provenance is missing from build metadata"
AXO_BUILD_MONACO_ISSUE="$(jq -r '.upstream_bundled_dompurify_issue // empty' "$AXO_MONACO_BUILD")"
[ "$AXO_BUILD_MONACO_ISSUE" = \
  'https://github.com/microsoft/monaco-editor/issues/5454' ] \
  || fail "Monaco bundled-DOMPurify issue provenance is missing from build metadata"
AXO_BUILD_PAYLOAD_FILES="$(jq -r '.payload_files // empty' "$AXO_MONACO_BUILD")"
[ "$AXO_BUILD_PAYLOAD_FILES" = "$AXO_EXPECTED_MONACO_COUNT" ] \
  || fail "Monaco build metadata has the wrong payload file count"
AXO_MONACO_ENTRY="$(jq -r '.entry_chunk // empty' "$AXO_MONACO_BUILD")"
case "$AXO_MONACO_ENTRY" in
  *[!A-Za-z0-9._-]*|""|.*) fail "invalid Monaco entry chunk in build metadata" ;;
esac
AXO_MONACO_ENTRY_FILE="$AXO_REPO_ROOT/axocoatl-server/static/vendor/monaco/vs/$AXO_MONACO_ENTRY"
[ -f "$AXO_MONACO_ENTRY_FILE" ] \
  || fail "documented Monaco entry chunk is missing: $AXO_MONACO_ENTRY"
AXO_MONACO_ENTRY_EXPECTED="$(jq -r '.entry_chunk_sha256 // empty' "$AXO_MONACO_BUILD")"
AXO_MONACO_ENTRY_ACTUAL="$(sha256_file "$AXO_MONACO_ENTRY_FILE")"
[ "$AXO_MONACO_ENTRY_ACTUAL" = "$AXO_MONACO_ENTRY_EXPECTED" ] \
  || fail "Monaco entry chunk does not match the reviewed DOMPurify 3.4.13 build"
AXO_EDITOR_MAIN_FILE="$AXO_REPO_ROOT/axocoatl-server/static/vendor/monaco/vs/editor/editor.main.js"
[ -f "$AXO_EDITOR_MAIN_FILE" ] || fail "Monaco editor.main.js is missing"
AXO_EDITOR_MAIN_EXPECTED="$(jq -r '.editor_main_sha256 // empty' "$AXO_MONACO_BUILD")"
AXO_EDITOR_MAIN_ACTUAL="$(sha256_file "$AXO_EDITOR_MAIN_FILE")"
[ "$AXO_EDITOR_MAIN_ACTUAL" = "$AXO_EDITOR_MAIN_EXPECTED" ] \
  || fail "Monaco editor.main.js does not match the reviewed patched build"

for AXO_MONACO_NOTICE_MARKER in \
  '%% markedjs NOTICES AND INFORMATION BEGIN HERE' \
  '%% vscode-swift version 0.0.1'; do
  grep -Fq "$AXO_MONACO_NOTICE_MARKER" \
    "$AXO_REPO_ROOT/licenses/vendor/monaco-editor-ThirdPartyNotices.txt" \
    || fail "Monaco upstream third-party notice is incomplete: $AXO_MONACO_NOTICE_MARKER"
done
grep -Fq 'Mozilla Public License Version 2.0' \
  "$AXO_REPO_ROOT/licenses/vendor/dompurify-3.4.13-MPL-2.0.txt" \
  || fail "DOMPurify MPL-2.0 text is missing"
grep -Fq 'Copyright (c) Cure53 and other contributors.' \
  "$AXO_REPO_ROOT/licenses/vendor/dompurify-3.4.13-NOTICE.txt" \
  || fail "DOMPurify copyright attribution is missing"

(cd "$AXO_VENDOR_WEB_DIR" && npm audit --package-lock-only --omit=dev --audit-level=low)

AXO_ABOUT_JSON="$AXO_WORK_DIR/cargo-about.json"
AXO_RUST_NOTICES="$AXO_WORK_DIR/rust-licenses.txt"

cargo-about generate \
  --config "$AXO_REPO_ROOT/about.toml" \
  --frozen \
  --all-features \
  --fail \
  --manifest-path "$AXO_REPO_ROOT/axocoatl-cli/Cargo.toml" \
  --format json > "$AXO_ABOUT_JSON"
cargo-about generate \
  --config "$AXO_REPO_ROOT/about.toml" \
  --frozen \
  --all-features \
  --fail \
  --manifest-path "$AXO_REPO_ROOT/axocoatl-cli/Cargo.toml" \
  "$AXO_REPO_ROOT/about.hbs" \
  --output-file "$AXO_RUST_NOTICES"

AXO_AUXILIARY_LIST="$AXO_WORK_DIR/rust-auxiliary.tsv"
jq -r '
  .crates[]
  | select(.package.source != null)
  | [.package.name, .package.version, .package.manifest_path]
  | @tsv
' "$AXO_ABOUT_JSON" | LC_ALL=C sort -u |
  while IFS="$(printf '\t')" read -r AXO_CRATE_NAME AXO_CRATE_VERSION AXO_MANIFEST_FILE; do
    AXO_CRATE_DIR=${AXO_MANIFEST_FILE%/Cargo.toml}
    find "$AXO_CRATE_DIR" -maxdepth 1 -type f \
      \( -iname 'NOTICE*' -o -iname 'COPYRIGHT*' -o -iname 'AUTHORS*' \
         -o -iname 'THIRD*PARTY*' \) -print |
      while IFS= read -r AXO_AUXILIARY_FILE; do
        printf '%s\t%s\t%s\t%s\n' \
          "$AXO_CRATE_NAME" \
          "$AXO_CRATE_VERSION" \
          "${AXO_AUXILIARY_FILE##*/}" \
          "$AXO_AUXILIARY_FILE"
      done
  done | LC_ALL=C sort -u > "$AXO_AUXILIARY_LIST"

AXO_GENERATED="$AXO_WORK_DIR/THIRD_PARTY_LICENSES.txt"
{
  cat <<'EOF'
AXOCOATL THIRD-PARTY LICENSES
============================

This file is generated from Cargo.lock and the checked-in embedded-asset
inventory. Do not edit it directly. Regenerate it with
scripts/check-third-party-licenses.sh --write using cargo-about 0.9.1.

Axocoatl itself is licensed under Apache-2.0; see the adjacent LICENSE file.
The notices below cover third-party material embedded in or statically linked
into the native Axocoatl distribution.

EMBEDDED BROWSER ASSETS
=======================

EOF
  cat "$AXO_REPO_ROOT/licenses/VENDORED_ASSETS.md"
  printf '\n'

  find "$AXO_REPO_ROOT/licenses/vendor" -maxdepth 1 -type f -name '*.txt' -print |
    LC_ALL=C sort |
    while IFS= read -r AXO_VENDOR_LICENSE; do
      AXO_VENDOR_LICENSE_NAME=${AXO_VENDOR_LICENSE##*/}
      printf '%s\n' '-------------------------------------------------------------------------------'
      printf '%s\n\n' "$AXO_VENDOR_LICENSE_NAME"
      cat "$AXO_VENDOR_LICENSE"
      printf '\n\n'
    done

  cat "$AXO_RUST_NOTICES"

  if [ -s "$AXO_AUXILIARY_LIST" ]; then
    cat <<'EOF'

RUST DEPENDENCY AUXILIARY NOTICES
=================================

These top-level NOTICE, COPYRIGHT, AUTHORS, and THIRD_PARTY files accompany
the license texts above. They are copied verbatim from the exact Cargo.lock
package sources used to generate this release graph.

EOF
    while IFS="$(printf '\t')" read -r AXO_CRATE_NAME AXO_CRATE_VERSION AXO_AUXILIARY_NAME AXO_AUXILIARY_FILE; do
      printf '%s\n' '-------------------------------------------------------------------------------'
      printf '%s %s — %s\n\n' \
        "$AXO_CRATE_NAME" "$AXO_CRATE_VERSION" "$AXO_AUXILIARY_NAME"
      cat "$AXO_AUXILIARY_FILE"
      printf '\n\n'
    done < "$AXO_AUXILIARY_LIST"
  fi
} > "$AXO_GENERATED"

case "$AXO_MODE" in
  write)
    cp "$AXO_GENERATED" "$AXO_NOTICE_FILE"
    echo "third-party-licenses: wrote ${AXO_NOTICE_FILE#$AXO_REPO_ROOT/}"
    ;;
  check)
    [ -f "$AXO_NOTICE_FILE" ] \
      || fail "checked-in notice is missing; run scripts/check-third-party-licenses.sh --write"
    if ! cmp -s "$AXO_NOTICE_FILE" "$AXO_GENERATED"; then
      diff -u "$AXO_NOTICE_FILE" "$AXO_GENERATED" >&2 || true
      fail "checked-in notice is stale; run scripts/check-third-party-licenses.sh --write"
    fi
    echo "Third-party license and embedded-asset contract: PASS"
    ;;
esac
