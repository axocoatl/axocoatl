#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
gate="$script_dir/verify-cross-linux-gate.sh"
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-cross-gate-test.XXXXXX")
trap 'rm -rf -- "$work_dir"' EXIT HUP INT TERM
fixture="$work_dir/repo"

fail() {
  echo "test-cross-linux-gate: $*" >&2
  exit 1
}

mkdir -p "$fixture/axocoatl-cli/src" "$fixture/crates/example/src"
git -C "$fixture" init -q
git -C "$fixture" config user.name 'Axocoatl Test'
git -C "$fixture" config user.email test@axocoatl.invalid
printf '%s\n' '[workspace]' > "$fixture/Cargo.toml"
printf '%s\n' '# lock' > "$fixture/Cargo.lock"
printf '%s\n' 'fn main() {}' > "$fixture/axocoatl-cli/src/main.rs"
printf '%s\n' 'pub fn example() {}' > "$fixture/crates/example/src/lib.rs"
printf '%s\n' 'outside the cross-build inputs' > "$fixture/README.md"
git -C "$fixture" add .
git -C "$fixture" commit -qm baseline
base=$(git -C "$fixture" rev-parse HEAD)

AXO_CROSS_REPO_ROOT="$fixture" "$gate" prove-unchanged "$base" >/dev/null
printf '%s\n' 'documentation changed' >> "$fixture/README.md"
AXO_CROSS_REPO_ROOT="$fixture" "$gate" prove-unchanged "$base" >/dev/null

printf '%s\n' '# changed lock' >> "$fixture/Cargo.lock"
if AXO_CROSS_REPO_ROOT="$fixture" "$gate" prove-unchanged "$base" \
  >"$work_dir/changed.out" 2>&1; then
  fail 'a changed tracked build input unexpectedly passed'
fi
grep -F 'Linux aarch64 build inputs changed' "$work_dir/changed.out" >/dev/null \
  || fail 'tracked-input failure was not explicit'

git -C "$fixture" restore Cargo.lock
printf '%s\n' 'pub fn new_input() {}' > "$fixture/crates/example/src/new.rs"
if AXO_CROSS_REPO_ROOT="$fixture" "$gate" prove-unchanged "$base" \
  >"$work_dir/untracked.out" 2>&1; then
  fail 'an untracked build input unexpectedly passed'
fi

echo 'Cross Linux routing contract: PASS (unchanged inputs inherit proof; changed inputs fail closed)'
