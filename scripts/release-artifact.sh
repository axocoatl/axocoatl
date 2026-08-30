#!/bin/sh
# Build and verify Axocoatl release archives without publishing anything.
#
# Usage:
#   release-artifact.sh package <tag> <target> <binary> <artifact-dir>
#   release-artifact.sh verify <tag> <target> <artifact-dir>
#   release-artifact.sh verify-set <tag> <artifact-dir> <target>...
set -eu

PROGRAM="axocoatl"
LICENSE_ENTRY="LICENSE"
THIRD_PARTY_ENTRY="THIRD_PARTY_LICENSES.txt"
script_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
control_repo_root="$(CDPATH= cd -- "$script_dir/.." && pwd)"
release_source_root="${AXO_RELEASE_SOURCE_ROOT:-$control_repo_root}"
case "$release_source_root" in
  /*) ;;
  *) fail_early="AXO_RELEASE_SOURCE_ROOT must be absolute: $release_source_root" ;;
esac
[ -z "${fail_early:-}" ] || {
  echo "release-artifact: $fail_early" >&2
  exit 1
}
release_source_root="$(CDPATH= cd -- "$release_source_root" && pwd)" \
  || {
    echo "release-artifact: release source root does not exist: $release_source_root" >&2
    exit 1
  }
source_license="$release_source_root/LICENSE"
source_third_party="$release_source_root/axocoatl-server/THIRD_PARTY_LICENSES.txt"
archive_builder="$control_repo_root/scripts/create-release-archive.py"
work_dir=""

fail() {
  echo "release-artifact: $*" >&2
  exit 1
}

cleanup() {
  if [ -n "$work_dir" ] && [ -d "$work_dir" ]; then
    rm -rf "$work_dir"
  fi
}

trap cleanup 0 HUP INT TERM

make_work_dir() {
  [ -z "$work_dir" ] || return 0
  work_dir="$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-release.XXXXXX")" \
    || fail "could not create a temporary directory"
}

validate_tag() {
  tag=$1
  case "$tag" in
    v[0-9A-Za-z]* ) ;;
    * ) fail "tag must start with 'v' and contain a version: $tag" ;;
  esac
  case "$tag" in
    *[!0-9A-Za-z.+-]* ) fail "tag contains characters that are unsafe in an artifact name: $tag" ;;
  esac
}

validate_target() {
  target=$1
  [ -n "$target" ] || fail "target must not be empty"
  case "$target" in
    *[!0-9A-Za-z._-]* ) fail "target contains characters that are unsafe in an artifact name: $target" ;;
  esac
}

host_target() {
  host_os="$(uname -s)"
  host_arch="$(uname -m)"
  case "$host_os:$host_arch" in
    Linux:x86_64|Linux:amd64) printf '%s\n' x86_64-unknown-linux-gnu ;;
    Linux:aarch64|Linux:arm64) printf '%s\n' aarch64-unknown-linux-gnu ;;
    Darwin:x86_64|Darwin:amd64) printf '%s\n' x86_64-apple-darwin ;;
    Darwin:arm64|Darwin:aarch64) printf '%s\n' aarch64-apple-darwin ;;
    *) fail "unsupported release host: $host_os $host_arch" ;;
  esac
}

assert_native_target() {
  target=$1
  native_target="$(host_target)"
  [ "$target" = "$native_target" ] \
    || fail "target $target must be packaged and executed on its native $native_target runner"
}

sha256_file() {
  file=$1
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
  else
    fail "neither sha256sum nor shasum is available"
  fi
}

expected_version() {
  printf '%s\n' "${1#v}"
}

assert_version() {
  binary=$1
  tag=$2
  expected="$PROGRAM $(expected_version "$tag")"
  [ -x "$binary" ] || fail "packaged binary is not executable: $binary"
  actual="$("$binary" --version)" \
    || fail "packaged binary could not run '$PROGRAM --version': $binary"
  [ "$actual" = "$expected" ] \
    || fail "packaged version mismatch: expected '$expected', got '$actual'"
}

artifact_name() {
  printf '%s-%s-%s.tar.gz\n' "$PROGRAM" "$1" "$2"
}

assert_checksum_manifest() {
  archive=$1
  manifest=$2
  archive_basename=$3

  [ -f "$manifest" ] && [ ! -L "$manifest" ] \
    || fail "checksum manifest is missing or is not a regular file: $manifest"

  awk -v expected_name="$archive_basename" '
    NR != 1 { exit 1 }
    NF != 2 { exit 1 }
    length($1) != 64 { exit 1 }
    $1 !~ /^[0-9A-Fa-f]+$/ { exit 1 }
    $2 != expected_name { exit 1 }
    END { if (NR != 1) exit 1 }
  ' "$manifest" \
    || fail "checksum manifest must contain exactly '<sha256>  $archive_basename'"

  expected_digest="$(awk '{print $1}' "$manifest" | tr 'A-F' 'a-f')"
  actual_digest="$(sha256_file "$archive" | tr 'A-F' 'a-f')"
  [ "$actual_digest" = "$expected_digest" ] \
    || fail "checksum mismatch for $archive_basename"
}

assert_distribution_sources() {
  [ -f "$source_license" ] && [ ! -L "$source_license" ] && [ -s "$source_license" ] \
    || fail "Axocoatl license source is missing or invalid: $source_license"
  [ -f "$source_third_party" ] && [ ! -L "$source_third_party" ] && [ -s "$source_third_party" ] \
    || fail "third-party notice source is missing or invalid: $source_third_party"
}

assert_archive_contents() {
  archive=$1
  tag=$2
  run_binary=$3
  extract_dir=$4

  listing="$(tar -tzf "$archive")" \
    || fail "could not list archive: $archive"
  expected_listing="$(
    printf '%s\n' "$LICENSE_ENTRY" "$THIRD_PARTY_ENTRY" "$PROGRAM" | LC_ALL=C sort
  )"
  sorted_listing="$(printf '%s\n' "$listing" | LC_ALL=C sort)"
  [ "$sorted_listing" = "$expected_listing" ] \
    || fail "archive must contain exactly '$PROGRAM', '$LICENSE_ENTRY', and '$THIRD_PARTY_ENTRY': $archive"

  mkdir -p "$extract_dir"
  tar -xzf "$archive" -C "$extract_dir" \
    || fail "could not extract archive: $archive"

  extracted="$extract_dir/$PROGRAM"
  [ -f "$extracted" ] && [ ! -L "$extracted" ] \
    || fail "archive entry is not a regular '$PROGRAM' binary: $archive"
  [ -x "$extracted" ] \
    || fail "archive entry is not executable: $archive"

  extracted_license="$extract_dir/$LICENSE_ENTRY"
  extracted_third_party="$extract_dir/$THIRD_PARTY_ENTRY"
  [ -f "$extracted_license" ] && [ ! -L "$extracted_license" ] && [ -s "$extracted_license" ] \
    || fail "archive license is missing, empty, or not a regular file: $archive"
  [ -f "$extracted_third_party" ] && [ ! -L "$extracted_third_party" ] && [ -s "$extracted_third_party" ] \
    || fail "archive third-party notice is missing, empty, or not a regular file: $archive"
  cmp -s "$source_license" "$extracted_license" \
    || fail "archive license does not match the reviewed repository license: $archive"
  cmp -s "$source_third_party" "$extracted_third_party" \
    || fail "archive third-party notice does not match the generated release notice: $archive"

  if [ "$run_binary" = "yes" ]; then
    assert_version "$extracted" "$tag"
  fi
}

verify_one() {
  tag=$1
  target=$2
  artifact_dir=$3
  run_binary=$4
  extract_suffix=$5

  validate_tag "$tag"
  validate_target "$target"
  assert_distribution_sources
  if [ "$run_binary" = "yes" ]; then
    assert_native_target "$target"
  fi

  name="$(artifact_name "$tag" "$target")"
  archive="$artifact_dir/$name"
  manifest="$archive.sha256"

  [ -f "$archive" ] && [ ! -L "$archive" ] \
    || fail "release archive is missing or is not a regular file: $archive"
  assert_checksum_manifest "$archive" "$manifest" "$name"
  make_work_dir
  assert_archive_contents "$archive" "$tag" "$run_binary" "$work_dir/$extract_suffix"
  echo "release-artifact: verified $name"
}

package_artifact() {
  tag=$1
  target=$2
  binary=$3
  artifact_dir=$4

  validate_tag "$tag"
  validate_target "$target"
  assert_distribution_sources
  assert_native_target "$target"
  [ -f "$binary" ] && [ ! -L "$binary" ] \
    || fail "release binary is missing or is not a regular file: $binary"
  assert_version "$binary" "$tag"

  mkdir -p "$artifact_dir"
  name="$(artifact_name "$tag" "$target")"
  archive="$artifact_dir/$name"
  manifest="$archive.sha256"
  [ ! -e "$archive" ] && [ ! -e "$manifest" ] \
    || fail "refusing to overwrite an existing release artifact: $archive"

  make_work_dir
  staging="$work_dir/package"
  mkdir -p "$staging"
  cp "$binary" "$staging/$PROGRAM"
  cp "$source_license" "$staging/$LICENSE_ENTRY"
  cp "$source_third_party" "$staging/$THIRD_PARTY_ENTRY"
  chmod 0755 "$staging/$PROGRAM"
  chmod 0644 "$staging/$LICENSE_ENTRY" "$staging/$THIRD_PARTY_ENTRY"
  command -v python3 >/dev/null 2>&1 \
    || fail "python3 is required for deterministic release archives"
  [ -f "$archive_builder" ] && [ ! -L "$archive_builder" ] \
    || fail "deterministic archive builder is missing: $archive_builder"
  python3 "$archive_builder" "$archive" "$staging"

  digest="$(sha256_file "$archive" | tr 'A-F' 'a-f')"
  printf '%s  %s\n' "$digest" "$name" > "$manifest"

  verify_one "$tag" "$target" "$artifact_dir" yes "packaged-$target"
}

verify_set() {
  tag=$1
  artifact_dir=$2
  shift 2

  validate_tag "$tag"
  [ "$#" -gt 0 ] || fail "verify-set requires at least one target"
  [ -d "$artifact_dir" ] || fail "artifact directory does not exist: $artifact_dir"

  expected_count=$((2 * $#))
  actual_count="$(find "$artifact_dir" -mindepth 1 -maxdepth 1 -print | wc -l | tr -d '[:space:]')"
  [ "$actual_count" = "$expected_count" ] \
    || fail "expected $expected_count release files, found $actual_count in $artifact_dir"

  index=0
  for target in "$@"; do
    index=$((index + 1))
    verify_one "$tag" "$target" "$artifact_dir" no "set-$index"
  done
  echo "release-artifact: verified complete artifact set for $tag"
}

usage() {
  cat >&2 <<'EOF'
Usage:
  release-artifact.sh package <tag> <target> <binary> <artifact-dir>
  release-artifact.sh verify <tag> <target> <artifact-dir>
  release-artifact.sh verify-set <tag> <artifact-dir> <target>...
EOF
  exit 2
}

command=${1:-}
case "$command" in
  package)
    [ "$#" -eq 5 ] || usage
    package_artifact "$2" "$3" "$4" "$5"
    ;;
  verify)
    [ "$#" -eq 4 ] || usage
    verify_one "$2" "$3" "$4" yes "verified-$3"
    ;;
  verify-set)
    [ "$#" -ge 4 ] || usage
    shift
    verify_set "$@"
    ;;
  *) usage ;;
esac
