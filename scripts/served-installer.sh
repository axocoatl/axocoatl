#!/usr/bin/env bash
# Write the install.sh the marketing site serves (axocoatl.ai/install.sh) to
# OUTPUT: scripts/install.sh at the tag of axocoatl-cli's version (vX.Y.Z),
# fetched from the remote, so an installer change merged to main is never
# served before the release that ships it.
set -euo pipefail

fail() {
  echo "served-installer: $*" >&2
  exit 1
}

[[ $# -eq 1 && -n "$1" ]] || {
  echo "Usage: served-installer.sh <output-file>" >&2
  exit 2
}
output=$1

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
remote_name=origin
# Local fixtures (scripts/test-public-release.sh), never in GitHub Actions.
if [[ "${GITHUB_ACTIONS:-}" != true ]]; then
  if [[ -n "${AXO_PUBLIC_RELEASE_REPO_ROOT:-}" ]]; then
    [[ -d "$AXO_PUBLIC_RELEASE_REPO_ROOT/.git" ]] \
      || fail 'local repository fixture is missing .git'
    repo_root=$(CDPATH= cd -- "$AXO_PUBLIC_RELEASE_REPO_ROOT" && pwd)
  fi
  remote_name=${AXO_PUBLIC_RELEASE_REMOTE:-$remote_name}
fi
cd "$repo_root"

version=$(awk '
  $0 == "[package]" { package_section = 1; next }
  package_section && /^\[/ { exit }
  package_section && $1 == "version" {
    gsub(/"/, "", $3)
    print $3
    exit
  }
' axocoatl-cli/Cargo.toml)
[[ -n "$version" ]] || fail "could not read axocoatl-cli product version"
tag="v$version"

ref=refs/axocoatl-deploy/installer-tag
git fetch --quiet --no-tags --force "$remote_name" "+refs/tags/$tag:$ref" \
  || fail "could not fetch $tag from $remote_name; the site serves only a released installer"
commit=$(git rev-parse "$ref^{commit}")
git show "$commit:scripts/install.sh" > "$output" \
  || fail "$tag ($commit) has no scripts/install.sh"
sh -n "$output" || fail "scripts/install.sh at $tag does not parse"
echo "served-installer: install.sh is scripts/install.sh at $tag ($commit)"
