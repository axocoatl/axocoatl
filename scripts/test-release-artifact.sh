#!/bin/sh
# Exercise native packaging and the complete four-target artifact contract.
set -eu

AXO_SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
AXO_REPO_ROOT="$(CDPATH= cd -- "$AXO_SCRIPT_DIR/.." && pwd)"
AXO_RELEASE_SCRIPT="$AXO_SCRIPT_DIR/release-artifact.sh"
AXO_WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-release-test.XXXXXX")"

cleanup() {
  if [ -n "${AXO_WORK_DIR:-}" ] && [ -d "$AXO_WORK_DIR" ]; then
    rm -rf -- "$AXO_WORK_DIR"
  fi
}
trap cleanup 0 HUP INT TERM

fail() {
  echo "test-release-artifact: $*" >&2
  exit 1
}

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

case "$(uname -s):$(uname -m)" in
  Linux:x86_64|Linux:amd64) AXO_NATIVE_TARGET=x86_64-unknown-linux-gnu ;;
  Linux:aarch64|Linux:arm64) AXO_NATIVE_TARGET=aarch64-unknown-linux-gnu ;;
  Darwin:x86_64|Darwin:amd64) AXO_NATIVE_TARGET=x86_64-apple-darwin ;;
  Darwin:arm64|Darwin:aarch64) AXO_NATIVE_TARGET=aarch64-apple-darwin ;;
  *) fail "unsupported test host" ;;
esac

AXO_FIXTURE_BINARY="$AXO_WORK_DIR/axocoatl"
cat > "$AXO_FIXTURE_BINARY" <<'EOF'
#!/bin/sh
echo 'axocoatl 1.0.0'
EOF
chmod 0755 "$AXO_FIXTURE_BINARY"

AXO_DIST_DIR="$AXO_WORK_DIR/dist"
"$AXO_RELEASE_SCRIPT" package \
  v1.0.0 "$AXO_NATIVE_TARGET" "$AXO_FIXTURE_BINARY" "$AXO_DIST_DIR"

AXO_NATIVE_ARCHIVE="$AXO_DIST_DIR/axocoatl-v1.0.0-$AXO_NATIVE_TARGET.tar.gz"
AXO_EXTRACT_DIR="$AXO_WORK_DIR/extracted"
mkdir -p "$AXO_EXTRACT_DIR"
tar -xzf "$AXO_NATIVE_ARCHIVE" -C "$AXO_EXTRACT_DIR"
cmp -s "$AXO_REPO_ROOT/LICENSE" "$AXO_EXTRACT_DIR/LICENSE" \
  || fail "packaged Axocoatl license differs from the repository source"
cmp -s \
  "$AXO_REPO_ROOT/axocoatl-server/THIRD_PARTY_LICENSES.txt" \
  "$AXO_EXTRACT_DIR/THIRD_PARTY_LICENSES.txt" \
  || fail "packaged third-party notice differs from the generated source"

for AXO_TARGET in \
  x86_64-unknown-linux-gnu \
  aarch64-unknown-linux-gnu \
  x86_64-apple-darwin \
  aarch64-apple-darwin
do
  AXO_ARCHIVE="$AXO_DIST_DIR/axocoatl-v1.0.0-$AXO_TARGET.tar.gz"
  if [ ! -f "$AXO_ARCHIVE" ]; then
    cp "$AXO_NATIVE_ARCHIVE" "$AXO_ARCHIVE"
  fi
  AXO_DIGEST="$(sha256_file "$AXO_ARCHIVE" | tr 'A-F' 'a-f')"
  printf '%s  %s\n' "$AXO_DIGEST" "${AXO_ARCHIVE##*/}" > "$AXO_ARCHIVE.sha256"
done

AXO_RELEASE_FILE_COUNT="$(find "$AXO_DIST_DIR" -mindepth 1 -maxdepth 1 -print | wc -l | tr -d '[:space:]')"
[ "$AXO_RELEASE_FILE_COUNT" = 8 ] \
  || fail "four-target release fixture should contain exactly eight files"

"$AXO_RELEASE_SCRIPT" verify-set v1.0.0 "$AXO_DIST_DIR" \
  x86_64-unknown-linux-gnu \
  aarch64-unknown-linux-gnu \
  x86_64-apple-darwin \
  aarch64-apple-darwin

AXO_BAD_DIR="$AXO_WORK_DIR/missing-notices"
AXO_BAD_STAGE="$AXO_WORK_DIR/missing-notices-stage"
mkdir -p "$AXO_BAD_DIR" "$AXO_BAD_STAGE"
cp "$AXO_FIXTURE_BINARY" "$AXO_BAD_STAGE/axocoatl"
AXO_BAD_ARCHIVE="$AXO_BAD_DIR/axocoatl-v1.0.0-$AXO_NATIVE_TARGET.tar.gz"
tar -czf "$AXO_BAD_ARCHIVE" -C "$AXO_BAD_STAGE" axocoatl
AXO_BAD_DIGEST="$(sha256_file "$AXO_BAD_ARCHIVE" | tr 'A-F' 'a-f')"
printf '%s  %s\n' "$AXO_BAD_DIGEST" "${AXO_BAD_ARCHIVE##*/}" > "$AXO_BAD_ARCHIVE.sha256"

set +e
"$AXO_RELEASE_SCRIPT" verify-set \
  v1.0.0 "$AXO_BAD_DIR" "$AXO_NATIVE_TARGET" \
  > "$AXO_WORK_DIR/missing-notices.out" 2>&1
AXO_EXIT_CODE=$?
set -e
[ "$AXO_EXIT_CODE" -ne 0 ] \
  || fail "archive without required license files unexpectedly verified"
grep -F "archive must contain exactly" "$AXO_WORK_DIR/missing-notices.out" >/dev/null \
  || fail "missing-license rejection did not report the archive contract"

echo "Release artifact regression contract: PASS (native package + four targets)"
