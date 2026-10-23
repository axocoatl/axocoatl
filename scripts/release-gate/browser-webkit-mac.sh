#!/bin/bash
# The product browser journeys in WebKit on a Mac with Podman
# (docs/RELEASE_GATE.md, path C1-browser-webkit).
#
# Safari's own cookie behaviour exists only in WebKit on macOS, and the
# journeys that assert it with a real Preview need a Session on Podman:
# CI's hosted macOS runner has WebKit but no Podman, and CI's Linux legs
# have Podman but Linux WebKit. This runs scripts/verify-product-browser-gate.sh
# in WebKit with the candidate's own macOS binary (from its release archive,
# checked against its .sha256) on this Mac's running Podman machine, with
# the Preview's Session image required, so no Preview journey is skipped.
# Run it in any account of the gate Mac with Node 22 and a running Podman
# machine, after the candidate's archives are built. Evidence goes to
# <evidence-root>/<version>/<commit>/<label>.json and its folder.
#
# Usage:
#   browser-webkit-mac.sh --tarball DIST/axocoatl-vX.Y.Z-<mac target>.tar.gz [options]
# Options:
#   --label NAME         evidence path (default C1-browser-webkit)
#   --commit SHA         commit the tarball was built from (default: HEAD)
#   --evidence-root DIR  default: <repo>/release-evidence
# Scripts handed to `sh -c` and `bash -c` take their values as positional
# parameters.
# shellcheck disable=SC2016
set -uo pipefail

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=scripts/release-gate/lib.sh
. "$script_dir/lib.sh"

usage() {
  awk 'NR == 1 { next } /^#/ { if ($0 ~ /shellcheck|positional param|take their values|read their values|^# parameters\.$/) next; sub(/^# ?/, ""); print; next } { exit }' "$0"
  exit 2
}

# The Preview journeys' Session image, pinned as CI pins it (ci.yml).
node_image=docker.io/library/node@sha256:2cf067cfed83d5ea958367df9f966191a942351a2df77d6f0193e162b5febfc0
tarball='' label=C1-browser-webkit commit='' evidence_root="$gate_repo/release-evidence"
while [ $# -gt 0 ]; do
  case "$1" in
    --tarball) tarball=${2:?}; shift ;;
    --label) label=${2:?}; shift ;;
    --commit) commit=${2:?}; shift ;;
    --evidence-root) evidence_root=${2:?}; shift ;;
    -h | --help) usage ;;
    *) gate_die "unknown argument: $1 (see --help)" ;;
  esac
  shift
done

[ "$(uname -s)" = Darwin ] || gate_die "this runs the WebKit journeys on a Mac; CI runs them on Linux"
[ -n "$tarball" ] && [ -f "$tarball" ] && [ -f "$tarball.sha256" ] \
  || gate_die "--tarball must be the candidate's macOS release archive, next to its .sha256"
tarball=$(CDPATH='' cd -- "$(dirname -- "$tarball")" && pwd)/${tarball##*/}
case "${tarball##*/}" in
  *-apple-darwin.tar.gz) ;;
  *) gate_die "not a macOS release archive: ${tarball##*/}" ;;
esac
for tool in node npm git tar podman; do
  command -v "$tool" >/dev/null 2>&1 || gate_die "$tool is required"
done
[ -n "$commit" ] || commit=$(git -C "$gate_repo" rev-parse HEAD)
gate_valid_commit "$commit" || gate_die "--commit must be a full 40-character commit SHA"
gate_valid_name "$label" || gate_die "--label must be letters, digits and dashes"
version=${tarball##*/axocoatl-v}
version=${version%%-*}
gate_valid_version "$version" || gate_die "no X.Y.Z version in the tarball name ${tarball##*/}"

work=$(mktemp -d "${GATE_TMPDIR:-${TMPDIR:-/tmp}}/axo-gate-webkit.XXXXXX")
trap 'rm -rf -- "$work"' EXIT
trap 'exit 130' INT TERM

gate_begin "$evidence_root" "$version" "$commit" "$label" C
gate_host_json "$GATE_TMP/host.json"
exec > >(tee "$GATE_ATTACH/transcript.txt") 2>&1

# 1. The candidate's binary, as its archive and checksum carry it.
expected=$(awk 'NR == 1 { print $1 }' "$tarball.sha256")
actual=$(gate_sha256 "$tarball")
if [ -n "$expected" ] && [ "$expected" = "$actual" ]; then
  gate_record "the archive matches its .sha256" pass '' '' '' "$actual"
else
  gate_record "the archive matches its .sha256" fail '' '' '' "expected ${expected:-none}, got $actual"
fi
gate_step "unpack the candidate's binary" -- tar -xzf "$tarball" -C "$work"
binary="$work/axocoatl"
GATE_AXOCOATL_VERSION=$("$binary" --version 2>/dev/null)
export GATE_AXOCOATL_VERSION
gate_check "the binary is $version" "$("$binary" --version 2>&1 | head -n 1)" -- \
  sh -c '"$1" --version | grep -Fq -- "$2"' gate-version "$binary" "$version"

# 2. Podman answers, and the Preview's Session image is there (pulled by
# digest and named as the journeys look it up when it is not).
gate_step "Podman answers" -- podman info --format '{{.Host.Arch}} {{.Version.Version}}'
if ! podman image exists docker.io/library/node:20-slim; then
  gate_step "pull the Preview's Session image" -- /bin/bash -c \
    'podman pull "$1" && podman tag "$1" docker.io/library/node:20-slim' gate-pull "$node_image"
fi

# 3. Every journey in WebKit with this binary; a missing Session image fails
# the Preview journeys instead of skipping them.
gate_step "product browser journeys in WebKit on macOS with Podman" -- env \
  AXO_BROWSER_ENGINE=webkit AXO_BROWSER_PROFILE=release AXO_BROWSER_BINARY="$binary" \
  AXOCOATL_E2E_REQUIRE_NODE_IMAGE=1 "$gate_repo/scripts/verify-product-browser-gate.sh"
summary=$(grep -E '^Browser regressions \(webkit\):' "$GATE_ATTACH/logs/$(gate_slug "product browser journeys in WebKit on macOS with Podman").log" | tail -n 1)
gate_record "the run's summary" "$([ -n "$summary" ] && echo pass || echo fail)" '' '' '' "${summary:-no summary line}"

gate_tools_json "$GATE_TMP/tools.json"
gate_finish --json host="$GATE_TMP/host.json" --json tools="$GATE_TMP/tools.json"
