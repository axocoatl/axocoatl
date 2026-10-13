#!/usr/bin/env bash
# Install the pinned Axocoatl release for this runner (Linux x86_64 or ARM64)
# and check its archive against the release's published SHA-256 file, and
# against the `sha256` input when one is given. Or, with the `binary` input,
# use an executable already on the runner. Either way, the executable's path
# is this step's `binary` output.
#
# Environment: AXOCOATL_VERSION (a release tag such as v1.4.0),
# AXOCOATL_SHA256 (optional), AXOCOATL_BINARY (optional), RUNNER_OS,
# RUNNER_ARCH, RUNNER_TEMP and GITHUB_OUTPUT as GitHub Actions sets them.
set -euo pipefail

REPO=axocoatl/axocoatl

fail() {
  printf '::error title=Axocoatl install::%s\n' "$1"
  exit 1
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

[[ -n ${GITHUB_OUTPUT:-} ]] || fail "GITHUB_OUTPUT is not set; this script runs as a step of the verify-receipt action"

if [[ -n ${AXOCOATL_BINARY:-} ]]; then
  [[ -f $AXOCOATL_BINARY && -x $AXOCOATL_BINARY ]] \
    || fail "binary: '$AXOCOATL_BINARY' is not an executable file"
  version_line=$("$AXOCOATL_BINARY" --version) \
    || fail "binary: '$AXOCOATL_BINARY --version' failed"
  printf 'Using %s (%s), not a downloaded release.\n' "$AXOCOATL_BINARY" "$version_line"
  printf 'binary=%s\n' "$AXOCOATL_BINARY" >>"$GITHUB_OUTPUT"
  exit 0
fi

tag=${AXOCOATL_VERSION:-}
[[ $tag =~ ^v[0-9][0-9A-Za-z.+-]*$ ]] \
  || fail "version: '$tag' is not a release tag such as v1.4.0"

case "${RUNNER_OS:-}:${RUNNER_ARCH:-}" in
  Linux:X64) target=x86_64-unknown-linux-gnu ;;
  Linux:ARM64) target=aarch64-unknown-linux-gnu ;;
  *) fail "this action runs on Linux x86_64 or ARM64 runners; this one is ${RUNNER_OS:-unknown} ${RUNNER_ARCH:-unknown}" ;;
esac

pinned=$(printf '%s' "${AXOCOATL_SHA256:-}" | tr 'A-F' 'a-f')
[[ -z $pinned || $pinned =~ ^[0-9a-f]{64}$ ]] \
  || fail "sha256: '$AXOCOATL_SHA256' is not a SHA-256 in hex"

archive="axocoatl-${tag}-${target}.tar.gz"
url="https://github.com/${REPO}/releases/download/${tag}/${archive}"
work=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/axocoatl-verify-receipt.XXXXXX")

download() {
  curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
    --retry 3 --connect-timeout 10 --max-time 300 --output "$2" "$1"
}

download "$url" "$work/$archive" \
  || fail "could not download $url (does release $tag exist?)"
download "$url.sha256" "$work/$archive.sha256" \
  || fail "could not download $url.sha256"

awk -v expected_name="$archive" '
  NR != 1 { exit 1 }
  NF != 2 { exit 1 }
  length($1) != 64 { exit 1 }
  $1 !~ /^[0-9A-Fa-f]+$/ { exit 1 }
  $2 != expected_name { exit 1 }
  END { if (NR != 1) exit 1 }
' "$work/$archive.sha256" \
  || fail "the release's checksum file must be exactly one line '<sha256>  $archive'"

published=$(awk '{print tolower($1)}' "$work/$archive.sha256")
actual=$(sha256_of "$work/$archive" | tr 'A-F' 'a-f')
[[ $actual == "$published" ]] \
  || fail "$archive has SHA-256 $actual, but the release's checksum file says $published"
[[ -z $pinned || $actual == "$pinned" ]] \
  || fail "$archive has SHA-256 $actual, but the sha256 input pins $pinned"

listing=$(tar -tzf "$work/$archive") || fail "could not list $archive"
[[ $(grep -cx 'axocoatl' <<<"$listing") == 1 ]] \
  || fail "$archive does not hold exactly one 'axocoatl' executable"
mkdir "$work/bin"
tar -xzf "$work/$archive" -C "$work/bin" axocoatl || fail "could not extract axocoatl from $archive"
binary="$work/bin/axocoatl"
[[ -f $binary && ! -L $binary && -x $binary ]] \
  || fail "$archive's 'axocoatl' is not an executable file"

expected_version="axocoatl ${tag#v}"
version_line=$("$binary" --version) || fail "'axocoatl --version' failed"
[[ $version_line == "$expected_version" ]] \
  || fail "the installed binary says '$version_line', not '$expected_version'"

printf 'Installed %s from %s (SHA-256 %s, as the release publishes%s).\n' \
  "$version_line" "$archive" "$actual" "${pinned:+ and as the sha256 input pins}"
printf 'binary=%s\n' "$binary" >>"$GITHUB_OUTPUT"
