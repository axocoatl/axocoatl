#!/usr/bin/env bash
# Build the Linux aarch64 CLI in CI, or fail closed locally if its inputs changed.
set -euo pipefail

fail() {
  echo "cross-linux-gate: $*" >&2
  exit 1
}

usage() {
  echo 'Usage: verify-cross-linux-gate.sh <build|prove-unchanged BASE_COMMIT>' >&2
  exit 2
}

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if [[ "${GITHUB_ACTIONS:-}" == true && -n "${AXO_CROSS_REPO_ROOT:-}" ]]; then
  fail 'AXO_CROSS_REPO_ROOT overrides are forbidden in GitHub Actions'
fi
repo_root=${AXO_CROSS_REPO_ROOT:-$(CDPATH= cd -- "$script_dir/.." && pwd)}
repo_root=$(CDPATH= cd -- "$repo_root" && pwd)
cd "$repo_root"

mode=${1:-}
case "$mode" in
  build)
    [[ $# -eq 1 ]] || usage
    [[ "$(uname -s)" == Linux ]] || fail 'the aarch64 Linux link gate must run on Linux'
    command -v aarch64-linux-gnu-gcc >/dev/null 2>&1 \
      || fail 'aarch64-linux-gnu-gcc is required'
    cargo build --locked --release -p axocoatl-cli \
      --target aarch64-unknown-linux-gnu \
      --config target.aarch64-unknown-linux-gnu.linker=\"aarch64-linux-gnu-gcc\"
    ;;
  prove-unchanged)
    [[ $# -eq 2 ]] || usage
    base=$2
    git cat-file -e "$base^{commit}" 2>/dev/null \
      || fail "base is not a commit: $base"
    changed=$(git diff --name-only "$base" -- \
      Cargo.toml Cargo.lock .cargo \
      axocoatl-cli/Cargo.toml axocoatl-cli/build.rs axocoatl-cli/src \
      axocoatl-server/Cargo.toml axocoatl-server/build.rs \
      axocoatl-server/src axocoatl-server/static \
      crates packages licenses/vendor-web || true)
    untracked=$(git ls-files --others --exclude-standard -- \
      Cargo.toml Cargo.lock .cargo \
      axocoatl-cli/Cargo.toml axocoatl-cli/build.rs axocoatl-cli/src \
      axocoatl-server/Cargo.toml axocoatl-server/build.rs \
      axocoatl-server/src axocoatl-server/static \
      crates packages licenses/vendor-web || true)
    [[ -z "$changed" && -z "$untracked" ]] || {
      printf '%s\n%s\n' "$changed" "$untracked" | sed '/^$/d' >&2
      fail 'Linux aarch64 build inputs changed; run the build gate on Linux before opening a PR'
    }
    echo "Cross Linux gate: PASS (build inputs unchanged from $base; CI retains the pinned aarch64 link proof)"
    ;;
  *) usage ;;
esac
