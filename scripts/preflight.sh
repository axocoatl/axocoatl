#!/usr/bin/env bash
# One local command for every deterministic PR/release gate that can run on this host.
set -euo pipefail

[[ $# -le 1 ]] || {
  echo 'Usage: preflight.sh [comparison-commit]' >&2
  exit 2
}

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
cd "$repo_root"

if ! command -v cargo >/dev/null 2>&1; then
  cargo_bin=${CARGO_HOME:-${HOME:-}/.cargo}/bin
  [[ -x "$cargo_bin/cargo" ]] && export PATH="$cargo_bin:$PATH"
fi

fail() {
  echo "preflight: $*" >&2
  exit 1
}

for command in \
  cargo rustc go node npm npx python3 ruby jq curl git ffmpeg ffprobe; do
  command -v "$command" >/dev/null 2>&1 \
    || fail "$command is required; see CONTRIBUTING.md#the-quality-gate"
done

node_version=$(node --version)
[[ "$node_version" =~ ^v22\. ]] \
  || fail "Node 22 is required for CI parity (found $node_version)"
python_version=$(python3 --version 2>&1)
[[ "$python_version" == "Python 3.13.7" ]] \
  || fail "Python 3.13.7 is required for deterministic release archives (found $python_version)"
go_version=$(go version | awk '{print $3}')
[[ "$go_version" == go1.26.2 ]] \
  || fail "Go 1.26.2 is required for pinned actionlint execution (found $go_version)"
rust_version=$(rustc --version | awk '{print $2}')
[[ "$rust_version" == 1.95.0 ]] \
  || fail "Rust 1.95.0 must be the active toolchain (found $rust_version)"
msrv_cargo_version=$(cargo +1.88.0 --version | awk '{print $2}')
[[ "$msrv_cargo_version" == 1.88.0 ]] \
  || fail "Rust 1.88.0 must be installed for the MSRV gate (found $msrv_cargo_version)"
audit_version=$(cargo audit --version 2>&1)
[[ "$audit_version" == *" 0.22.2" ]] \
  || fail "cargo-audit 0.22.2 is required (found $audit_version)"
about_version=$(cargo about --version 2>&1)
[[ "$about_version" == *" 0.9.1" ]] \
  || fail "cargo-about 0.9.1 is required (found $about_version)"

base=${1:-}
if [[ -z "$base" ]]; then
  if git rev-parse --verify origin/main^{commit} >/dev/null 2>&1; then
    base=origin/main
  else
    base=HEAD^
  fi
fi

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-preflight.XXXXXX")
trap 'rm -rf -- "$work_dir"' EXIT HUP INT TERM

./scripts/run-actionlint.sh
./scripts/check-workflow-contracts.rb
./scripts/test-film-gate.sh
./scripts/test-public-release.sh
node --test demo/one-app/films/*.test.mjs
./scripts/test-recover-v1.0.1-workflow.sh
./scripts/verify-product-browser-gate.sh
if [[ "$(uname -s)" == Linux ]]; then
  ./scripts/verify-cross-linux-gate.sh build
else
  ./scripts/verify-cross-linux-gate.sh prove-unchanged "$base"
fi
./scripts/verify-marketing-gate.sh portable "$work_dir/marketing"
./scripts/verify-film-gate.sh candidate-worktree "$base"
./scripts/verify-docs-gate.sh
./scripts/verify-native-gate.sh

echo 'Local PR and release preflight: PASS'
