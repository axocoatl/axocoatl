#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
gate="$script_dir/verify-film-gate.sh"
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-film-gate-test.XXXXXX")
trap 'rm -rf -- "$work_dir"' EXIT HUP INT TERM

fail() {
  echo "test-film-gate: $*" >&2
  exit 1
}

fake_verifier="$work_dir/verifier.mjs"
printf '%s\n' \
  "import { appendFileSync } from 'node:fs';" \
  "appendFileSync(process.env.AXO_FILM_TEST_LOG, process.argv.slice(2).join(' ') + '\\n');" \
  > "$fake_verifier"
log="$work_dir/invocations.log"
fixture_repo="$work_dir/repo"
mkdir -p \
  "$fixture_repo/axocoatl-cli" \
  "$fixture_repo/demo/one-app/films/provenance" \
  "$fixture_repo/demo/one-app/films/source"
cp "$fake_verifier" "$fixture_repo/demo/one-app/films/verify-film-set.mjs"
git -C "$fixture_repo" init -q
git -C "$fixture_repo" config user.name 'Axocoatl Test'
git -C "$fixture_repo" config user.email test@axocoatl.invalid
printf '%s\n' '[package]' 'name = "axocoatl-cli"' 'version = "1.0.0"' \
  > "$fixture_repo/axocoatl-cli/Cargo.toml"
printf '%s\n' '{"identity":"first-committed"}' \
  > "$fixture_repo/demo/one-app/films/provenance/example.json"
git -C "$fixture_repo" add .
git -C "$fixture_repo" commit -qm baseline
base=$(git -C "$fixture_repo" rev-parse HEAD)
printf '%s\n' '[package]' 'name = "axocoatl-cli"' 'version = "1.0.1"' \
  > "$fixture_repo/axocoatl-cli/Cargo.toml"
git -C "$fixture_repo" add axocoatl-cli/Cargo.toml
git -C "$fixture_repo" commit -qm version-bump
version_head=$(git -C "$fixture_repo" rev-parse HEAD)

run_gate() {
  AXO_FILM_REPO_ROOT="$fixture_repo" \
    AXO_FILM_VERIFIER="$fake_verifier" \
    AXO_FILM_TEST_LOG="$log" \
    "$gate" "$@" >/dev/null
}

: > "$log"
run_gate portable
[[ "$(cat "$log")" == '--portable' ]] || fail "portable mode did not route exactly"

: > "$log"
run_gate candidate "$base" "$version_head"
expected=$'--portable\n--source-bound'
[[ "$(cat "$log")" == "$expected" ]] \
  || fail "a product version bump did not require portable plus source-bound verification"

: > "$log"
run_gate candidate "$version_head" "$version_head"
[[ "$(cat "$log")" == '--portable' ]] \
  || fail "an unchanged product version invoked the wrong PR gate"

printf '%s\n' '{"identity":"rewritten"}' \
  > "$fixture_repo/demo/one-app/films/provenance/example.json"
git -C "$fixture_repo" add demo/one-app/films/provenance/example.json
git -C "$fixture_repo" commit -qm provenance-rewrite
rewrite_head=$(git -C "$fixture_repo" rev-parse HEAD)
if run_gate candidate "$version_head" "$rewrite_head" >"$work_dir/rewrite.out" 2>&1; then
  fail "rewriting first-seen provenance unexpectedly passed"
fi

printf '%s\n' '{"identity":"first-committed"}' \
  > "$fixture_repo/demo/one-app/films/provenance/example.json"
git -C "$fixture_repo" add demo/one-app/films/provenance/example.json
git -C "$fixture_repo" commit -qm provenance-restoration
restored_head=$(git -C "$fixture_repo" rev-parse HEAD)
: > "$log"
run_gate candidate "$rewrite_head" "$restored_head"
[[ "$(cat "$log")" == '--portable' ]] \
  || fail "an exact first-committed provenance restoration did not take the audited portable path"

printf '%s\n' 'new versioned capture' \
  > "$fixture_repo/demo/one-app/films/source/example-v2.json"
git -C "$fixture_repo" add demo/one-app/films/source/example-v2.json
git -C "$fixture_repo" commit -qm versioned-capture
capture_head=$(git -C "$fixture_repo" rev-parse HEAD)
: > "$log"
run_gate candidate "$restored_head" "$capture_head"
[[ "$(cat "$log")" == $'--portable\n--source-bound' ]] \
  || fail "a new versioned capture did not require source-bound verification"

printf '%s\n' '[package]' 'name = "axocoatl-cli"' 'version = "1.0.2"' \
  > "$fixture_repo/axocoatl-cli/Cargo.toml"
: > "$log"
run_gate candidate-worktree "$capture_head"
[[ "$(cat "$log")" == $'--portable\n--source-bound' ]] \
  || fail "an uncommitted version bump did not require source-bound verification"

printf '%s\n' '{"identity":"tampered-again"}' \
  > "$fixture_repo/demo/one-app/films/provenance/example.json"
if run_gate candidate-worktree "$capture_head" >"$work_dir/worktree-rewrite.out" 2>&1; then
  fail "an uncommitted first-seen provenance rewrite unexpectedly passed"
fi

echo 'Film gate routing contract: PASS (version bumps and new captures bind source; first-seen artifacts are immutable)'
