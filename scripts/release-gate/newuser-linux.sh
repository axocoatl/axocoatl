#!/usr/bin/env bash
# Clean-machine new-user gate, Linux (docs/RELEASE_GATE.md, Tier A and D1).
#
# Tier A (CI, default): a brand-new HOME (seeded from /etc/skel, as adduser
# does) and a PATH without ~/.local/bin; the README's install command
# against the just-built release tarball; onboarding with no terminal
# against a scripted model server (an Ollama 0.40.6 with cloud models off);
# doctor; `axocoatl dev`; the README's `axocoatl run fix` command verbatim on
# every tests/fixtures/newuser fixture; the documented receipt and record
# checks; and the negative cases. Evidence goes to
# <evidence-root>/<version>/<commit>/<label>.json and <label>/.
#
# Usage:
#   newuser-linux.sh --tarball DIST/axocoatl-vX.Y.Z-<target>.tar.gz [options]
# Options:
#   --label NAME            evidence path (default A-x86 or A-arm by machine)
#   --commit SHA            commit the tarball was built from (default: HEAD)
#   --evidence-root DIR     default: <repo>/release-evidence
#   --port N                daemon port (default: a free one from 8200; with
#                           --live, the one onboarding chooses)
#   --expect-refusal TEXT   only check that onboarding and doctor refuse this
#                           machine, naming TEXT (the Ubuntu 22.04 leg)
#   --live                  Tier D: no stub; in this account's own, brand-new
#                           HOME, run onboarding interactively and use the
#                           models it configures
#   --system-changes        CI only: allow the empty-/etc/subuid case, which
#                           edits /etc/subuid with sudo and restores it
#   --command TEXT          run TEXT on each fixture instead of the README's
#                           command (live VMs whose models differ)
#   --keep-work             keep the temporary HOME and repositories
#
# Depends on (being built in parallel; see docs/RELEASE_GATE.md):
#   onboard --yes/--writer/--ollama-url/--no-pull/--start/--port flags and its
#   exit codes, doctor --json, and the run's `image: …` start line.
# Scripts handed to `bash -c` take their values as positional parameters.
# shellcheck disable=SC2016
set -uo pipefail

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=scripts/release-gate/lib.sh
. "$script_dir/lib.sh"

usage() {
  awk 'NR == 1 { next } /^#/ { if ($0 ~ /shellcheck|positional param|take their values|read their values|^# parameters\.$/) next; sub(/^# ?/, ""); print; next } { exit }' "$0"
  exit 2
}

original_args=("$@")
tarball='' label='' commit='' evidence_root="$gate_repo/release-evidence" port='' port_given=0
expect_refusal='' live=0 system_changes=0 keep_work=0 run_command=
while [ $# -gt 0 ]; do
  case "$1" in
    --tarball) tarball=${2:?}; shift ;;
    --label) label=${2:?}; shift ;;
    --commit) commit=${2:?}; shift ;;
    --evidence-root) evidence_root=${2:?}; shift ;;
    --port) port=${2:?}; port_given=1; shift ;;
    --expect-refusal) expect_refusal=${2:?}; shift ;;
    --live) live=1 ;;
    --system-changes) system_changes=1 ;;
    --keep-work) keep_work=1 ;;
    --command) run_command=${2:?}; shift ;;
    -h | --help) usage ;;
    *) gate_die "unknown argument: $1 (see --help)" ;;
  esac
  shift
done

# GATE_SIMULATION=1 runs this script against a stand-in axocoatl on any host
# (test-release-gate.sh), with an installer whose /usr/local/bin is a locked
# temporary directory.
simulation=${GATE_SIMULATION:-0}
[ "$(uname -s)" = Linux ] || [ "$simulation" = 1 ] || gate_die "this is the Linux gate; on macOS use newuser-mac.sh"
[ "$(id -u)" != 0 ] || gate_die "run the gate as a normal user: Axocoatl runs need rootless Podman"
[ -n "$tarball" ] || gate_die "--tarball is required (see --help)"
[ -f "$tarball" ] || gate_die "no such tarball: $tarball"
tarball=$(CDPATH='' cd -- "$(dirname -- "$tarball")" && pwd)/${tarball##*/}
for tool in node git curl tar; do
  command -v "$tool" >/dev/null 2>&1 || gate_die "$tool is required"
done
[ -n "$commit" ] || commit=$(git -C "$gate_repo" rev-parse HEAD)
gate_valid_commit "$commit" || gate_die "--commit must be a full 40-character commit SHA"
version=${tarball##*/axocoatl-v}
version=${version%%-*}
gate_valid_version "$version" || gate_die "no X.Y.Z version in the tarball name ${tarball##*/}"
case "$port" in *[!0-9]* | 0*) gate_die "--port must be a port number" ;; esac
if [ "$system_changes" = 1 ] && [ "${GITHUB_ACTIONS:-}" != true ]; then
  gate_die "--system-changes edits /etc/subuid and /etc/subgid with sudo; it runs only on a CI runner (GITHUB_ACTIONS=true)"
fi
if [ -z "$label" ]; then
  case "$(uname -m)" in
    x86_64 | amd64) label=A-x86 ;;
    aarch64 | arm64) label=A-arm ;;
    *) gate_die "no default label for $(uname -m); pass --label" ;;
  esac
  [ -n "$expect_refusal" ] && label="$label-refusal"
  [ "$live" = 1 ] && label=D1
fi
gate_valid_name "$label" || gate_die "--label must be letters, digits and dashes (A-x86, D1, ...)"

# A live run uses this account's own HOME, as a user made with adduser has
# it: /etc/skel's files and nothing of Axocoatl's yet.
if [ "$live" = 1 ]; then
  for state in "${XDG_CONFIG_HOME:-$HOME/.config}/axocoatl" "${XDG_DATA_HOME:-$HOME/.local/share}/axocoatl" \
    "$HOME/.local/state/axocoatl" "$HOME/.axocoatl" "$HOME/.local/bin/axocoatl" \
    "$HOME/Library/Application Support/Axocoatl"; do
    [ ! -e "$state" ] || gate_die "--live needs a brand-new account (made with adduser for the gate): $state already exists"
  done
fi

work=$(mktemp -d "${GATE_TMPDIR:-/tmp}/axo-gate.XXXXXX")
real_home=$HOME
stub_pid='' dev_pid='' subuid_backup=''
# shellcheck disable=SC2329 # the EXIT trap runs it
cleanup() {
  [ -n "$dev_pid" ] && kill "$dev_pid" 2>/dev/null && wait "$dev_pid" 2>/dev/null
  [ -n "$stub_pid" ] && kill "$stub_pid" 2>/dev/null && wait "$stub_pid" 2>/dev/null
  restore_subuid
  if [ "$keep_work" = 1 ]; then
    gate_say "kept $work"
  else
    chmod -R u+w "$work" 2>/dev/null
    HOME=$real_home podman unshare rm -rf "$work" 2>/dev/null || rm -rf "$work" 2>/dev/null
  fi
}
# Put /etc/subuid and /etc/subgid back from the backup; on any failure keep
# the backup (and the whole work directory) and say where it is.
restore_subuid() {
  [ -n "$subuid_backup" ] || return 0
  if sudo -n cp "$subuid_backup/subuid" /etc/subuid && sudo -n cp "$subuid_backup/subgid" /etc/subgid \
    && cmp -s "$subuid_backup/subuid" /etc/subuid && cmp -s "$subuid_backup/subgid" /etc/subgid; then
    subuid_backup=''
    gate_say "restored /etc/subuid and /etc/subgid"
    return 0
  fi
  keep_work=1
  gate_say "FAILED to restore /etc/subuid and /etc/subgid; the originals are kept in $subuid_backup: restore them with sudo cp"
  return 1
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# A live run answers onboarding's prompts in a real terminal, so the whole
# session is recorded with script(1); otherwise output is teed.
if [ "$live" = 1 ] && [ -z "${GATE_IN_SCRIPT:-}" ]; then
  transcript="${GATE_TMPDIR:-/tmp}/axo-gate-live-$(date -u +%Y%m%dT%H%M%SZ).txt"
  echo "Recording this session to $transcript"
  rm -rf "$work"
  GATE_IN_SCRIPT=$transcript exec script -q -e -c "$(printf '%q ' bash "$0" "${original_args[@]}")" "$transcript"
fi

gate_begin "$evidence_root" "$version" "$commit" "$label" "$([ "$live" = 1 ] && echo D || echo A)"
GATE_RUNNER="${RUNNER_ENVIRONMENT:-local} ${ImageOS:-} ${ImageVersion:-}"
export GATE_RUNNER
gate_host_json "$GATE_TMP/host.json"
if [ "$live" = 0 ]; then
  exec > >(tee "$GATE_ATTACH/transcript.txt") 2>&1
fi
# Write this path's evidence and exit with its verdict (0 pass, 1 fail).
finish() {
  [ -n "${GATE_IN_SCRIPT:-}" ] && cp "$GATE_IN_SCRIPT" "$GATE_ATTACH/transcript.txt" 2>/dev/null
  gate_tools_json "$GATE_TMP/tools.json"
  cp "$work/guard.log" "$GATE_ATTACH/logs/guard.log" 2>/dev/null || true
  gate_finish --json host="$GATE_TMP/host.json" --json tools="$GATE_TMP/tools.json"
  exit $?
}

# 1. A brand-new user: a fresh HOME (or, live, this brand-new account's
# own), no ~/.local/bin, no axocoatl on PATH.
unset XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME AXOCOATL_DATA_DIR AXOCOATL_CONFIG AXOCOATL_TOKEN AXOCOATL_SOCKET_PATH OLLAMA_HOST CONTAINER_CONNECTION
if [ "$live" = 0 ]; then
  new_home="$work/home"
  mkdir -p "$new_home"
  [ -d /etc/skel ] && cp -R /etc/skel/. "$new_home/" 2>/dev/null
  export HOME="$new_home"
  # The login shell adduser gives a new user (zsh on a simulated macOS run).
  if [ "$(uname -s)" = Darwin ]; then export SHELL=/bin/zsh; else export SHELL=/bin/bash; fi
fi
GATE_BASE_PATH=$(gate_new_user_path)
export PATH="$GATE_BASE_PATH" GATE_BASE_PATH
if command -v axocoatl >/dev/null 2>&1; then
  gate_record "start without axocoatl on PATH" fail '' '' '' "$(command -v axocoatl)"
else
  gate_record "start without axocoatl on PATH" pass '' '' '' "HOME=$HOME"
fi

# 2. The scripted model server.
stub_url=''
if [ "$live" = 0 ]; then
  node "$script_dir/stub-model-server.mjs" --port 0 --port-file "$work/stub.port" \
    --fixes "${GATE_STUB_FIXES:-$script_dir/stub-fixes.json}" \
    --log "$GATE_ATTACH/logs/stub-model-server.jsonl" >"$GATE_ATTACH/logs/stub-model-server.log" 2>&1 &
  stub_pid=$!
  for _ in $(seq 1 100); do [ -s "$work/stub.port" ] && break; sleep 0.1; done
  if [ -s "$work/stub.port" ] && curl -fsS "http://127.0.0.1:$(cat "$work/stub.port")/api/version" >/dev/null; then
    stub_url="http://127.0.0.1:$(cat "$work/stub.port")"
    GATE_OLLAMA_VERSION="gate stub reporting $(curl -fsS "$stub_url/api/version" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>process.stdout.write(JSON.parse(s).version))')"
    export GATE_OLLAMA_VERSION
    gate_record "scripted model server ($GATE_OLLAMA_VERSION, cloud off)" pass '' '' '' "$stub_url"
  else
    gate_record "scripted model server" fail '' '' '' "it did not start; see logs/stub-model-server.log"
  fi
fi
stub_stat() {
  curl -fsS "$stub_url/__gate/stats" | node -e '
    let s = ""; process.stdin.on("data", (d) => s += d).on("end", () => {
      const stats = JSON.parse(s); const key = process.argv[1];
      process.stdout.write(String(key === "model-calls" ? stats.writer + stats.reviewer + stats.other : stats[key]));
    });' "$1"
}

# 3. The README's install command, as written, against this tarball.
stage="$work/release"
gate_stage_release "$tarball" "$stage" >/dev/null
site="$gate_repo/scripts"
if [ "$simulation" = 1 ]; then
  site="$work/site"
  mkdir -p "$site" "$work/locked-bin"
  chmod 0555 "$work/locked-bin"
  sed "s#/usr/local/bin#$work/locked-bin#g" "$gate_repo/scripts/install.sh" >"$site/install.sh"
  gate_record "simulation: a stand-in axocoatl and a sandboxed installer" fail '' '' '' "GATE_SIMULATION=1 is never release evidence"
fi
if ! install_command=$(node "$gate_commands_js" install "$gate_repo/README.md" 2>&1); then
  gate_record "the README shows the install command" fail '' '' '' "$install_command"
  finish
fi
gate_install_new_user "$stage" "$site" "$install_command" || finish
if [ "$(gate_sha256 "$GATE_INSTALLED")" = "$(tar -xzOf "$tarball" axocoatl | gate_sha256 /dev/stdin)" ]; then
  gate_record "installed binary is the tarball's" pass '' '' '' "$(gate_sha256 "$GATE_INSTALLED")"
else
  gate_record "installed binary is the tarball's" fail '' '' '' "sha256 differs"
fi
GATE_AXOCOATL_VERSION=$(axocoatl --version 2>/dev/null)
export GATE_AXOCOATL_VERSION
case "$GATE_AXOCOATL_VERSION" in
  *"$version"*) gate_record "axocoatl --version names $version" pass '' '' '' "$GATE_AXOCOATL_VERSION" ;;
  *) gate_record "axocoatl --version names $version" fail '' '' '' "$GATE_AXOCOATL_VERSION" ;;
esac

# A port nothing listens on, from 8200. A live run lets onboarding choose
# (8080 when it is free) unless --port was given, and reads it back.
if [ -z "$port" ] && [ "$live" = 0 ]; then
  port=8200
  while (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; do port=$((port + 1)); done
fi
guard="$work/guard"
gate_guard_shims "$guard" "$work/guard.log"
writer_model=${GATE_WRITER_MODEL:-ollama:qwen3-coder:30b}
reviewer_model=${GATE_REVIEWER_MODEL:-ollama:gpt-oss:20b}
onboard_flags=(--yes --writer ollama --ollama-url "$stub_url" --writer-model "$writer_model"
  --reviewer "$reviewer_model" --no-pull --start none --no-smoke-test --port "$port")

# The Ubuntu 22.04 leg: onboarding and doctor refuse cleanly, naming why.
if [ -n "$expect_refusal" ]; then
  sudo_mark=$(gate_sudo_mark)
  gate_step "onboard refuses this machine (exit 1)" --expect 1 -- \
    env PATH="$guard:$PATH" axocoatl onboard "${onboard_flags[@]}" </dev/null
  if grep -q -- "$expect_refusal" "$GATE_ATTACH/logs/$(gate_slug "onboard refuses this machine (exit 1)").log"; then
    gate_record "onboard names $expect_refusal" pass '' '' '' ''
  else
    gate_record "onboard names $expect_refusal" fail '' '' '' "the refusal does not mention $expect_refusal"
  fi
  gate_doctor_json "doctor --json is not ready" 1 "$GATE_ATTACH/doctor-refusal.json"
  if node -e '
      const doc = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
      const items = Object.values(doc.sections || {}).flat();
      process.exit(doc.ready === false && items.some((i) => i.status === "fail" && JSON.stringify(i).includes(process.argv[2])) ? 0 : 1);
    ' "$GATE_ATTACH/doctor-refusal.json" "$expect_refusal" 2>/dev/null; then
    gate_record "doctor fails Podman, naming $expect_refusal" pass '' '' '' ''
  else
    gate_record "doctor fails Podman, naming $expect_refusal" fail '' '' '' "no failing item naming $expect_refusal"
  fi
  gate_check "nothing ran sudo or a package manager through PATH" "$(head -n 3 "$work/guard.log" | tr '\n' ' ')" -- \
    test ! -s "$work/guard.log"
  gate_sudo_check "nothing ran sudo by any path (system journal)" "$sudo_mark"
  finish
fi

# 4. Onboarding, then doctor.
if [ "$live" = 1 ]; then
  # Every consent prompt is answered by the operator. --start none: the gate
  # starts `axocoatl dev` itself, with its output (and sign-in link) kept
  # out of the transcript.
  onboard_live=(axocoatl onboard --start none)
  [ "$port_given" = 1 ] && onboard_live+=(--port "$port")
  gate_step "onboard (interactive, every consent answered by the operator; --start none)" --interactive -- "${onboard_live[@]}"
else
  sudo_mark=$(gate_sudo_mark)
  gate_step "onboard --yes --writer ollama with no terminal" -- \
    env PATH="$guard:$PATH" axocoatl onboard "${onboard_flags[@]}" </dev/null
  gate_check "onboard ran no sudo and no package manager through PATH" "$(head -n 3 "$work/guard.log" | tr '\n' ' ')" -- \
    test ! -s "$work/guard.log"
  gate_sudo_check "onboard ran no sudo by any path (system journal)" "$sudo_mark"
  gate_check "--no-pull pulled nothing" "the model server saw $(stub_stat pulls) pulls" -- \
    test "$(stub_stat pulls)" = 0
fi
config=$(gate_config_path)
if [ ! -f "$config" ]; then
  gate_record "onboarding wrote $config" fail '' '' '' "no config.yaml where Axocoatl keeps it"
elif [ "$live" = 0 ]; then
  if [ "$(gate_run_model "$config" writer)" = "$writer_model" ] && [ "$(gate_run_model "$config" reviewer)" = "$reviewer_model" ]; then
    gate_record "config.yaml holds the run models (run.models)" pass '' '' '' "$config"
  else
    gate_record "config.yaml holds the run models (run.models)" fail '' '' '' \
      "run.models is writer '$(gate_run_model "$config" writer)', reviewer '$(gate_run_model "$config" reviewer)'"
  fi
  gate_check "config.yaml uses the free port $port" "server.port is '$(gate_server_port "$config")'" -- \
    test "$(gate_server_port "$config")" = "$port"
else
  configured=$(gate_server_port "$config")
  if [ "$port_given" = 1 ] && [ "$configured" != "$port" ]; then
    gate_record "config.yaml uses --port $port" fail '' '' '' "server.port is '$configured'"
  fi
  port=${configured:-8080}
  gate_record "onboarding wrote $config" pass '' '' '' "server.port $port; writer $(gate_run_model "$config" writer); reviewer $(gate_run_model "$config" reviewer)"
fi
gate_doctor_json "doctor --json says ready" 0 "$GATE_ATTACH/doctor.json"
if node -e '
    const doc = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
    const sections = doc.sections || {};
    const missing = ["sandbox", "models", "writers", "daemon"].filter((s) => !Array.isArray(sections[s]));
    const failing = Object.values(sections).flat().filter((i) => i.status === "fail");
    if (doc.ready !== true || missing.length || failing.length) {
      console.log(JSON.stringify({ ready: doc.ready, missing, failing }));
      process.exit(1);
    }' "$GATE_ATTACH/doctor.json" >"$work/doctor-check.txt" 2>&1; then
  gate_record "doctor: ready, no FAIL in sandbox, models, writers, daemon" pass '' '' '' ''
else
  gate_record "doctor: ready, no FAIL in sandbox, models, writers, daemon" fail '' '' '' "$(cut -c1-500 "$work/doctor-check.txt")"
fi
gate_step "doctor exits 0" -- axocoatl doctor
gate_doctor_text_check "$GATE_ATTACH/logs/$(gate_slug "doctor exits 0").log"

# 5. The daemon, on its configured port.
cd "$work" || finish
axocoatl dev >"$GATE_ATTACH/logs/dev.log" 2>&1 &
dev_pid=$!
ready=0
for _ in $(seq 1 600); do
  if curl -fsS "http://127.0.0.1:$port/health/live" >/dev/null 2>&1; then ready=1; break; fi
  kill -0 "$dev_pid" 2>/dev/null || break
  sleep 0.5
done
if [ "$ready" = 1 ]; then
  gate_record "axocoatl dev serves on port $port" pass '' '' '' ''
else
  gate_record "axocoatl dev serves on port $port" fail '' '' '' "$(tail -n 3 "$GATE_ATTACH/logs/dev.log" | tr '\n' ' ')"
fi

# 6-7. The README's command, verbatim, on every fixture, then the
# documented receipt and record checks.
readme_command=$(node "$gate_commands_js" readme-run "$gate_repo/README.md") \
  || gate_record "README shows an axocoatl run fix command" fail '' '' '' "none found"
if [ "$live" = 1 ] && [ -n "$readme_command" ]; then
  # A live VM runs the models onboarding configured (run.models): the
  # README's command with any --model options left out.
  readme_command=$(gate_without_models "$readme_command")
fi
[ -n "$run_command" ] && readme_command=$run_command
printf '%s\n' "$readme_command" >"$GATE_ATTACH/readme-command.txt"
fixtures=()
while IFS= read -r fixture; do fixtures+=("$fixture"); done < <(gate_fixture_names)
for fixture in "${fixtures[@]}"; do
  [ -n "$readme_command" ] || break
  fixture_command=$readme_command
  [ -n "$run_command" ] || fixture_command=$(gate_fixture_command "$fixture" "$readme_command")
  [ "$live" = 0 ] && curl -fsS -X POST "$stub_url/__gate/reset" -d '{}' >/dev/null
  gate_run_fixture "$fixture" "$fixture_command" "$work/repos/$fixture"
  if [ "$live" = 0 ]; then
    edits=$(curl -fsS "$stub_url/__gate/stats" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>process.stdout.write(JSON.parse(s).edits.map(e=>e.fixture).join(",")))')
    gate_check "the model fixed $fixture once" "the stub's edits: ${edits:-none}" -- test "$edits" = "$fixture"
  fi
done

# 8. Negative cases.
task=$(printf '%s\n' "$readme_command" | sed -n 's/.*--task "\([^"]*\)".*/\1/p')
[ -n "$task" ] || task="Paging skips the last item when the total is a multiple of the page size"
if [ "$live" = 0 ]; then
  # A check whose program is absent: refused in under 15 s, no model call.
  missing="$work/repos/missing-check"
  rm -rf "$missing" && mkdir -p "$missing"
  printf '{ "name": "missing-check", "version": "0.1.0", "private": true, "scripts": { "test": "axocoatl-gate-absent-check-program --run" } }\n' >"$missing/package.json"
  printf 'exports.answer = 42;\n' >"$missing/index.js"
  git -C "$missing" init -q -b main
  git -C "$missing" -c user.name='New User' -c user.email='new-user@example.invalid' add -A
  git -C "$missing" -c user.name='New User' -c user.email='new-user@example.invalid' commit -qm start
  curl -fsS -X POST "$stub_url/__gate/reset" -d '{}' >/dev/null
  t0=$(date +%s)
  gate_step "a check program that is absent: exit 3" --expect 3 -- \
    /bin/bash -c 'cd "$1" && axocoatl run fix --task "$2"' gate-run "$missing" "$task"
  elapsed=$(($(date +%s) - t0))
  gate_check "refused within 15 s" "took ${elapsed}s" -- test "$elapsed" -lt 15
  calls=$(stub_stat model-calls)
  gate_check "refused before any model call (0 tokens)" "$calls model calls" -- test "$calls" = 0

  # A reviewer that cannot review: exit 3 and no Session.
  repo="$work/repos/anthropic-reviewer"
  gate_fixture_repo node-deps "$repo"
  axocoatl session list >"$work/sessions-before.txt" 2>&1
  before_containers=$(podman ps -a --format '{{.Names}}' 2>/dev/null | grep -c . || true)
  curl -fsS -X POST "$stub_url/__gate/reset" -d '{}' >/dev/null
  gate_step "reviewer anthropic:x: exit 3" --expect 3 -- /bin/bash -c \
    'cd "$1" && axocoatl run fix --task "$2" --model "writer=$3" --model reviewer=anthropic:x' \
    gate-run "$repo" "$task" "$writer_model"
  axocoatl session list >"$work/sessions-after.txt" 2>&1
  after_containers=$(podman ps -a --format '{{.Names}}' 2>/dev/null | grep -c . || true)
  if cmp -s "$work/sessions-before.txt" "$work/sessions-after.txt" && [ "$before_containers" = "$after_containers" ] \
    && [ "$(stub_stat model-calls)" = 0 ]; then
    gate_record "no Session, container or model call for the refused run" pass '' '' '' ''
  else
    gate_record "no Session, container or model call for the refused run" fail '' '' '' \
      "sessions changed: $(diff "$work/sessions-before.txt" "$work/sessions-after.txt" | head -n 3 | tr '\n' ' '); containers $before_containers -> $after_containers"
  fi

  # No Podman: onboarding names the install command and stops with exit 1.
  nopodman_home="$work/home-no-podman"
  mkdir -p "$nopodman_home"
  nopodman_path=$(gate_path_without podman "$work/no-podman-bin")
  : >"$work/guard.log"
  sudo_mark=$(gate_sudo_mark)
  gate_step "onboard without Podman: exit 1" --expect 1 -- env HOME="$nopodman_home" \
    PATH="$guard:$nopodman_path" axocoatl onboard "${onboard_flags[@]}" </dev/null
  log="$GATE_ATTACH/logs/$(gate_slug "onboard without Podman: exit 1").log"
  gate_check "onboard prints the Podman install command" "no Podman install command in its output" -- \
    grep -Eq 'apt-get install -y podman|dnf install -y podman|install podman' "$log"
  gate_check "onboard did not run sudo without --allow-sudo (PATH)" "$(head -n 2 "$work/guard.log" | tr '\n' ' ')" -- \
    test ! -s "$work/guard.log"
  gate_sudo_check "onboard did not run sudo without --allow-sudo (system journal)" "$sudo_mark"

  # No subordinate user IDs: doctor fails with the usermod fix. The case
  # runs only when both files are backed up, and puts them back after.
  if [ "$system_changes" = 1 ] && sudo -n true 2>/dev/null; then
    backup="$work/subuid-backup"
    mkdir -p "$backup"
    if sudo -n cp /etc/subuid "$backup/subuid" && sudo -n cp /etc/subgid "$backup/subgid" \
      && cmp -s /etc/subuid "$backup/subuid" && cmp -s /etc/subgid "$backup/subgid"; then
      subuid_backup=$backup
      user=$(id -un)
      if sudo -n sed -i "/^$user:/d;/^$(id -u):/d" /etc/subuid /etc/subgid; then
        # A new user's onboarding names the fix and stops...
        subuid_home="$work/home-no-subuid"
        mkdir -p "$subuid_home"
        : >"$work/guard.log"
        sudo_mark=$(gate_sudo_mark)
        gate_step "onboard without a subuid range: exit 1" --expect 1 -- env HOME="$subuid_home" \
          PATH="$guard:$PATH" axocoatl onboard "${onboard_flags[@]}" </dev/null
        gate_check "onboard names the usermod --add-subuids fix" "its output does not name usermod --add-subuids" -- \
          grep -q 'usermod --add-subuids' "$GATE_ATTACH/logs/$(gate_slug "onboard without a subuid range: exit 1").log"
        gate_check "onboard did not run sudo for the subuid range (PATH)" "$(head -n 2 "$work/guard.log" | tr '\n' ' ')" -- \
          test ! -s "$work/guard.log"
        gate_sudo_check "onboard did not run sudo for the subuid range (system journal)" "$sudo_mark"
        # ...and doctor, on the configured user from step 4, fails the sandbox.
        gate_doctor_json "doctor --json without a subuid range" 1 "$GATE_ATTACH/doctor-no-subuid.json"
        if node -e '
            const doc = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
            const items = Object.values(doc.sections || {}).flat();
            process.exit(doc.ready === false && items.some((i) => i.status === "fail" && /usermod --add-subuids/.test(i.fix || "")) ? 0 : 1);
          ' "$GATE_ATTACH/doctor-no-subuid.json" 2>/dev/null; then
          gate_record "doctor FAILs the subuid range with the usermod fix" pass '' '' '' ''
        else
          gate_record "doctor FAILs the subuid range with the usermod fix" fail '' '' '' "no failing item whose fix runs usermod --add-subuids"
        fi
      else
        gate_record "remove this user's subuid range for the case" fail '' '' '' "sudo sed on /etc/subuid and /etc/subgid failed"
      fi
      restore_subuid || gate_record "restore /etc/subuid and /etc/subgid" fail '' '' '' "the originals are kept in $backup"
    else
      gate_record "back up /etc/subuid and /etc/subgid" fail '' '' '' "they could not be copied, so the case did not run and nothing was changed"
    fi
  else
    gate_skip "doctor FAILs an empty /etc/subuid with the usermod fix" "needs --system-changes and passwordless sudo (CI)"
  fi
else
  gate_skip "negative cases with the scripted model server" "a live run (Tier D) has no stub" optional
fi

# 9. The copy-paste lint, when the tree has it.
if [ -f "$gate_repo/scripts/check-copy-paste.mjs" ]; then
  gate_step "copy-paste lint (scripts/check-copy-paste.mjs)" -- node "$gate_repo/scripts/check-copy-paste.mjs"
else
  gate_skip "copy-paste lint" "scripts/check-copy-paste.mjs is not in this tree" optional
fi

# 10. Ctrl+C stops the daemon cleanly.
if [ -n "$dev_pid" ] && kill -0 "$dev_pid" 2>/dev/null; then
  kill -INT "$dev_pid"
  wait "$dev_pid"
  dev_status=$?
  dev_pid=''
  gate_check "Ctrl+C stops axocoatl dev with exit 0" "exit $dev_status" optional -- test "$dev_status" = 0
fi

finish
