#!/usr/bin/env bash
# Exercise the actual suite runner without compiling or starting Podman.
# Cargo's colored binary headers must delimit the same summaries as plain ones.
set -euo pipefail

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-suites-parser.XXXXXX")
trap 'rm -rf -- "$work"' EXIT HUP INT TERM
fixture="$work/repo"
mkdir -p "$fixture/scripts" "$work/bin" "$work/logs"
cp "$script_dir/test-podman-suites.sh" "$fixture/scripts/"
for crate in axocoatl-memory axocoatl-session; do
  mkdir -p "$fixture/crates/$crate/src" "$fixture/crates/$crate/tests"
  touch "$fixture/crates/$crate/src/lib.rs" "$fixture/crates/$crate/tests/empty.rs"
  : >"$work/logs/$crate.list"
  : >"$work/logs/$crate.run"
  printf '0\n' >"$work/logs/$crate.status"
done
cat >"$work/bin/cargo" <<'CARGO'
#!/usr/bin/env bash
set -euo pipefail
crate='' mode=run
while (($#)); do
  case "$1" in
    -p) crate=$2; shift ;;
    --list) mode=list ;;
  esac
  shift
done
cat "$AXO_SUITE_FIXTURES/$crate.$mode"
[[ $mode == list ]] || exit "$(cat "$AXO_SUITE_FIXTURES/$crate.status")"
CARGO
chmod +x "$work/bin/cargo"

checks=0
expect_run() {
  local name=$1 expected_status=$2 expected_summary=$3 status=0
  PATH="$work/bin:$PATH" AXO_SUITE_FIXTURES="$work/logs" \
    bash "$fixture/scripts/test-podman-suites.sh" run other >"$work/output" 2>&1 || status=$?
  if [[ $status -ne $expected_status ]] || ! grep -Fxq "$expected_summary" "$work/output"; then
    echo "FAIL: $name (exit $status, expected $expected_status)" >&2
    cat "$work/output" >&2
    exit 1
  fi
  checks=$((checks + 1))
  echo "ok: $name"
}

printf 'first: test\nsecond: test\n' >"$work/logs/axocoatl-memory.list"
cat >"$work/logs/axocoatl-memory.run" <<'LOG'
     Running unittests src/lib.rs (target/debug/deps/memory)
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
     Running tests/empty.rs (target/debug/deps/empty)
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
LOG
expect_run 'plain Cargo headers preserve earlier binary results' 0 \
  'Podman suites (other): 2 of 2 ignored tests passed, 0 failed, 0 without a result'

# The actual CI form: CARGO_TERM_COLOR=always decorates Running, while
# libtest summaries in the pipe are plain. The later empty target must not
# replace the earlier memory suite's two passes.
printf '\033[1m\033[92m     Running\033[0m unittests src/lib.rs (target/debug/deps/memory)\n' >"$work/logs/axocoatl-memory.run"
printf 'test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 156 filtered out; finished in 46.17s\n' >>"$work/logs/axocoatl-memory.run"
printf '\033[1m\033[92m     Running\033[0m tests/empty.rs (target/debug/deps/empty)\n' >>"$work/logs/axocoatl-memory.run"
printf 'test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 36 filtered out; finished in 0.00s\n' >>"$work/logs/axocoatl-memory.run"
expect_run 'CI colored Cargo headers preserve earlier binary results' 0 \
  'Podman suites (other): 2 of 2 ignored tests passed, 0 failed, 0 without a result'

printf '\033[1m\033[92m     Running\033[0m unittests src/lib.rs (target/debug/deps/memory)\n' >"$work/logs/axocoatl-memory.run"
printf 'test result: \033[32mok\033[0m. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n' >>"$work/logs/axocoatl-memory.run"
expect_run 'colored libtest status is counted too' 0 \
  'Podman suites (other): 2 of 2 ignored tests passed, 0 failed, 0 without a result'

cat >"$work/logs/axocoatl-memory.run" <<'LOG'
     Running unittests src/lib.rs (target/debug/deps/memory)
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
LOG
expect_run 'child-process summary is not counted as another binary' 0 \
  'Podman suites (other): 2 of 2 ignored tests passed, 0 failed, 0 without a result'

cat >"$work/logs/axocoatl-memory.run" <<'LOG'
     Running unittests src/lib.rs (target/debug/deps/memory)
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
LOG
printf '101\n' >"$work/logs/axocoatl-memory.status"
expect_run 'failed parent remains failed after a passing child' 1 \
  'Podman suites (other): 1 of 2 ignored tests passed, 1 failed, 0 without a result'
printf '0\n' >"$work/logs/axocoatl-memory.status"

cat >"$work/logs/axocoatl-memory.run" <<'LOG'
     Running unittests src/lib.rs (target/debug/deps/memory)
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
     Running tests/empty.rs (target/debug/deps/empty)
error: test process exited without a result
LOG
printf '101\n' >"$work/logs/axocoatl-memory.status"
expect_run 'crashed binary remains missing from the inventoried total' 1 \
  'Podman suites (other): 1 of 2 ignored tests passed, 0 failed, 1 without a result'
printf '0\n' >"$work/logs/axocoatl-memory.status"

printf 'test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n' >"$work/logs/axocoatl-memory.run"
expect_run 'extra results cannot satisfy a smaller inventory' 1 \
  'Podman suites (other): 3 of 2 ignored tests passed, 0 failed, 0 without a result'

printf 'test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n' >"$work/logs/axocoatl-memory.run"
printf '101\n' >"$work/logs/axocoatl-memory.status"
expect_run 'Cargo failure cannot pass even when the result count matches' 1 \
  'Podman suites (other): 2 of 2 ignored tests passed, 0 failed, 0 without a result'
grep -Fxq 'test-podman-suites: 1 Cargo test pipelines failed' "$work/output"
printf '0\n' >"$work/logs/axocoatl-memory.status"

: >"$work/logs/axocoatl-memory.list"
printf 'test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n' >"$work/logs/axocoatl-memory.run"
expect_run 'an empty ignored-test inventory cannot pass' 1 \
  'Podman suites (other): 0 of 0 ignored tests passed, 0 failed, 0 without a result'

echo "Podman suite result parser: PASS ($checks cases)"
