#!/usr/bin/env bash
# The test suites that are #[ignore]d by default because they need Podman,
# prepared images, the internet or a live Ollama, as CI runs them
# (.github/workflows/ci.yml, "Podman and live suites"; docs/RELEASE_GATE.md).
#
# Usage:
#   test-podman-suites.sh prepare GROUP   build and pull what GROUP's suites need
#   test-podman-suites.sh run GROUP       run every ignored test of GROUP's crates,
#                                         and fail unless every one of them passed
#   test-podman-suites.sh list GROUP      no Podman: print the ignored tests run
#                                         GROUP would run, as "crate test" lines
#   test-podman-suites.sh check           no build: every crate with an ignored test
#                                         is in a group, and every test left out
#                                         exists and says why
#   test-podman-suites.sh serve-ollama VERSION
#                                         CI only: install that Ollama release and
#                                         serve it as this user, cloud models off
#
# Groups:
#   isolation  axocoatl-isolation (but supervisor_runtime, which
#              scripts/test-exec-supervisor-runtime.sh runs per image) and the
#              sandbox-session example
#   daemon     axocoatl-daemon
#   cli        axocoatl-cli
#   other      axocoatl-memory and axocoatl-session (no Podman)
#   ollama     axocoatl-llm-ollama's and axocoatl-daemon's live Ollama tests,
#              against an Ollama at OLLAMA_URL (default http://127.0.0.1:11434)
#              serving PROBE_MODEL, with cloud models off (the native Session
#              test once plain, once with JSON, once with a tool round-trip)
#
# prepare and run need Podman (rootless, the default connection or
# CONTAINER_CONNECTION) and the internet; prepare for daemon and cli builds
# the recipe and browser images with this checkout's CLI. Everything a suite
# makes is labelled and removed by the suite itself.
set -euo pipefail

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH='' cd -- "$script_dir/.." && pwd)
cd "$repo_root"

fail() {
  echo "test-podman-suites: $*" >&2
  exit 1
}

# Images, each pinned to its multi-architecture index by digest and tagged
# with the name the tests look up with --pull=never.
pinned_images=(
  "docker.io/library/alpine@sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc docker.io/library/alpine:3.20"
  "docker.io/library/node@sha256:2cf067cfed83d5ea958367df9f966191a942351a2df77d6f0193e162b5febfc0 docker.io/library/node:20-slim"
  "docker.io/library/node@sha256:fe4f6d74e20370acb04ad889077d14d0f64a9c8ea6a42bc60cddf1e844699914 docker.io/library/node:22-alpine"
  "docker.io/library/node@sha256:de170b39439eb0e5a3444ad7d60b015614a381e10fecae2e30ec9e735122f38d docker.io/library/node:22-bookworm-slim"
  "docker.io/library/rust@sha256:48b7f435b8fc6378ce43b44f15e3a47fd8919b2825f835c39bd4bd3da50af5b8 docker.io/library/rust:bookworm"
  "docker.io/library/ubuntu@sha256:68ee22004a9109b44973313b02ff7b1de250f1de0f5ecb628d73f579aaff33ac docker.io/library/ubuntu:24.04"
)
# The tools image the supervisor, egress and workload suites start Sessions
# from: Alpine with the repository commands, as Axocoatl's passive recovery
# image is made (session_sandbox.rs, passive_recovery_provision_args), then
# tests/fixtures/supervisor-*.Containerfile.
recovery_image=localhost/axocoatl-recovery-tools:alpine-3.20-v1
supervisor_root=localhost/axocoatl-supervisor-test-root:20260914
supervisor_nonroot=localhost/axocoatl-supervisor-test-nonroot:20260914

# The tests no group runs, by crate and name, each with why. `check` fails
# when one no longer exists.
exempt=(
  "axocoatl-isolation e2b::tests::live_exec_and_stdin_against_e2b|E2B's hosted service with an E2B_API_KEY; the project has no E2B account"
  "axocoatl-isolation e2b::tests::live_git_native_flow_against_e2b|E2B's hosted service with an E2B_API_KEY; the project has no E2B account"
  "axocoatl-isolation e2b::tests::live_pty_terminal_against_e2b|E2B's hosted service with an E2B_API_KEY; the project has no E2B account"
  "axocoatl-isolation e2b::tests::live_sandbox_trait_against_e2b|E2B's hosted service with an E2B_API_KEY; the project has no E2B account"
  "axocoatl-daemon session_dispatch::tests::run_tests::input_tests::openrouter_live_tests::openrouter_live_claude_sonnet_reasoning_tool_turn_settles_the_grant|OpenRouter credit and a key; the release gate's C1-openrouter and D1 paths run OpenRouter models live"
  "axocoatl-daemon session_dispatch::tests::run_tests::input_tests::openrouter_live_tests::openrouter_live_gpt_sol_reasoning_tool_turn_settles_the_grant|OpenRouter credit and a key; the release gate's C1-openrouter and D1 paths run OpenRouter models live"
  "axocoatl-daemon lock_inheritance_tests::background_pty_loop|a stress harness the lock tests start with AXOCOATL_TEST_PTY_LOOP_MS"
  "axocoatl-cli connect_cmd::tests::start_podman_commands_while_ctrl_c_is_typed|a helper podman_commands_started_while_ctrl_c_is_typed runs"
  "axocoatl-session ownership_crash_child|a helper the crash-cut test runs"
  "axocoatl-session provision_disposable_ownership_fixture|a helper of a private released-binary probe"
)

# The live Ollama tests, which the ollama group runs and the others leave out.
ollama_tests=(
  "axocoatl-llm-ollama native::observed_context::watch::the_contract_probes_hold_on_a_live_ollama"
  "axocoatl-daemon session_dispatch::tests::ollama_tests::native_ollama_real_model_refuses_over_long_input_and_stops_at_its_limit"
  "axocoatl-daemon session_dispatch::tests::ollama_tests::native_ollama_real_model_through_durable_controller"
)

# The crates and cargo targets of each group.
group_crates() {
  case "$1" in
    isolation) echo axocoatl-isolation sandbox-session ;;
    daemon) echo axocoatl-daemon ;;
    cli) echo axocoatl-cli ;;
    other) echo axocoatl-memory axocoatl-session ;;
    ollama) echo axocoatl-llm-ollama axocoatl-daemon ;;
    *) fail "unknown group $1 (isolation, daemon, cli, other, ollama)" ;;
  esac
}

# The cargo test arguments that select a crate's tests: its unit tests,
# every integration test but supervisor_runtime (run per image by
# test-exec-supervisor-runtime.sh), and an example package's binary.
targets() {
  local crate=$1 dir file
  case "$crate" in
    sandbox-session) echo "-p sandbox-session"; return ;;
    axocoatl-cli) dir=axocoatl-cli ;;
    *) dir=crates/$crate ;;
  esac
  # The unit tests: the library's, or a binary crate's (axocoatl-cli has no
  # library, and `--lib` refuses a package without one).
  if [[ -f $dir/src/lib.rs ]]; then
    printf -- '-p %s --lib' "$crate"
  else
    printf -- '-p %s --bins' "$crate"
  fi
  for file in "$dir"/tests/*.rs; do
    [[ -f $file ]] || continue
    file=${file##*/}
    [[ $file == supervisor_runtime.rs ]] && continue
    printf -- ' --test %s' "${file%.rs}"
  done
  echo
}

# The names a crate's group leaves out: the exempt tests, and, outside the
# ollama group, the live Ollama tests.
skipped() {
  local group=$1 crate=$2 entry
  for entry in "${exempt[@]}"; do
    entry=${entry%%|*}
    [[ ${entry%% *} == "$crate" ]] && echo "${entry#* }"
  done
  if [[ $group != ollama ]]; then
    for entry in "${ollama_tests[@]}"; do
      [[ ${entry%% *} == "$crate" ]] && echo "${entry#* }"
    done
  fi
  return 0
}

pull_pinned() {
  local entry reference name
  for entry in "${pinned_images[@]}"; do
    reference=${entry%% *}
    name=${entry#* }
    podman pull -q "$reference" >/dev/null
    podman tag "$reference" "$name"
    echo "pulled $name ($reference)"
  done
}

build_supervisor_images() {
  local fixtures=crates/axocoatl-isolation/tests/fixtures container
  if [[ "$(podman image inspect --format '{{ index .Labels "io.axocoatl.recovery-tools" }}' "$recovery_image" 2>/dev/null)" != alpine-3.20-v1 ]]; then
    container=axo-suites-recovery-$$
    podman run --name "$container" --security-opt=no-new-privileges --entrypoint /bin/sh \
      docker.io/library/alpine:3.20 -c 'apk add --no-cache coreutils findutils git grep'
    podman commit --quiet --change 'LABEL io.axocoatl.recovery-tools=alpine-3.20-v1' "$container" "$recovery_image" >/dev/null
    podman rm "$container" >/dev/null
  fi
  podman build -q -f "$fixtures/supervisor-root.Containerfile" -t "$supervisor_root" "$fixtures" >/dev/null
  podman build -q -f "$fixtures/supervisor-nonroot.Containerfile" -t "$supervisor_nonroot" "$fixtures" >/dev/null
  echo "built $supervisor_root and $supervisor_nonroot"
}

# This checkout's CLI, built once, for the images only it builds.
cli() {
  cargo build --locked -q -p axocoatl-cli
  "${CARGO_TARGET_DIR:-$repo_root/target}/debug/axocoatl" "$@"
}

prepare() {
  local group=$1
  group_crates "$group" >/dev/null
  case "$group" in
    isolation)
      pull_pinned
      build_supervisor_images
      cli browser install
      ;;
    daemon)
      pull_pinned
      build_supervisor_images
      cli browser install
      cli recipe build claude-code
      cli recipe build codex
      ;;
    cli)
      pull_pinned
      ;;
    other | ollama) ;;
  esac
}

# Install Ollama VERSION and serve it as this user with cloud models off (the
# settings Axocoatl's onboarding writes): the installer's own service, when
# it starts one, runs as another user with that user's settings.
serve_ollama() {
  local version=$1 served
  [[ "${GITHUB_ACTIONS:-}" == true ]] || fail "serve-ollama installs Ollama system-wide; it runs only on a CI runner"
  curl -fsSL https://ollama.com/install.sh | OLLAMA_VERSION="$version" sh
  if systemctl is-active --quiet ollama 2>/dev/null; then sudo systemctl stop ollama; fi
  mkdir -p "$HOME/.ollama"
  printf '{"disable_ollama_cloud": true}\n' >"$HOME/.ollama/server.json"
  nohup ollama serve >"${RUNNER_TEMP:-/tmp}/ollama-serve.log" 2>&1 &
  for _ in $(seq 1 30); do
    curl -fsS --max-time 5 http://127.0.0.1:11434/api/version >/dev/null 2>&1 && break
    sleep 2
  done
  served=$(curl -fsS --max-time 5 http://127.0.0.1:11434/api/version | sed -n 's/.*"version":"\([^"]*\)".*/\1/p') || true
  [[ "$served" == "$version" ]] || {
    cat "${RUNNER_TEMP:-/tmp}/ollama-serve.log" >&2 || true
    fail "Ollama $version does not answer on 127.0.0.1:11434 (it answers: ${served:-nothing})"
  }
  echo "Ollama $served answers on 127.0.0.1:11434, cloud models off."
}

run() {
  local group=$1 mode=${2:-run} crate args log count expected=0 passed=0 failed=0 unfinished name
  local filters=()
  umask 022
  export AXO_SUPERVISOR_TEST_IMAGE=$supervisor_root
  export AXO_RELAY_CLIENT_IMAGE=docker.io/library/rust:bookworm
  export AXOCOATL_LIVE_WEB=1 AXOCOATL_LIVE_EGRESS=1
  if [[ $group == ollama && $mode == run ]]; then
    [[ -n "${PROBE_MODEL:-}" ]] || fail "the ollama group needs PROBE_MODEL, a model the Ollama serves"
    local url=${OLLAMA_URL:-http://127.0.0.1:11434}
    export AXOCOATL_WATCH_OLLAMA_URL=$url AXOCOATL_WATCH_OLLAMA_MODEL=$PROBE_MODEL
    export AXOCOATL_TEST_OLLAMA_URL=$url AXOCOATL_TEST_OLLAMA_MODEL=$PROBE_MODEL
  fi
  local work
  work=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-suites.XXXXXX")
  for crate in $(group_crates "$group"); do
    read -r -a args <<<"$(targets "$crate")"
    filters=()
    if [[ $group == ollama ]]; then
      # The live Ollama tests are unit tests: build only the library's.
      args=(-p "$crate" --lib)
      for name in "${ollama_tests[@]}"; do
        [[ ${name%% *} == "$crate" ]] && filters+=("${name#* }")
      done
      [[ ${#filters[@]} -gt 0 ]] || continue
      filters+=(--exact)
    else
      while IFS= read -r name; do
        [[ -n $name ]] && filters+=(--skip "$name")
      done < <(skipped "$group" "$crate")
      [[ ${#filters[@]} -eq 0 ]] || filters+=(--exact)
    fi
    log="$work/$crate.list"
    cargo test --locked "${args[@]}" -- --ignored --list --format terse ${filters[@]+"${filters[@]}"} >"$log"
    count=$(grep -c ': test$' "$log" || true)
    if [[ $mode == list ]]; then
      sed -n "s/^\(.*\): test\$/$crate \1/p" "$log"
      continue
    fi
    echo "== $crate: $count ignored tests"
    expected=$((expected + count))
    log="$work/$crate.log"
    set +e
    cargo test --locked --no-fail-fast "${args[@]}" -- --ignored --test-threads=1 ${filters[@]+"${filters[@]}"} 2>&1 | tee "$log"
    set -e
    tally "$log"
    # The native Session test's other two cases: JSON and a tool round-trip.
    if [[ $group == ollama && $crate == axocoatl-daemon ]]; then
      name=session_dispatch::tests::ollama_tests::native_ollama_real_model_through_durable_controller
      for case in AXOCOATL_TEST_OLLAMA_JSON AXOCOATL_TEST_OLLAMA_TOOLS; do
        expected=$((expected + 1))
        set +e
        env "$case=1" cargo test --locked "${args[@]}" -- --ignored --test-threads=1 --exact "$name" 2>&1 | tee "$log"
        set -e
        tally "$log"
      done
    fi
  done
  rm -rf -- "$work"
  [[ $mode == run ]] || return 0
  # A test whose binary did not build or crashed has no result line.
  unfinished=$((expected - passed - failed))
  ((unfinished > 0)) || unfinished=0
  echo "Podman suites ($group): $passed of $expected ignored tests passed, $failed failed, $unfinished without a result"
  [[ $expected -gt 0 && $passed -eq $expected && $failed -eq 0 ]]
}

# Add one cargo test log's passed and failed tests to the run's.
tally() {
  passed=$((passed + $(sed -n 's/^test result: [a-zA-Z]*\. \([0-9]*\) passed.*/\1/p' "$1" | awk '{ s += $1 } END { print s + 0 }')))
  failed=$((failed + $(sed -n 's/^test result: .* \([0-9]*\) failed;.*/\1/p' "$1" | awk '{ s += $1 } END { print s + 0 }')))
}

# Every #[ignore]d test in the tree, as "crate file name" lines.
ignored_tests() {
  git ls-files -- 'crates/*.rs' 'axocoatl-cli/*.rs' 'examples/*.rs' | while IFS= read -r file; do
    awk -v file="$file" '
      /^[[:space:]]*#\[ignore/ { pending = 1; next }
      pending && /fn [A-Za-z0-9_]+/ {
        match($0, /fn [A-Za-z0-9_]+/)
        print file, substr($0, RSTART + 3, RLENGTH - 3)
        pending = 0
      }' "$file"
  done | while read -r file name; do
    case "$file" in
      axocoatl-cli/*) crate=axocoatl-cli ;;
      examples/sandbox-session/*) crate=sandbox-session ;;
      crates/*) crate=${file#crates/}; crate=${crate%%/*} ;;
      *) crate=unknown ;;
    esac
    echo "$crate $file $name"
  done
}

check() {
  local problems=0 crate file name group covered entry found
  local -a groups=(isolation daemon cli other ollama)
  local tests
  tests=$(ignored_tests)
  [[ -n $tests ]] || fail "found no #[ignore]d test"
  while read -r crate file name; do
    covered=''
    if [[ $file == crates/axocoatl-isolation/tests/supervisor_runtime.rs ]]; then
      covered='scripts/test-exec-supervisor-runtime.sh'
    else
      for group in "${groups[@]}"; do
        [[ " $(group_crates "$group") " == *" $crate "* ]] && { covered=$group; break; }
      done
    fi
    if [[ -z $covered ]]; then
      echo "test-podman-suites: $file: $name is #[ignore]d, but no group runs $crate" >&2
      problems=$((problems + 1))
    fi
  done <<<"$tests"
  for entry in "${exempt[@]%%|*}" "${ollama_tests[@]}"; do
    crate=${entry%% *}
    name=${entry#* }
    found=$(awk -v crate="$crate" -v name="${name##*::}" '$1 == crate && $3 == name' <<<"$tests")
    if [[ -z $found ]]; then
      echo "test-podman-suites: $crate has no #[ignore]d test $name any more; update this script" >&2
      problems=$((problems + 1))
    fi
  done
  for entry in "${exempt[@]}"; do
    [[ $entry == *'|'?* ]] || { echo "test-podman-suites: ${entry%%|*} is left out without a reason" >&2; problems=$((problems + 1)); }
  done
  ((problems == 0)) || exit 1
  echo "Podman suites: every one of $(wc -l <<<"$tests" | tr -d ' ') #[ignore]d tests is in a group; ${#exempt[@]} are left out, each with a reason:"
  for entry in "${exempt[@]}"; do
    echo "  ${entry%%|*}: ${entry#*|}"
  done
}

usage='usage: test-podman-suites.sh prepare GROUP | run GROUP | list GROUP | check | serve-ollama VERSION'
[[ $# -ge 1 ]] || fail "$usage"
case "$1" in
  serve-ollama)
    [[ $# -eq 2 ]] || fail "usage: test-podman-suites.sh serve-ollama VERSION"
    serve_ollama "$2"
    ;;
  prepare | run)
    [[ $# -eq 2 ]] || fail "usage: test-podman-suites.sh $1 GROUP"
    "$1" "$2"
    ;;
  list)
    [[ $# -eq 2 ]] || fail "usage: test-podman-suites.sh list GROUP"
    group_crates "$2" >/dev/null
    run "$2" list
    ;;
  check)
    [[ $# -eq 1 ]] || fail "usage: test-podman-suites.sh check"
    check
    ;;
  *) fail "$usage" ;;
esac
