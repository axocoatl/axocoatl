#!/bin/bash
# Clean-machine new-user gate, macOS plumbing (docs/RELEASE_GATE.md, Tier B).
#
# On a GitHub-hosted macOS runner, which cannot run a Podman machine: the
# README's install command into a brand-new HOME on the runner's own PATH,
# with /usr/local/bin unwritable (the installer's copy names a locked
# temporary directory instead), then `axocoatl doctor --json` and
# `axocoatl onboard --plan` against a Podman stand-in (podman-shim.sh) in each
# state a new Mac user meets: no Podman (and, on Apple silicon, no Homebrew
# either), no machine, a stopped machine, a running one, a rootful one, and
# Podman 6. A state without Podman or Homebrew runs under sandbox-exec, so
# the programs the runner has where their installers put them stay unseen.
# The x86_64 build on an Intel runner gets the Intel answers. Nothing is
# installed or created. Evidence goes to
# <evidence-root>/<version>/<commit>/<label>.json; Tier B is not a required
# path.
#
# Usage:
#   newuser-mac-ci.sh --tarball DIST/axocoatl-vX.Y.Z-<mac target>.tar.gz [options]
# Options:
#   --label NAME         evidence path (default B-arm or B-intel by machine)
#   --commit SHA         commit the tarball was built from (default: HEAD)
#   --evidence-root DIR  default: <repo>/release-evidence
#   --no-plan            skip `onboard --plan`, which probes whether port 8080
#                        is free (for a run on a machine whose 8080 is in use)
#
# Depends on (being built in parallel; see docs/RELEASE_GATE.md):
#   doctor --json and its Podman findings, and onboard --plan.
# Scripts handed to `node -e` read their values from process.argv.
# shellcheck disable=SC2016
set -uo pipefail

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=scripts/release-gate/lib.sh
. "$script_dir/lib.sh"

usage() {
  awk 'NR == 1 { next } /^#/ { if ($0 ~ /shellcheck|positional param|take their values|read their values|^# parameters\.$/) next; sub(/^# ?/, ""); print; next } { exit }' "$0"
  exit 2
}

tarball='' label='' commit='' evidence_root="$gate_repo/release-evidence" plan=1
while [ $# -gt 0 ]; do
  case "$1" in
    --tarball) tarball=${2:?}; shift ;;
    --label) label=${2:?}; shift ;;
    --commit) commit=${2:?}; shift ;;
    --evidence-root) evidence_root=${2:?}; shift ;;
    --no-plan) plan=0 ;;
    -h | --help) usage ;;
    *) gate_die "unknown argument: $1 (see --help)" ;;
  esac
  shift
done

[ "$(uname -s)" = Darwin ] || gate_die "this is the macOS plumbing gate (Tier B); on Linux use newuser-linux.sh"
[ "$(id -u)" != 0 ] || gate_die "run the gate as a normal user"
[ -n "$tarball" ] && [ -f "$tarball" ] || gate_die "--tarball must be the candidate's macOS release archive"
tarball=$(CDPATH='' cd -- "$(dirname -- "$tarball")" && pwd)/${tarball##*/}
for tool in node git curl tar sandbox-exec; do
  command -v "$tool" >/dev/null 2>&1 || gate_die "$tool is required"
done
[ -n "$commit" ] || commit=$(git -C "$gate_repo" rev-parse HEAD)
gate_valid_commit "$commit" || gate_die "--commit must be a full 40-character commit SHA"
version=${tarball##*/axocoatl-v}
version=${version%%-*}
gate_valid_version "$version" || gate_die "no X.Y.Z version in the tarball name ${tarball##*/}"
case "${tarball##*/}" in
  *-x86_64-apple-darwin.tar.gz) intel=1 ;;
  *-aarch64-apple-darwin.tar.gz) intel=0 ;;
  *) gate_die "not a macOS release archive: ${tarball##*/}" ;;
esac
if [ -z "$label" ]; then
  if [ "$intel" = 1 ]; then label=B-intel; else label=B-arm; fi
fi
gate_valid_name "$label" || gate_die "--label must be letters, digits and dashes"

work=$(mktemp -d "${GATE_TMPDIR:-${TMPDIR:-/tmp}}/axo-gate-b.XXXXXX")
stub_pid=''
# shellcheck disable=SC2329 # the EXIT trap runs it
cleanup() {
  [ -n "$stub_pid" ] && kill "$stub_pid" 2>/dev/null && wait "$stub_pid" 2>/dev/null
  chmod -R u+w "$work" 2>/dev/null
  rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

gate_begin "$evidence_root" "$version" "$commit" "$label" B
GATE_RUNNER="${RUNNER_ENVIRONMENT:-local} ${ImageOS:-} ${ImageVersion:-}"
export GATE_RUNNER
gate_host_json "$GATE_TMP/host.json"
exec > >(tee "$GATE_ATTACH/transcript.txt") 2>&1
finish() {
  gate_tools_json "$GATE_TMP/tools.json"
  cp "$work/podman-calls.log" "$GATE_ATTACH/logs/podman-calls.log" 2>/dev/null || true
  gate_finish --json host="$GATE_TMP/host.json" --json tools="$GATE_TMP/tools.json"
  exit $?
}

# 1. A brand-new user on the runner's own PATH, without ~/.local/bin, any
# axocoatl, Podman or Ollama: Podman is what the stand-in plays.
unset XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME ZDOTDIR AXOCOATL_DATA_DIR AXOCOATL_CONFIG AXOCOATL_TOKEN AXOCOATL_SOCKET_PATH OLLAMA_HOST CONTAINER_CONNECTION CONTAINER_HOST
export HOME="$work/home" SHELL=/bin/zsh
mkdir -p "$HOME"
PATH=$(gate_new_user_path)
PATH=$(gate_path_without podman "$work/hide-podman")
PATH=$(gate_path_without ollama "$work/hide-ollama")
GATE_BASE_PATH=$PATH
export PATH GATE_BASE_PATH
if command -v axocoatl >/dev/null 2>&1 || command -v podman >/dev/null 2>&1; then
  gate_record "start without axocoatl or podman on PATH" fail '' '' '' "$(command -v axocoatl) $(command -v podman)"
else
  gate_record "start without axocoatl or podman on PATH" pass '' '' '' "HOME=$HOME"
fi

# 2. The scripted model server, for the models doctor and onboarding read.
node "$script_dir/stub-model-server.mjs" --port 0 --port-file "$work/stub.port" \
  --log "$GATE_ATTACH/logs/stub-model-server.jsonl" >"$GATE_ATTACH/logs/stub-model-server.log" 2>&1 &
stub_pid=$!
for _ in $(seq 1 100); do [ -s "$work/stub.port" ] && break; sleep 0.1; done
stub_url="http://127.0.0.1:$(cat "$work/stub.port" 2>/dev/null)"
gate_check "scripted model server" "it did not start; see logs/stub-model-server.log" -- curl -fsS "$stub_url/api/version"

# 3. The README's install command, with /usr/local/bin unwritable: the
# published installer, except that its /usr/local/bin is a locked
# temporary directory, so it installs into ~/.local/bin as it would for a
# user who cannot write /usr/local/bin.
stage="$work/release"
gate_stage_release "$tarball" "$stage" >/dev/null
site="$work/site"
mkdir -p "$site" "$work/locked-bin"
chmod 0555 "$work/locked-bin"
sed "s#/usr/local/bin#$work/locked-bin#g" "$gate_repo/scripts/install.sh" >"$site/install.sh"
if ! install_command=$(node "$gate_commands_js" install "$gate_repo/README.md" 2>&1); then
  gate_record "the README shows the install command" fail '' '' '' "$install_command"
  finish
fi
gate_install_new_user "$stage" "$site" "$install_command" || finish
GATE_AXOCOATL_VERSION=$(axocoatl --version 2>/dev/null)
export GATE_AXOCOATL_VERSION
case "$GATE_AXOCOATL_VERSION" in
  *"$version"*) gate_record "axocoatl --version names $version" pass '' '' '' "$GATE_AXOCOATL_VERSION" ;;
  *) gate_record "axocoatl --version names $version" fail '' '' '' "$GATE_AXOCOATL_VERSION" ;;
esac

# The Podman stand-in, and a port nothing listens on for doctor's daemon
# check.
shim="$work/podman-bin"
mkdir -p "$shim"
cp "$script_dir/podman-shim.sh" "$shim/podman"
chmod 0755 "$shim/podman"
export PODMAN_SHIM_STATE="$work/podman.state" PODMAN_SHIM_LOG="$work/podman-calls.log"
: >"$PODMAN_SHIM_LOG"
port=8200
while (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; do port=$((port + 1)); done
# PATH as the installer left it (axocoatl on it, as the user was told).
user_path=$PATH
path_for() { # path_for STATE: that PATH with the stand-in, or without any Podman
  case "$1" in
    none) printf '%s\n' "$user_path" ;;
    none-no-brew) PATH="$user_path" gate_path_without brew "$work/hide-brew" ;;
    *) printf '%s\n' "$shim:$user_path" ;;
  esac
}
# hidden_for STATE: the programs a Mac in STATE does not have.
hidden_for() {
  case "$1" in
    none) echo podman ;;
    none-no-brew) echo podman brew ;;
  esac
}
# without "PROGRAMS" COMMAND...: COMMAND, unable to see PROGRAMS where
# installers put them. Doctor and onboarding look for Homebrew and Podman
# there (Homebrew's /opt/homebrew/bin and /usr/local/bin, the podman.io
# package's /opt/podman/bin) whatever PATH says, and GitHub's macOS runners
# have Homebrew there, so a PATH without them cannot play a Mac that lacks
# them. sandbox-exec denies reading those files, as on a Mac that never
# installed them.
without() {
  local programs=$1 profile='(version 1)(allow default)(deny file-read*' program dir
  shift
  [ -n "$programs" ] || { "$@"; return; }
  for program in $programs; do
    for dir in /opt/homebrew/bin /usr/local/bin /opt/podman/bin; do
      profile="$profile (literal \"$dir/$program\")"
    done
  done
  sandbox-exec -p "$profile)" "$@"
}

# 4. doctor --json in each state: what its sandbox section must say.
if [ "$intel" = 1 ]; then
  none_fix='Podman 6 no longer supports Intel Macs'
else
  none_fix='brew install podman'
fi
doctor_states=(none no-machine stopped running rootful podman6)
[ "$intel" = 0 ] && doctor_states+=(none-no-brew)
for state in "${doctor_states[@]}"; do
  home="$work/doctor-$state"
  mkdir -p "$home"
  config=$(HOME="$home" gate_config_path)
  mkdir -p "$(dirname -- "$config")"
  printf 'server:\n  port: %s\n  host: "127.0.0.1"\nproviders:\n  ollama:\n    base_url: "%s"\n' "$port" "$stub_url" >"$config"
  case "$state" in none | none-no-brew) ;; *) printf '%s\n' "$state" >"$PODMAN_SHIM_STATE" ;; esac
  printf '== %s\n' "$state" >>"$PODMAN_SHIM_LOG"
  without "$(hidden_for "$state")" env HOME="$home" PATH="$(path_for "$state")" axocoatl doctor --json \
    >"$GATE_ATTACH/doctor-$state.json" 2>"$GATE_ATTACH/logs/doctor-$state.log" </dev/null
  status=$?
  if detail=$(node -e '
      const [file, state, noneFix] = process.argv.slice(1);
      let doc;
      try { doc = JSON.parse(require("fs").readFileSync(file, "utf8")); } catch (error) { console.log(`not JSON: ${error.message}`); process.exit(1); }
      const sandbox = doc?.sections?.sandbox;
      if (!Array.isArray(sandbox)) { console.log("no sandbox section"); process.exit(1); }
      const text = (item) => `${item.name} ${item.detail} ${item.fix ?? ""}`;
      const has = (status, words) => sandbox.some((item) => item.status === status && text(item).includes(words));
      const fails = sandbox.filter((item) => item.status === "fail");
      const expect = {
        "none": [() => has("fail", noneFix), `a FAIL whose fix names: ${noneFix}`],
        "none-no-brew": [() => has("fail", "https://podman.io") && !has("fail", "it installs Podman"), "a FAIL whose fix names the podman.io installer, not onboarding installing Podman"],
        "no-machine": [() => has("fail", "podman machine init"), "a FAIL whose fix runs podman machine init"],
        "stopped": [() => fails.length === 0 && has("ok", "stopped"), "OK: the stopped machine starts on first use; no FAIL"],
        "running": [() => fails.length === 0 && has("ok", "ready"), "OK: ready; no FAIL"],
        "rootful": [() => has("fail", "--rootful=false"), "a FAIL whose fix switches the machine to rootless"],
        "podman6": [() => fails.length === 0 && has("warn", "libkrun") && !sandbox.some((item) => item.name === "Podman version"), "Podman 6.1.4 is tested: no version WARN; a WARN naming the untested libkrun provider; no FAIL"],
      }[state];
      const summary = sandbox.map((item) => `${item.status} ${item.name}`).join("; ");
      console.log(`${expect[1]} | doctor: ${summary}`);
      process.exit(expect[0]() ? 0 : 1);
    ' "$GATE_ATTACH/doctor-$state.json" "$state" "$none_fix"); then
    gate_record "doctor --json, Podman $state" pass "$status" '' '' "$detail"
  else
    gate_record "doctor --json, Podman $state" fail "$status" '' '' "$detail"
  fi
done

# 5. onboard --plan in each state a first run meets: the consent it would
# ask for, and nothing changed.
if [ "$plan" = 1 ]; then
  plan_states=(none no-machine stopped)
  [ "$intel" = 0 ] && plan_states+=(none-no-brew)
  for state in "${plan_states[@]}"; do
    home="$work/plan-$state"
    mkdir -p "$home"
    case "$state" in none | none-no-brew) ;; *) printf '%s\n' "$state" >"$PODMAN_SHIM_STATE" ;; esac
    printf '== plan %s\n' "$state" >>"$PODMAN_SHIM_LOG"
    log="$GATE_ATTACH/logs/onboard-plan-$state.log"
    without "$(hidden_for "$state")" env HOME="$home" PATH="$(path_for "$state")" axocoatl onboard --plan \
      --writer ollama --ollama-url "$stub_url" --no-pull --port "$port" </dev/null >"$log" 2>&1
    status=$?
    case "$state" in
      none) if [ "$intel" = 1 ]; then words='Podman 6 no longer supports Intel Macs'; else words='Install Podman now with `brew install podman`'; fi ;;
      none-no-brew) words='https://podman.io' ;;
      no-machine) words='would ask: Create the Podman machine your Sessions run in?' ;;
      stopped) words='would ask: Start podman-machine-default now? (`podman machine start`)' ;;
    esac
    if grep -qF -- "$words" "$log"; then
      gate_record "onboard --plan, Podman $state: asks or says what to do" pass "$status" '' '' "$words"
    else
      gate_record "onboard --plan, Podman $state: asks or says what to do" fail "$status" '' '' \
        "no '$words' in its output: $(grep -v '^[[:space:]]*$' "$log" | tail -n 3 | tr '\n' ' ' | cut -c1-400)"
    fi
    gate_check "onboard --plan, Podman $state: changed nothing" "it wrote $(HOME="$home" gate_config_path)" -- \
      test ! -e "$(HOME="$home" gate_config_path)"
  done
else
  gate_skip "onboard --plan in each Podman state" "--no-plan" optional
fi

# Neither doctor nor --plan may create, start or change a machine.
gate_check "doctor and --plan ran only read-only podman commands" \
  "$(grep -E 'machine (init|start|stop|rm|set)|system (migrate|reset)' "$PODMAN_SHIM_LOG" | head -n 3 | tr '\n' ' ')" -- \
  sh -c '! grep -Eq "machine (init|start|stop|rm|set)|system (migrate|reset)" "$1"' gate-podman "$PODMAN_SHIM_LOG"

finish
