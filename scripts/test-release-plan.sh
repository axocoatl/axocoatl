#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
planner="$script_dir/release-plan.sh"
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-release-plan-test.XXXXXX")
trap 'rm -rf -- "$work_dir"' EXIT HUP INT TERM

fail() {
  echo "test-release-plan: $*" >&2
  exit 1
}

if ! command -v cargo >/dev/null 2>&1; then
  cargo_bin=${CARGO_HOME:-${HOME:-}/.cargo}/bin
  [[ -x "$cargo_bin/cargo" ]] && export PATH="$cargo_bin:$PATH"
fi
command -v cargo >/dev/null 2>&1 || fail "cargo is required"
command -v jq >/dev/null 2>&1 || fail "jq is required"

metadata="$work_dir/metadata.json"
(cd "$repo_root" && cargo metadata --locked --offline --no-deps --format-version 1) > "$metadata"
empty_changes="$work_dir/empty-changes.txt"
notice_changes="$work_dir/notice-changes.txt"
bad_changes="$work_dir/bad-changes.txt"
: > "$empty_changes"
printf '%s\n' axocoatl-server/THIRD_PARTY_LICENSES.txt > "$notice_changes"
printf '%s\n' crates/axocoatl-core/src/lib.rs > "$bad_changes"

expect_pass() {
  name=$1
  expected=$2
  shift 2
  output="$work_dir/$name.out"
  "$planner" "$@" > "$output"
  [[ "$(cat "$output")" == "$expected" ]] \
    || fail "$name selected an unexpected release set: $(tr '\n' ' ' < "$output")"
}

expect_fail() {
  name=$1
  expected=$2
  shift 2
  output="$work_dir/$name.out"
  if "$planner" "$@" > "$output" 2>&1; then
    fail "$name unexpectedly passed"
  fi
  grep -F "$expected" "$output" >/dev/null \
    || fail "$name did not report '$expected': $(cat "$output")"
}

expect_pass cli-only axocoatl-cli v1.0.1 1.0.0 "$metadata" "$empty_changes"
expect_pass cli-notice-only axocoatl-cli v1.0.1 1.0.0 "$metadata" "$notice_changes"
expect_fail cli-source-change 'unpublished package changes' \
  v1.0.1 1.0.0 "$metadata" "$bad_changes"
expect_fail malformed-tag 'not a supported semantic product version' \
  release-1.0.1 1.0.0 "$metadata" "$empty_changes"

extra_release="$work_dir/extra-release.json"
jq '(.packages[] | select(.name == "axocoatl-core") | .version) = "1.0.1"' \
  "$metadata" > "$extra_release"
expect_fail cli-extra-crate 'may publish only axocoatl-cli' \
  v1.0.1 1.0.0 "$extra_release" "$empty_changes"

missing_cli="$work_dir/missing-cli.json"
jq '(.packages[] | select(.name == "axocoatl-cli") | .version) = "1.0.0"
    | (.packages[] | select(.name == "axocoatl-core") | .version) = "1.0.1"' \
  "$metadata" > "$missing_cli"
expect_fail cli-missing 'axocoatl-cli must be part' \
  v1.0.1 1.0.0 "$missing_cli" "$empty_changes"

coordinated="$work_dir/coordinated.json"
jq '(.packages[] | select(.source == null and .publish == null) | .version) = "1.0.0"' \
  "$metadata" > "$coordinated"
expected_all=$(printf '%s\n' \
  axocoatl-core axocoatl-token axocoatl-llm axocoatl-config axocoatl-memory \
  axocoatl-graph axocoatl-isolation axocoatl-a2a axocoatl-llm-openai \
  axocoatl-llm-anthropic axocoatl-llm-ollama axocoatl-llm-mistral \
  axocoatl-llm-gemini axocoatl-mcp axocoatl-tools axocoatl-coordination \
  axocoatl-actor axocoatl-session axocoatl-service axocoatl-daemon \
  axocoatl-server axocoatl-cli)
expect_pass coordinated "$expected_all" v1.0.0 1.0.0 "$coordinated" "$empty_changes"

coordinated_missing="$work_dir/coordinated-missing.json"
jq '(.packages[] | select(.name == "axocoatl-core") | .version) = "0.9.9"' \
  "$coordinated" > "$coordinated_missing"
expect_fail coordinated-missing 'must include every publishable package' \
  v1.0.0 1.0.0 "$coordinated_missing" "$empty_changes"

inventory_extra="$work_dir/inventory-extra.json"
jq '.packages += [{"name":"axocoatl-surprise","version":"1.0.0","source":null,"publish":null,"dependencies":[]}]' \
  "$metadata" > "$inventory_extra"
expect_fail inventory-extra 'not the exact publishable workspace set' \
  v1.0.1 1.0.0 "$inventory_extra" "$empty_changes"

bad_order="$work_dir/bad-order.json"
jq '(.packages[] | select(.name == "axocoatl-core") | .dependencies) += [{"name":"axocoatl-cli"}]' \
  "$metadata" > "$bad_order"
expect_fail dependency-order 'appears before local dependency axocoatl-cli' \
  v1.0.1 1.0.0 "$bad_order" "$empty_changes"

printf '%s\n' 'not-json' > "$work_dir/malformed.json"
expect_fail malformed-metadata 'not valid Cargo metadata' \
  v1.0.1 1.0.0 "$work_dir/malformed.json" "$empty_changes"

echo 'Release plan contract: PASS (11 positive and negative simulations)'
