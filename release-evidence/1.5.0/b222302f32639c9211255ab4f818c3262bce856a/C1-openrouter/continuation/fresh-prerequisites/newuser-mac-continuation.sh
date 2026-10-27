#!/bin/bash
# Clean-machine new-user gate, macOS (docs/RELEASE_GATE.md, Tier C).
#
# Run by a person, in the gate account (GATE_ACCOUNT, default axo-newuser)
# right after reset-mac-account.sh, on the release candidate: it reads the
# staged site's commands and runs them as published: the install command
# exactly as written, then (after checking the site shows them)
# `axocoatl onboard` with every consent prompt answered live,
# `axocoatl doctor` and `axocoatl dev`, then the site's and README's run
# commands. Before the install it records that the account has no Axocoatl,
# Podman or Ollama, and after each setup what onboarding installed, created
# and signed in to, so the evidence shows each thing the public text says.
# Onboarding gets --start none (except in the service path, whose Step 6
# chooses the Always-On Service): the gate starts the daemon itself, so the
# sign-in link never reaches the transcript. The whole session is recorded
# with script(1), flushed as it goes; each step's wall time and download
# are measured, and each question and answer is also kept in the path's
# answers.txt. Evidence goes to
# <evidence-root>/<version>/<commit>/<machine>-<path>.json and its folder.
#
# Usage:
#   newuser-mac.sh --release-dir DIR --site DIR [options]
#     --release-dir DIR   the candidate's axocoatl-vX.Y.Z-<mac target>.tar.gz
#                         (and .sha256): what the installer will download
#     --site DIR          the staged marketing site (index.html, install.sh, ...)
#     --machine C1|C2     C1: the main gate Mac; C2: the 16 GB Mac (default C1)
#     --paths LIST        comma-separated, default for C1:
#                         local,claude-code,codex,openrouter,service,workbench,
#                         workbench-chromium,recovery,real-repo;
#                         default for C2: local,openrouter
#     --resume            continue service, workbench, workbench-chromium,
#                         recovery or real-repo with the installed candidate;
#                         verifies its bytes and version against the archive,
#                         never counts as a clean install or a fresh sign-in
#     --commit SHA        commit the candidate was built from (default HEAD)
#     --evidence-root DIR default <repo>/release-evidence
#     --pr-repo OWNER/axocoatl-gate-NAME  throwaway repository for --keep pr
#     --real-repo DIR --real-task TEXT    the real-repo path on this repository
#                         instead of the pinned ones in real-repos.json
#     --workbench-driver  answer the Safari workbench path's first eight
#                         questions with safari-workbench.mjs through
#                         safaridriver (recorded as the driver's answers, with
#                         its prompt); the Preview and cookie questions after
#                         them are always the person's, in Safari itself
#     --yes               answer onboarding with flags instead of prompts (for
#                         trying the gate: it skips the steps that show a
#                         person answered onboarding and signed in live, so
#                         local, claude-code, codex, openrouter and service
#                         cannot pass; workbench, recovery and real-repo do
#                         not onboard and are not changed by it)
#
# Scripts handed to `sh -c` and `bash -c` take their values as positional
# parameters.
# shellcheck disable=SC2016
set -u

script_dir='/Users/erick/.codex/worktrees/release-1-5-recovery/axocoatl/scripts/release-gate'
# shellcheck source=scripts/release-gate/lib.sh
. "$script_dir/lib.sh"

usage() {
  awk 'NR == 1 { next } /^#/ { if ($0 ~ /shellcheck|positional param|take their values|read their values|^# parameters\.$/) next; sub(/^# ?/, ""); print; next } { exit }' "$0"
  exit 2
}

account=${GATE_ACCOUNT:-axo-newuser}
release_dir='' site='' machine=C1 paths='' commit='' evidence_root="$gate_repo/release-evidence"
pr_repo='' real_repo='' real_task='' assume_yes=0 workbench_driver=0 resume=0 resume_binary='' resume_path='' resume_daemon_checked=0
original_args=("$@")
while [ $# -gt 0 ]; do
  case "$1" in
    --release-dir) release_dir=${2:?}; shift ;;
    --site) site=${2:?}; shift ;;
    --machine) machine=${2:?}; shift ;;
    --paths) paths=${2:?}; shift ;;
    --commit) commit=${2:?}; shift ;;
    --evidence-root) evidence_root=${2:?}; shift ;;
    --pr-repo) pr_repo=${2:?}; shift ;;
    --real-repo) real_repo=${2:?}; shift ;;
    --real-task) real_task=${2:?}; shift ;;
    --workbench-driver) workbench_driver=1 ;;
    --resume) resume=1 ;;
    --yes) assume_yes=1 ;;
    -h | --help) usage ;;
    *) gate_die "unknown argument: $1 (see --help)" ;;
  esac
  shift
done

[ "$(uname -s)" = Darwin ] || gate_die "this is the macOS gate; on Linux use newuser-linux.sh"
[ "$(id -u)" != 0 ] || gate_die "run the gate as the gate account, not root"
[ "$(id -un)" = "$account" ] || gate_die "run this in the gate account '$account' (this is '$(id -un)'); see docs/RELEASE_GATE.md"
case "$machine" in C1 | C2) ;; *) gate_die "--machine is C1 or C2" ;; esac
[ -n "$release_dir" ] && [ -d "$release_dir" ] || gate_die "--release-dir must be the candidate's release directory"
[ -n "$site" ] && [ -f "$site/index.html" ] && [ -f "$site/install.sh" ] || gate_die "--site must be the staged marketing site (index.html, install.sh)"
command -v node >/dev/null 2>&1 || gate_die "node is required"
# ensure_daemon changes directory; every evidence path must stay anchored
# to the invocation directory, even when the output root does not exist yet.
evidence_root=$(node -e 'process.stdout.write(require("path").resolve(process.argv[1]))' "$evidence_root") \
  || gate_die 'cannot resolve --evidence-root'
release_dir=$(CDPATH='' cd -- "$release_dir" && pwd)
site=$(CDPATH='' cd -- "$site" && pwd)
case "$(uname -m)" in
  arm64) target=aarch64-apple-darwin ;;
  *) target=x86_64-apple-darwin ;;
esac
tarball=$(find "$release_dir" -maxdepth 1 -name "axocoatl-v*-$target.tar.gz" | head -n 1)
[ -n "$tarball" ] || gate_die "no axocoatl-v*-$target.tar.gz in $release_dir"
version=${tarball##*/axocoatl-v}
version=${version%%-*}
[ -n "$commit" ] || commit=$(git -C "$gate_repo" rev-parse HEAD)
gate_valid_commit "$commit" || gate_die "--commit must be a full 40-character commit SHA (lowercase hex)"
gate_valid_version "$version" || gate_die "no X.Y.Z version in the tarball name ${tarball##*/}"
# The staged site's own commands, read once and run as written.
site_page=$site/index.html
install_command=$(node "$gate_commands_js" install "$site_page" 2>/dev/null \
  || node "$gate_commands_js" install "$site/install/index.html" 2>/dev/null) || install_command=''
if [ -z "$paths" ]; then
  if [ "$machine" = C1 ]; then
    paths=local,claude-code,codex,openrouter,service,workbench,workbench-chromium,recovery,real-repo
  else
    paths=local,openrouter
  fi
fi

# A continuation cannot supply the clean-account or fresh-sign-in paths.
# Validate before recording or changing PATH, including an omitted --paths
# (whose defaults contain local).
if [ "$resume" = 1 ]; then
  case ",$paths," in
    *,,*) gate_die "--resume requires an explicit, nonempty --paths list" ;;
  esac
  old_ifs=$IFS
  IFS=,
  for path_name in $paths; do
    case "$path_name" in
      service | workbench | workbench-webkit | workbench-chromium | recovery | real-repo) ;;
      *) gate_die "--resume cannot run '$path_name'; choose service, workbench, workbench-chromium, recovery or real-repo" ;;
    esac
  done
  IFS=$old_ifs
fi

work=${AXO_CONTINUATION_WORK:?launch through the verified continuation launcher}
mkdir -p "$work"
if [ -z "${GATE_IN_SCRIPT:-}" ]; then
  transcript="$work/transcript-$(date -u +%Y%m%dT%H%M%SZ).txt"
  echo "Recording this session to $transcript"
  # -t 0: written to the file as it happens, so a copy taken at the end of
  # a path holds every question and answer.
  GATE_IN_SCRIPT=$transcript exec script -q -t 0 "$transcript" /bin/bash "$0" ${original_args[@]+"${original_args[@]}"}
fi
transcript=$GATE_IN_SCRIPT

# Resolve the existing executable before hiding it. A resumed account may
# keep Node beside axocoatl, so mirror the other tools instead of removing
# their entire directory. Do not expose axocoatl until it is verified.
if [ "$resume" = 1 ]; then
  resume_binary=$(type -P axocoatl 2>/dev/null) || resume_binary=''
  if [ -n "$resume_binary" ]; then
    resume_binary=$(CDPATH='' cd -- "$(dirname -- "$resume_binary")" && pwd)/${resume_binary##*/}
  fi
  resume_path=$(mktemp -d "${TMPDIR:-/tmp}/axo-gate-resume-path.XXXXXX") || gate_die 'cannot prepare the resume PATH'
  GATE_BASE_PATH=$(gate_path_without axocoatl "$resume_path")
else
  GATE_BASE_PATH=$PATH # wrapper verified the already installed candidate; keep launch-scoped opener shim
fi
export GATE_BASE_PATH PATH="$GATE_BASE_PATH"
models_cached=no
grep -q 'models_cached=1' "$HOME/.axocoatl-gate-reset" 2>/dev/null && models_cached=yes
task="Paging skips the last item when the total is a multiple of the page size"
preview_port=5173
dev_pid='' driver_pid='' pages_pid=''
podman_before='' ollama_before='' had_secret=''
onboarding_interrupted=0
# shellcheck disable=SC2329 # the EXIT trap runs it
cleanup() {
  [ -n "$dev_pid" ] && kill -INT "$dev_pid" 2>/dev/null
  [ -n "$driver_pid" ] && kill "$driver_pid" 2>/dev/null
  [ -n "$pages_pid" ] && kill "$pages_pid" 2>/dev/null
  [ -n "$resume_path" ] && rm -rf -- "$resume_path"
}
trap cleanup EXIT

# The operator's answers (gate_ask, gate_confirm in lib.sh).
ask() { gate_ask "$@"; }
confirm() { gate_confirm "$@"; }

config_file() { gate_config_path; }
daemon_port() {
  local port
  port=$(gate_server_port "$(config_file)")
  printf '%s\n' "${port:-8080}"
}

# podman_bin: the Podman a new user's onboarding installed (Homebrew's, or
# the podman.io package's), whatever PATH says.
podman_bin() {
  local candidate
  for candidate in "$(command -v podman 2>/dev/null)" /opt/homebrew/bin/podman /usr/local/bin/podman /opt/podman/bin/podman; do
    [ -n "$candidate" ] && [ -x "$candidate" ] && { printf '%s\n' "$candidate"; return 0; }
  done
  return 1
}

brew_bin() {
  local candidate
  for candidate in /opt/homebrew/bin/brew /usr/local/bin/brew; do
    [ -x "$candidate" ] && { printf '%s\n' "$candidate"; return 0; }
  done
  return 1
}

# dev_running: the `axocoatl dev` this gate started still runs (one that
# exited, for example because onboarding stopped it, is a zombie until the
# gate waits for it, and kill -0 still finds that).
dev_running() {
  [ -n "$dev_pid" ] && kill -0 "$dev_pid" 2>/dev/null && ! ps -o stat= -p "$dev_pid" 2>/dev/null | grep -q '^Z'
}

# stop_own_dev: stop the `axocoatl dev` this gate started (Ctrl+C, as a
# person stops it), waiting up to three minutes while it ends its
# Sessions. Sets DEV_STATUS to its exit status.
stop_own_dev() {
  local waited=0
  [ -n "$dev_pid" ] || return 0
  kill -INT "$dev_pid" 2>/dev/null
  while dev_running && [ "$waited" -lt 180 ]; do
    sleep 1
    waited=$((waited + 1))
  done
  dev_running && kill -TERM "$dev_pid" 2>/dev/null
  wait "$dev_pid"
  DEV_STATUS=$?
  dev_pid=''
}

# ensure_daemon: a daemon that runs the current configuration on its port:
# one already running (the service, or the dev this gate started), or a new
# `axocoatl dev`. A daemon that started before the configuration last
# changed, and anything on the port the gate cannot tell is an Axocoatl
# daemon, is restarted when it is the dev this gate started, as a person
# would after onboarding changed the configuration, and is a failure
# otherwise (gate_daemon_state never takes a failed request for a daemon).
ensure_daemon() {
  local port state
  port=$(daemon_port)
  GATE_DAEMON_PORT=$port
  export GATE_DAEMON_PORT
  state=$(gate_daemon_state "$port" "$(config_file)")
  case "$state" in
    current)
      gate_record "the daemon answers on port $port" pass '' '' '' "$(gate_daemon_state_says "$state" "$port") (the service or dev)"
      return 0
      ;;
    none) ;;
    *)
      if ! dev_running; then
        gate_record "the daemon on port $port runs the current configuration" fail '' '' '' \
          "$(gate_daemon_state_says "$state" "$port"), and it is not the axocoatl dev this gate started"
        return 1
      fi
      stop_own_dev
      gate_record "restart axocoatl dev ($state)" pass '' '' '' "$(gate_daemon_state_says "$state" "$port")" optional
      ;;
  esac
  cd "$work" || return 1
  axocoatl dev >"$GATE_ATTACH/logs/dev.log" 2>&1 &
  dev_pid=$!
  local deadline=$((SECONDS + 600))
  while [ "$SECONDS" -lt "$deadline" ]; do
    state=$(gate_daemon_state "$port" "$(config_file)")
    [ "$state" = current ] && break
    dev_running || break
    sleep 1
  done
  gate_check "axocoatl dev answers on port $port with the current configuration" \
    "$(gate_daemon_state_says "$state" "$port"); $(tail -n 3 "$GATE_ATTACH/logs/dev.log" 2>/dev/null | tr '\n' ' ')" -- test "$state" = current
}

stop_daemon() {
  [ -n "$dev_pid" ] || return 0
  stop_own_dev
  gate_check "Ctrl+C stops axocoatl dev with exit 0" "exit $DEV_STATUS" -- test "$DEV_STATUS" = 0
}

doctor_ready() {
  gate_doctor_json "doctor --json says ready" 0 "$GATE_ATTACH/doctor.json"
  gate_check "doctor: ready, no FAIL" "doctor --json is not ready or has a FAIL item" -- node -e '
    const doc = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
    const failing = Object.values(doc.sections || {}).flat().filter((i) => i.status === "fail");
    process.exit(doc.ready === true && failing.length === 0 ? 0 : 1);
  ' "$GATE_ATTACH/doctor.json"
  # As a new user reads it: the command the site shows, as written.
  gate_step "axocoatl doctor (as the site shows it) exits 0" -- axocoatl doctor
  gate_doctor_text_check "$GATE_ATTACH/logs/$(gate_slug "axocoatl doctor (as the site shows it) exits 0").log" "${1:-fix}"
}

path_begin() {
  if [ "$resume" = 1 ] && { [ -e "$evidence_root/$version/$commit/$machine-$1.json" ] || [ -e "$evidence_root/$version/$commit/$machine-$1" ]; }; then
    gate_die "--resume will not overwrite existing $machine-$1 evidence; choose a new --evidence-root"
  fi
  gate_begin "$evidence_root" "$version" "$commit" "$machine-$1" C
  gate_host_json "$GATE_TMP/host.json"
  if [ "${GATE_SIMULATION:-}" = 1 ]; then
    gate_record "simulation: a stand-in axocoatl and a sandboxed installer" fail '' '' '' 'test scaffolding, never release evidence'
  fi
  touch "$GATE_TMP/began"
  gate_record "free disk at the start" pass '' '' '' "$(($(gate_free_kb "$HOME") / 1024)) MiB free; models cached: $models_cached" optional
  printf '\n===== %s-%s (%s at %s) =====\n' "$machine" "$1" "$version" "${commit:0:12}"
}

path_end() {
  gate_record "free disk at the end" pass '' '' '' "$(($(gate_free_kb "$HOME") / 1024)) MiB free" optional
  GATE_AXOCOATL_VERSION=$(axocoatl --version 2>/dev/null)
  GATE_OLLAMA_VERSION=$(gate_curl -fsS http://127.0.0.1:11434/api/version 2>/dev/null | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{process.stdout.write(JSON.parse(s).version)}catch{}})')
  export GATE_AXOCOATL_VERSION GATE_OLLAMA_VERSION
  gate_tools_json "$GATE_TMP/tools.json"
  gate_keep_onboarding_logs "$GATE_TMP/began"
  cp "$transcript" "$GATE_ATTACH/transcript.txt" 2>/dev/null
  gate_finish --json host="$GATE_TMP/host.json" --json tools="$GATE_TMP/tools.json"
}

# on_login_path PROGRAM LOGIN_PATH: where a new login shell finds PROGRAM.
on_login_path() {
  local dir old_ifs=$IFS
  IFS=:
  for dir in $2; do
    [ -n "$dir" ] && [ -x "$dir/$1" ] && { IFS=$old_ifs; printf '%s\n' "$dir/$1"; return 0; }
  done
  IFS=$old_ifs
  return 1
}

# found_any PATH...: the ones that exist, joined.
found_any() {
  local path out=''
  for path in "$@"; do
    [ -n "$path" ] && { [ -e "$path" ] || [ -L "$path" ]; } && out="${out:+$out; }$path"
  done
  printf '%s\n' "$out"
}

# fresh_account: the account as a new Mac user has it, recorded before the
# install (docs/RELEASE_GATE.md, Tier C): no Axocoatl, its data or stored
# secrets; no Podman or Podman machine; no Ollama or its settings (with
# --keep-models, ~/.ollama's downloaded models may stay). Sets
# podman_before and ollama_before.
fresh_account() {
  local login_path found
  login_path=$(env -i HOME="$HOME" USER="$(id -un)" TERM=dumb PATH=/usr/bin:/bin:/usr/sbin:/sbin \
    /bin/zsh -ilc 'printf "%s\n" "$PATH"' </dev/null 2>/dev/null | tail -n 1)
  found=$(found_any "$HOME/Library/Application Support/Axocoatl" "$HOME/.axocoatl" "$HOME/.config/axocoatl" \
    "$HOME/.local/share/axocoatl" "$HOME"/Library/LaunchAgents/ai.axocoatl.*.plist)
  gate_check "the account started without Axocoatl, its data or stored secrets" "found: $found" -- test -z "$found"
  found=$(found_any /opt/homebrew/bin/podman /usr/local/bin/podman /opt/podman/bin/podman \
    "$(on_login_path podman "$login_path")" "$HOME/.config/containers/podman/machine" \
    "$HOME/.local/share/containers/podman/machine")
  if [ -z "$found" ]; then
    podman_before=absent
    gate_record "the account started without Podman or a Podman machine" pass '' '' '' 'no podman where Homebrew or podman.io install it, none on the login PATH, no machine'
  else
    podman_before="present: $found"
    gate_record "the account started without Podman or a Podman machine" fail '' '' '' "found: $found"
  fi
  found=$(found_any /opt/homebrew/bin/ollama /usr/local/bin/ollama /Applications/Ollama.app "$HOME/Applications/Ollama.app" \
    "$(on_login_path ollama "$login_path")" "$HOME/.ollama/server.json")
  gate_curl -fsS http://127.0.0.1:11434/api/version >/dev/null 2>&1 && found="${found:+$found; }something answers on 127.0.0.1:11434"
  if [ -z "$found" ]; then
    ollama_before=absent
    gate_record "the account started without Ollama or its settings" pass '' '' '' \
      "no ollama program or app, no ~/.ollama/server.json, nothing on port 11434; downloaded models kept: $([ -d "$HOME/.ollama/models" ] && echo yes || echo no)"
  else
    ollama_before="present: $found"
    gate_record "the account started without Ollama or its settings" fail '' '' '' "found: $found"
  fi
}

# resume_install: verify the archive before reading its binary, then verify
# the installed executable before invoking it. Stream the one archive member;
# nothing is installed or extracted onto the host. Recheck for each path so
# a later path cannot silently use a replaced executable or archive.
resume_install() {
  local name="resume uses the checksum-verified installed candidate"
  local expected actual candidate installed reported state port
  PATH=$GATE_BASE_PATH
  export PATH
  if [ -z "$resume_binary" ] || [ ! -x "$resume_binary" ]; then
    gate_record "$name" fail '' '' '' '--resume found no executable axocoatl on the original PATH'
    return 1
  fi
  expected=$(awk 'NR == 1 { print $1 }' "$tarball.sha256" 2>/dev/null)
  actual=$(gate_sha256 "$tarball")
  if ! printf '%s\n' "$expected" | grep -Eq '^[0-9a-f]{64}$' || [ "$actual" != "$expected" ]; then
    gate_record "$name" fail '' '' '' "archive checksum mismatch: expected ${expected:-none}, got $actual"
    return 1
  fi
  if ! candidate=$(set -o pipefail; tar -xzOf "$tarball" axocoatl | gate_sha256 /dev/stdin); then
    gate_record "$name" fail '' '' '' 'cannot read the candidate archive binary'
    return 1
  fi
  installed=$(gate_sha256 "$resume_binary")
  if [ "$installed" != "$candidate" ]; then
    gate_record "$name" fail '' '' '' "installed binary checksum mismatch: expected $candidate, got $installed ($resume_binary)"
    return 1
  fi
  if ! reported=$("$resume_binary" --version 2>&1) || [ "$reported" != "axocoatl $version" ]; then
    gate_record "$name" fail '' '' '' "installed candidate version mismatch: expected axocoatl $version, got $reported"
    return 1
  fi
  # A still-running daemon may have loaded an older binary of the same
  # version before the installed file was replaced. Its health cannot prove
  # candidate identity. Never stop that unowned process or reuse it as proof.
  if [ "$resume_daemon_checked" = 0 ]; then
    port=$(daemon_port)
    state=$(gate_daemon_state "$port" "$(config_file)")
    if [ "$state" != none ]; then
      gate_record "$name" fail '' '' '' "resume found an unowned daemon or listener on port $port ($state); stop it explicitly before resuming so this invocation starts the verified candidate"
      return 1
    fi
    resume_daemon_checked=1
  fi
  GATE_INSTALLED=$resume_binary
  PATH="${resume_binary%/*}:$GATE_BASE_PATH"
  export GATE_INSTALLED PATH
  node -e '
    const [archive, archive_sha256, binary, binary_sha256, version] = process.argv.slice(1);
    console.log(JSON.stringify({ mode: "resume", clean_install: false, archive, archive_sha256, binary, binary_sha256, version }, null, 2));
  ' "$tarball" "$actual" "$resume_binary" "$installed" "$reported" >"$GATE_ATTACH/resume-candidate.json"
  gate_record "$name" pass '' '' '' "$resume_binary; $reported; binary sha256 $installed; archive sha256 $actual; continued existing installation, no clean-install claim"
}

install_once() {
  if [ "$resume" = 1 ]; then
    resume_install
    return $?
  fi
  if command -v axocoatl >/dev/null 2>&1; then
    gate_record "axocoatl is installed" pass '' '' '' "$(command -v axocoatl)"
    return 0
  fi
  fresh_account
  # A new user's Mac has no axocoatl at all. Another account's install in a
  # shared directory such as /usr/local/bin would shadow this one.
  local other
  other=$(env -i HOME="$HOME" USER="$(id -un)" TERM=dumb PATH=/usr/bin:/bin:/usr/sbin:/sbin \
    /bin/zsh -ilc 'command -v axocoatl' </dev/null 2>/dev/null | tail -n 1)
  gate_check "no axocoatl on this account's PATH before the install" \
    "$other is on this account's login PATH (another install); move it aside for the gate" -- test -z "$other"
  if [ -z "$install_command" ]; then
    gate_record "the staged site shows the install command" fail '' '' '' "no \`curl … install.sh | sh\` in $site_page or install/index.html"
    return 1
  fi
  # The rest of the site's sequence, which the gate runs (onboard with
  # --start none; doctor and dev as written).
  gate_check "the staged site shows axocoatl onboard, doctor and dev" \
    "$(node "$gate_commands_js" has "$site_page" 'axocoatl onboard' 'axocoatl doctor' 'axocoatl dev' 2>&1 | tr '\n' ' ')" -- \
    node "$gate_commands_js" has "$site_page" 'axocoatl onboard' 'axocoatl doctor' 'axocoatl dev'
  local stage="$work/release"
  rm -rf "$stage"
  gate_stage_release "$tarball" "$stage" >/dev/null
  gate_install_new_user "$stage" "$site" "$install_command"
}

# Preserve a failed prerequisite without letting bookkeeping mask it.
# An interrupted onboarding ends the harness after its path evidence is saved;
# other failures stop only that path, so independent paths can still run.
onboarding_step() {
  local result
  gate_step "$@"
  result=$?
  if [ "$result" != 0 ] && [ "$GATE_STEP_EXIT" = 130 ]; then
    onboarding_interrupted=1
  fi
  return "$result"
}

# onboard NAME FLAG... : interactive unless --yes was given, either way
# with --start none: Step 6's default would run `axocoatl dev` in the
# foreground (blocking the gate, and printing the sign-in link into the
# transcript); the gate starts the daemon itself. When onboarding finds
# the gate's own `axocoatl dev`, started before this onboarding changes the
# configuration, it asks to stop it; the person answers yes (Enter) and
# ensure_daemon starts it again afterwards. Records whether a person
# answered it live. ONBOARD_FROM is the transcript's size before it, so
# what it printed can be read back.
onboard() {
  local name=$1
  shift
  ONBOARD_FROM=$(wc -c <"$transcript" 2>/dev/null || echo 0)
  if [ "$assume_yes" = 1 ]; then
    onboarding_step "$name (--yes)" -- axocoatl onboard --yes "$@" --start none || return
    gate_skip "onboarding was answered live in the terminal" "a --yes run answers onboarding with flags"
  else
    onboarding_step "$name (interactive; every answer is in the transcript)" --interactive -- axocoatl onboard "$@" --start none || return
    gate_record "onboarding was answered live in the terminal" pass '' '' '' "$name: every prompt was answered by the operator ($(id -un)); the transcript holds each"
  fi
}

# onboard_output FILE: what the last onboarding printed (from the
# transcript, or the step's log for --yes) into FILE.
onboard_output() {
  if [ "$assume_yes" = 1 ]; then
    cat "$GATE_ATTACH"/logs/onboard*.log >"$1" 2>/dev/null
  else
    tail -c +"$((ONBOARD_FROM + 1))" "$transcript" >"$1" 2>/dev/null
  fi
}

# signed_out_before NAME SECRET: record NAME as pass when Axocoatl's secret
# store does not hold SECRET yet, so this path can show its sign-in; a
# list that cannot be read fails it.
signed_out_before() {
  gate_secret_stored "$2"
  case $? in
    0)
      had_secret=1
      gate_record "$1" fail '' '' '' "the secret $2 was already stored, so this path cannot show its sign-in: reset the account (reset-mac-account.sh) and run the paths from local"
      ;;
    1)
      had_secret=0
      gate_record "$1" pass '' '' '' "axocoatl secret list names no $2"
      ;;
    *)
      had_secret=unknown
      gate_record "$1" fail '' '' '' "cannot tell whether $2 was stored: $GATE_SECRET_ERROR"
      ;;
  esac
}

# signed_in_during_onboarding NAME SECRET QUESTION: record NAME as pass
# when SECRET was not stored before this path's interactive onboarding, is
# stored now, and the operator answers yes to QUESTION (that the sign-in
# happened in the browser, as onboarding asked).
signed_in_during_onboarding() {
  local name=$1 secret=$2 question=$3 stored
  if [ "$assume_yes" = 1 ]; then
    gate_skip "$name" "a --yes run signs in with axocoatl connect before onboarding, not from it"
    return
  fi
  gate_secret_stored "$secret"
  stored=$?
  if [ "$had_secret" = 1 ]; then
    gate_record "$name" fail '' '' '' "$secret was stored before this path's onboarding"
  elif [ "$had_secret" != 0 ]; then
    gate_record "$name" fail '' '' '' "nothing showed that $secret was not stored before this path's onboarding"
  elif [ "$stored" = 2 ]; then
    gate_record "$name" fail '' '' '' "cannot tell whether onboarding stored $secret: $GATE_SECRET_ERROR"
  elif [ "$stored" != 0 ]; then
    gate_record "$name" fail '' '' '' "onboarding ended without the secret $secret stored$(sign_in_log "$secret")"
  else
    ask "$question"
    if [ "$ANSWER" = y ]; then
      gate_record "$name" pass '' '' '' "$secret was stored during the interactive onboarding, and the operator ($(id -un)) answered yes: $question"
    else
      gate_record "$name" fail '' '' '' "the operator ($(id -un)) answered no: ${ANSWER_NOTE:-no note}"
    fi
  fi
}

# sign_in_log SECRET: for openrouter, what onboarding's log says reached its
# browser sign-in's callback since the path began, as "; …" for a detail.
sign_in_log() {
  [ "$1" = openrouter ] || return 0
  local lines
  lines=$(gate_onboarding_log_lines "$GATE_TMP/began" 'OpenRouter sign-in:')
  if [ -n "$lines" ]; then
    printf "; onboarding's log: %s" "$lines"
  else
    printf "; onboarding's log has no 'OpenRouter sign-in:' line, so no connection reached the sign-in's callback"
  fi
}

run_published_on() {
  # run_published_on COMMAND LABEL FIXTURE...: 0 when every run and its
  # checks passed.
  local command=$1 label=$2 fixture status=0
  shift 2
  for fixture in "$@"; do
    gate_run_fixture "$fixture" "$command" "$work/repos/$label-$fixture" "$label-$fixture" || status=1
  done
  return "$status"
}

run_readme_on() {
  # run_readme_on README_COMMAND LABEL FIXTURE...: the README's command, or a
  # fixture's own (gate_fixture_command: node-plain, onboarding's sample).
  local command=$1 label=$2 fixture status=0
  shift 2
  for fixture in "$@"; do
    gate_run_fixture "$fixture" "$(gate_fixture_command "$fixture" "$command")" "$work/repos/$label-$fixture" "$label-$fixture" || status=1
  done
  return "$status"
}

# summary NAME STATUS WHAT: one step for a claim made of several runs.
summary() {
  if [ "$2" = 0 ]; then
    gate_record "$1" pass '' '' '' "$3"
  else
    gate_record "$1" fail '' '' '' "$3: see the failing steps above"
  fi
}

all_fixtures() { gate_fixture_names | tr '\n' ' '; }

# Homebrew lists FORMULA, which the account did not have before the install.
installed_by_onboarding() {
  local name=$1 formula=$2 brew
  brew=$(brew_bin) || { gate_record "$name" fail '' '' '' 'no Homebrew'; return; }
  if [ "$podman_before" = '' ] || [ "$ollama_before" = '' ]; then
    gate_record "$name" fail '' '' '' "this path did not record the account before the install (run the paths from local, right after a reset)"
  elif { [ "$formula" = podman ] && [ "$podman_before" != absent ]; } || { [ "$formula" = ollama ] && [ "$ollama_before" != absent ]; }; then
    gate_record "$name" fail '' '' '' "$formula was on the account before onboarding"
  elif "$brew" list --formula "$formula" >/dev/null 2>&1; then
    gate_record "$name" pass '' '' '' "$("$brew" list --versions "$formula" 2>/dev/null | head -n 1), installed by Homebrew during onboarding"
  else
    gate_record "$name" fail '' '' '' "Homebrew lists no $formula formula"
  fi
}

# local_setup_steps: what the local path's onboarding set up, as the
# public text says it.
local_setup_steps() {
  installed_by_onboarding "onboarding installed Podman with Homebrew" podman
  installed_by_onboarding "onboarding installed Ollama with Homebrew" ollama
  local podman machines
  podman=$(podman_bin) || podman=podman
  machines=$("$podman" machine list --format json 2>/dev/null)
  if [ "$podman_before" = absent ] && printf '%s' "$machines" | node -e '
      let s = ""; process.stdin.on("data", (d) => (s += d)).on("end", () => {
        try { process.exit(JSON.parse(s).some((m) => m.Running === true) ? 0 : 1); } catch { process.exit(1); }
      });'; then
    gate_record "onboarding created the Podman machine, and it runs" pass '' '' '' "$(printf '%s' "$machines" | tr -d '\n' | cut -c1-300)"
  else
    gate_record "onboarding created the Podman machine, and it runs" fail '' '' '' \
      "before: ${podman_before:-not recorded}; podman machine list: $(printf '%s' "$machines" | tr -d '\n' | cut -c1-200)"
  fi
  gate_check "Ollama's cloud models are off (doctor)" "doctor --json has no OK item saying Ollama's cloud models are disabled" -- node -e '
    const doc = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
    const items = Object.values(doc.sections || {}).flat();
    process.exit(items.some((i) => i.status === "ok" && /cloud models disabled/i.test(`${i.name} ${i.detail}`)) ? 0 : 1);
  ' "$GATE_ATTACH/doctor.json"
}

path_local() {
  path_begin local
  install_once || { path_end; return; }
  if [ "$assume_yes" = 1 ]; then
    onboard "onboard: local models" --writer ollama --podman create --pull --smoke-test || { path_end; return 1; }
  else
    onboard "onboard: local models" || { path_end; return 1; }
  fi
  if [ "$machine" = C2 ]; then
    confirm "onboarding steered a 16 GB Mac to an OpenRouter reviewer" "Did onboarding recommend an OpenRouter reviewer instead of a 33 GB local pull?"
    confirm "a context below 16k was refused with its fix" "Did onboarding or the first run refuse a model context below 16,384 tokens and name OLLAMA_CONTEXT_LENGTH?"
  else
    confirm "onboarding ended Ready with a passing first run" "Did onboarding end with 'Ready' and a passing first fix run on the sample repository?"
  fi
  doctor_ready
  local_setup_steps
  ensure_daemon
  local readme status
  readme=$(node "$gate_commands_js" readme-run "$gate_repo/README.md")
  # shellcheck disable=SC2046
  run_readme_on "$readme" readme $(all_fixtures)
  status=$?
  summary "the README's command passed on every fixture, each receipt and record verified" "$status" "fixtures: $(all_fixtures)"
  path_end
}

path_claude_code() {
  path_begin claude-code
  install_once || { path_end; return; }
  signed_out_before "no Claude Code sign-in was stored before this path" claude-code-oauth
  onboard "onboard --update --writer claude-code" --update --writer claude-code || { path_end; return 1; }
  signed_in_during_onboarding "Claude Code was signed in through the browser during onboarding" claude-code-oauth \
    "Did onboarding open Claude's sign-in page in the browser, and did you sign in there?"
  doctor_ready fix-claude-code
  ensure_daemon
  local hero status
  hero=$(node "$gate_commands_js" first "$site/index.html" "axocoatl run fix-claude-code") \
    || gate_record "the home page shows the fix-claude-code command" fail '' '' '' 'not found'
  if [ -n "$hero" ]; then
    printf '%s\n' "$hero" >"$GATE_ATTACH/hero-command.txt"
    run_published_on "$hero" hero node-deps python-pytest
    status=$?
    summary "the home page's fix-claude-code command passed on node-deps and python-pytest" "$status" "$hero"
    keep_pr "$hero"
  fi
  local repo="$work/repos/anthropic-reviewer"
  gate_fixture_repo node-deps "$repo"
  axocoatl session list >"$GATE_TMP/sessions-before.txt" 2>&1
  gate_step "reviewer anthropic:opus is refused: exit 3" --expect 3 -- /bin/bash -c \
    'cd "$1" && axocoatl run fix-claude-code --task "$2" --model reviewer=anthropic:opus' gate-run "$repo" "$task"
  axocoatl session list >"$GATE_TMP/sessions-after.txt" 2>&1
  gate_check "the refused run left no Session" "the Session list changed" -- \
    cmp -s "$GATE_TMP/sessions-before.txt" "$GATE_TMP/sessions-after.txt"
  path_end
}

# keep_pr HERO: the hero command with --keep pr on the throwaway
# repository, whose workflow is the docs' strict verify-receipt workflow
# (the one for Keep as PR branches, which keeps require-receipt at its
# default, true), pinned to the candidate commit. The Action must verify
# this run's receipt: its log must say so, naming the run.
keep_pr() {
  local hero=$1 slug=${pr_repo#*/} clone="$work/pr-repo" url run head action_run log
  if [ -z "$pr_repo" ]; then
    gate_skip "--keep pr to a throwaway repository, checked by the verify-receipt Action" "pass --pr-repo OWNER/axocoatl-gate-NAME"
    return
  fi
  case "$slug" in
    axocoatl-gate*) ;;
    *)
      gate_record "--keep pr throwaway repository" fail '' '' '' "$pr_repo: the gate force-resets the repository it is given, so its name must start with axocoatl-gate"
      return
      ;;
  esac
  gate_step "gh is signed in" -- gh auth status || return
  rm -rf "$clone"
  gate_step "clone $pr_repo" -- gh repo clone "$pr_repo" "$clone" -- -q || return
  local workflow
  workflow=$(node "$gate_commands_js" yaml-block "$gate_repo/sites/docs/src/content/docs/workbench/receipts.mdx" "startsWith(github.head_ref, 'axocoatl/')") \
    || { gate_record "the docs show the strict verify-receipt workflow" fail '' '' '' "no yaml block runs only on axocoatl/ branches"; return; }
  case "$workflow" in
    *verify-receipt@*) ;;
    *) gate_record "the docs show the strict verify-receipt workflow" fail '' '' '' 'its yaml block does not use the verify-receipt Action'; return ;;
  esac
  case "$workflow" in
    *'require-receipt: false'*) gate_record "the docs show the strict verify-receipt workflow" fail '' '' '' 'it sets require-receipt: false'; return ;;
  esac
  (
    cd "$clone" || exit 1
    git checkout -q -B main
    git rm -rq --ignore-unmatch . >/dev/null
    cp -R "$gate_repo/tests/fixtures/newuser/node-deps/." .
    mkdir -p .axocoatl .github/workflows
    axocoatl receipt key >.axocoatl/receipt-keys
    # The release is not tagged yet: point the Action at the candidate commit.
    printf '%s' "$workflow" | sed -E "s#(actions/verify-receipt@)v[0-9][0-9A-Za-z.+-]*#\\1$commit#" >.github/workflows/verify-receipt.yml
    git add -A
    git -c user.name="$(id -un)" -c user.email="$(id -un)@example.invalid" commit -qm "gate: node-deps for $version at ${commit:0:12}"
    git push -q --force origin main
  ) >"$GATE_ATTACH/logs/pr-repo-reset.log" 2>&1 \
    || { gate_record "reset $pr_repo to the node-deps fixture" fail '' '' '' "$(tail -n 2 "$GATE_ATTACH/logs/pr-repo-reset.log" | tr '\n' ' ')"; return; }
  gate_record "reset $pr_repo to the node-deps fixture" pass '' '' '' 'the docs strict verify-receipt workflow (require-receipt true), pinned to the candidate commit'
  # Keep commits only the paths the run's Agents changed (keep_pr.rs), so
  # the record, receipt and report the gate asks for stay out of the pull
  # request; its answers are checked like every fixture run's.
  local command
  command=$(gate_with_outputs "$hero --keep pr")
  gate_step "hero command with --keep pr" -- /bin/bash -c "cd \"\$1\" && $command" gate-run "$clone" || return
  mkdir -p "$GATE_ATTACH/keep-pr"
  cp "$clone/run.axorecord.jsonl" "$clone/receipt.json" "$GATE_ATTACH/keep-pr/" 2>/dev/null
  gate_markup_check keep-pr "$clone" "$clone/run.axorecord.jsonl" receipt.json axocoatl-junit.xml
  log="$GATE_ATTACH/logs/$(gate_slug "hero command with --keep pr").log"
  url=$(grep -Eo 'https://github\.com/[^ ]+/pull/[0-9]+' "$log" | tail -n 1)
  run=$(sed -nE 's/^Record: (run-[0-9a-f-]+).*/\1/p' "$log" | tail -n 1)
  [ -n "$url" ] || { gate_record "--keep pr opened a pull request" fail '' '' '' 'no pull request URL in the output'; return; }
  gate_record "--keep pr opened a pull request" pass '' '' '' "$url"
  sleep 15
  gate_step "the verify-receipt workflow's checks pass on the pull request" -- gh pr checks "$url" --watch --interval 15
  # A pass alone could be a skipped job or a pull request without a receipt:
  # the Action's own log must say it verified this run's receipt.
  head=$(gh pr view "$url" --json headRefOid --jq .headRefOid 2>/dev/null)
  action_run=$(gh run list --repo "$pr_repo" --workflow verify-receipt.yml --commit "$head" --json databaseId --jq '.[0].databaseId' 2>/dev/null)
  if [ -n "$action_run" ] && gh run view "$action_run" --repo "$pr_repo" --log >"$GATE_ATTACH/logs/verify-receipt-action.log" 2>&1 \
    && [ -n "$run" ] && grep -qF "Verified the receipt of run $run " "$GATE_ATTACH/logs/verify-receipt-action.log"; then
    gate_record "the verify-receipt Action verified the pull request's receipt" pass '' '' '' \
      "$(grep -oF "Verified the receipt of run $run " "$GATE_ATTACH/logs/verify-receipt-action.log" | head -n 1)(Actions run $action_run)"
  else
    gate_record "the verify-receipt Action verified the pull request's receipt" fail '' '' '' \
      "the log of the Action's run ${action_run:-(none found for $head)} does not say it verified the receipt of ${run:-this run}"
  fi
}

path_codex() {
  path_begin codex
  install_once || { path_end; return; }
  signed_out_before "no Codex sign-in was stored before this path" codex-chatgpt
  onboard "onboard --update --writer codex" --update --writer codex || { path_end; return 1; }
  confirm "Codex billing was set to the ChatGPT plan" "Did onboarding say Codex runs bill to your ChatGPT plan (codex-chatgpt), without offering an OpenAI API key?"
  signed_in_during_onboarding "Codex was signed in with ChatGPT in the browser during onboarding" codex-chatgpt \
    "Did onboarding open the ChatGPT sign-in page in the browser, and did you sign in there?"
  doctor_ready fix-codex
  ensure_daemon
  local command status
  command=$(node "$gate_commands_js" first "$site/index.html" "axocoatl run fix-codex" 2>/dev/null \
    || node "$gate_commands_js" first "$site/install/index.html" "axocoatl run fix-codex")
  if [ -z "$command" ]; then
    gate_record "the site shows a fix-codex command" fail '' '' '' 'not found'
  else
    command=$(printf '%s' "$command" | sed "s/--task \"…\"/--task \"$task\"/")
    run_published_on "$command" codex node-deps python-pytest
    status=$?
    summary "the site's fix-codex command passed on node-deps and python-pytest" "$status" "$command"
  fi
  path_end
}

path_openrouter() {
  path_begin openrouter
  install_once || { path_end; return; }
  # The OpenRouter key comes from the browser sign-in (OAuth PKCE) into the
  # secret store; nobody types, pastes or exports it. A --yes run signs in
  # first and lets onboarding use the connected key.
  signed_out_before "no OpenRouter key was stored before this path" openrouter
  if [ "$assume_yes" = 1 ]; then
    onboarding_step "connect OpenRouter in the browser (axocoatl connect openrouter)" --interactive -- axocoatl connect openrouter \
      || { path_end; return 1; }
    onboard "onboard --update --writer openrouter" --update --writer openrouter || { path_end; return 1; }
  else
    onboard "onboard --update --writer openrouter (connect in the browser when it asks)" --update --writer openrouter \
      || { path_end; return 1; }
  fi
  signed_in_during_onboarding "OpenRouter was connected through the browser sign-in during onboarding" openrouter \
    "When onboarding asked how to get the OpenRouter key, did you choose the browser sign-in and sign in to OpenRouter there (no key typed or pasted)?"
  gate_check "config.yaml holds no OpenRouter key" "$(config_file) holds an sk-or- value" -- \
    sh -c '! grep -q "sk-or-" "$1"' gate-config "$(config_file)"
  confirm "onboarding asked for different writer and reviewer models" "Did onboarding ask separately for the writer and the reviewer model, and refuse the same model for both?"
  doctor_ready
  ensure_daemon
  local status
  run_published_on "axocoatl run fix --task \"$task\"" openrouter node-deps python-pytest
  status=$?
  summary "axocoatl run fix passed on node-deps and python-pytest with the OpenRouter models" "$status" \
    "writer $(gate_run_model "$(config_file)" writer), reviewer $(gate_run_model "$(config_file)" reviewer)"
  if [ "$machine" = C1 ]; then
    local reviewer
    reviewer=$(gate_run_model "$(config_file)" reviewer)
    case "$reviewer" in
      openrouter:*)
        run_published_on "axocoatl run fix-claude-code --task \"$task\" --model reviewer=$reviewer" claude-openrouter node-deps
        status=$?
        summary "fix-claude-code with the OpenRouter reviewer passed on node-deps" "$status" "reviewer $reviewer"
        ;;
      *) gate_record "the configured reviewer is on OpenRouter" fail '' '' '' "run.models.reviewer is '${reviewer:-unset}'" ;;
    esac
  fi
  path_end
}

# path_service: the Always-On Service as a person meets it: chosen in
# onboarding's Step 6, restarted by Step 7 after `onboard --update` changes
# the configuration, a fix run through it, then `axocoatl service status`,
# `stop` and `uninstall`. The change switches the writer between the local
# models and OpenRouter, so it runs after the local and openrouter paths.
path_service() {
  path_begin service
  install_once || { path_end; return; }
  # The service takes the configured port: the gate's own dev stops first.
  stop_own_dev
  local port change status output="$GATE_TMP/onboard-output.txt"
  port=$(daemon_port)
  GATE_DAEMON_PORT=$port
  export GATE_DAEMON_PORT
  if [ "$assume_yes" = 1 ]; then
    onboarding_step "onboard --update: Step 6, the Always-On Service (--yes)" -- axocoatl onboard --yes --update --start service --no-smoke-test \
      || { path_end; return 1; }
    gate_skip "onboarding was answered live in the terminal" "a --yes run answers onboarding with flags"
  else
    printf '\nIn Step 6 ("How should Axocoatl run?"), choose "Always on, starting at login" and agree to install it.\n'
    onboarding_step "onboard --update: Step 6, the Always-On Service (interactive)" --interactive -- axocoatl onboard --update \
      || { path_end; return 1; }
    gate_record "onboarding was answered live in the terminal" pass '' '' '' "Step 6: the operator ($(id -un)) chose the Always-On Service"
  fi
  gate_service_status "onboarding installed and started the Always-On Service (Step 6)" yes yes
  local state
  state=$(gate_wait_daemon "$port" "$(config_file)" 300)
  gate_check "the service answers on port $port" "$(gate_daemon_state_says "$state" "$port")" -- test "$state" = current
  case "$(gate_run_model "$(config_file)" writer)" in
    ollama:*) change=openrouter ;;
    *) change=ollama ;;
  esac
  onboard "onboard --update --writer $change --smoke-test (the configuration changes while the service runs)" \
    --update --writer "$change" --smoke-test || { path_end; return 1; }
  onboard_output "$output"
  gate_service_restart_steps "$(config_file)" "$port" "$output"
  local readme
  readme=$(node "$gate_commands_js" readme-run "$gate_repo/README.md")
  run_readme_on "$readme" service node-deps
  status=$?
  summary "a fix through the Always-On Service passed, its receipt and record verified" "$status" "the README's command on node-deps, port $port"
  gate_service_status "axocoatl service status: installed and running" yes yes
  gate_service_end "$port"
  path_end
}

# The workbench path's questions: what a new user does, from the sign-in
# link to a commit. With --workbench-driver, safari-workbench.mjs answers
# them in Safari through safaridriver.
wb_names=(
  "the sign-in link opened the workbench"
  "Open workspace refuses the home folder"
  "Open workspace accepts the repository"
  "New session starts"
  "Team & budget is pre-filled and Apply works"
  "one turn edits a file"
  "Source Control lists the turn's change to src/paginate.js"
  "a commit from the workbench succeeds"
)

workbench_questions() {
  # workbench_questions APP REPO
  local app=$1 repo=$2
  printf '%s\n' \
    "Did the link open the workbench in $app, signed in?" \
    "Choose Open workspace… with your home folder ($HOME). Was it refused?" \
    "Now open $repo as the workspace. Was it accepted?" \
    "Create a New session with $preview_port in Exposed ports. Did it start?" \
    "Open Team & budget. Were the limits pre-filled, and did Apply succeed without editing them?" \
    "Send one turn asking to fix the paging bug in src/paginate.js. Did the turn finish and change the file?" \
    "Open Source Control. Does it show 'Last turn N' (N of at least 1: the writer may also add or change a test) listing src/paginate.js?" \
    "Commit the change from Source Control. Did it succeed?"
}

# driver_answers LINK REPO: safari-workbench.mjs answers the eight
# questions in Safari through safaridriver; each is recorded as its answer,
# with the prompt it sent. The link goes to it on standard input.
driver_answers() {
  local link=$1 repo=$2 log="$GATE_ATTACH/logs/safari-workbench.log" n prompt line result seen
  if ! gate_curl -fsS http://127.0.0.1:4444/status >/dev/null 2>&1; then
    safaridriver -p 4444 >"$GATE_ATTACH/logs/safaridriver.log" 2>&1 &
    driver_pid=$!
    sleep 2
  fi
  printf '%s\n' "$link" | node "$script_dir/safari-workbench.mjs" --backend webdriver --link - --home "$HOME" --repo "$repo" \
    --out "$GATE_ATTACH/driver" --preview-port "$preview_port" >"$log" 2>&1
  [ -n "$driver_pid" ] && kill "$driver_pid" 2>/dev/null
  driver_pid=''
  prompt=$(sed -n 's/^PROMPT: //p' "$log" | head -n 1)
  for n in 1 2 3 4 5 6 7 8; do
    line=$(grep -m 1 "^STEP $n: " "$log")
    result=${line#"STEP $n: "}
    seen=${result#* }
    case "$result" in
      PASS*) gate_record "${wb_names[$((n - 1))]}" pass '' '' '' "answered by safari-workbench.mjs (prompt: \"$prompt\"): $seen" ;;
      *) gate_record "${wb_names[$((n - 1))]}" fail '' '' '' "answered by safari-workbench.mjs (prompt: \"$prompt\"): ${seen:-no answer; see logs/safari-workbench.log}" ;;
    esac
  done
}

# session_for REPO: the id and name, tab-separated, of the open Session
# whose folder is REPO.
session_for() {
  local out="$GATE_TMP/sessions.json"
  gate_api_get /api/sessions "$out" || return 1
  node -e '
    const sessions = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
    const real = require("fs").realpathSync(process.argv[2]);
    const open = sessions.filter((s) => s.working_dir === real && s.status !== "closed");
    if (!open.length) process.exit(1);
    const last = open[open.length - 1];
    process.stdout.write(`${last.id}\t${String(last.name).replace(/\s+/g, " ")}`);
  ' "$out" "$1"
}

# planted_since N: the number of planted cookies Safari sent back in the
# last count after the first N lines of safari-pages.mjs's log, or nothing.
planted_since() {
  tail -n +"$(($1 + 1))" "$GATE_ATTACH/logs/safari-pages.log" 2>/dev/null | sed -n 's/.* planted=\([0-9]*\)$/\1/p' | tail -n 1
}

pages_lines() {
  if [ -f "$GATE_ATTACH/logs/safari-pages.log" ]; then wc -l <"$GATE_ATTACH/logs/safari-pages.log" | tr -d ' '; else echo 0; fi
}

# app_pages_since N: the requests the Preview app received for a page of its
# own (Sec-Fetch-Dest document) after the first N lines of its log.
app_pages_since() {
  tail -n +"$(($1 + 1))" "$repo/.gate-preview-requests.log" 2>/dev/null | awk '$3 == "document"' | head -n 3 | tr '\n' ' '
}

app_requests() { wc -l <"$repo/.gate-preview-requests.log" 2>/dev/null | tr -d ' ' || echo 0; }

# refused_top_level NAME QUESTION SINCE: the operator saw Axocoatl's refusal
# page, and the app received no request for a page of its own.
refused_top_level() {
  local name=$1 reached
  ask "$2"
  reached=$(app_pages_since "$3")
  if [ "$ANSWER" = y ] && [ -z "$reached" ]; then
    gate_record "$name" pass '' '' '' "the operator ($(id -un)) answered yes: $2; the app received no request for a page of its own"
  elif [ -n "$reached" ]; then
    gate_record "$name" fail '' '' '' "the app received a request for a page of its own: $reached${ANSWER_NOTE:+; operator: $ANSWER_NOTE}"
  else
    gate_record "$name" fail '' '' '' "the operator ($(id -un)) answered no: ${ANSWER_NOTE:-no note}"
  fi
}

# flood_step QUESTION SINCE: the flood step: after line SINCE of
# safari-pages.mjs's log, Safari sent at least one planted cookie back (it
# kept the flood), and the operator answers yes to QUESTION.
flood_step() {
  local name="a Domain=localhost cookie flood from another page leaves the workbench as the docs say" planted
  ask "$1"
  planted=$(planted_since "$2")
  if [ -z "$planted" ]; then
    gate_record "$name" fail '' '' '' "the plant page's count never reached its server (logs/safari-pages.log): nothing shows the flood was in place"
  elif [ "$planted" = 0 ]; then
    gate_record "$name" fail '' '' '' "Safari sent none of the 60 planted cookies back: it kept none, so the flood was not in place"
  elif [ "$ANSWER" != y ]; then
    gate_record "$name" fail '' '' '' "the operator ($(id -un)) answered no (Safari sent $planted of the 60 planted cookies back): ${ANSWER_NOTE:-no note}"
  else
    gate_record "$name" pass '' '' '' "Safari sent $planted of the 60 planted cookies back (Path=/api); the operator ($(id -un)) answered yes: $1"
  fi
}

# expired_step QUESTION SINCE: the fixture's /clear page expired only its
# named Domain=localhost cookies with Path=/api. Require a fresh clear
# navigation and a subsequent server count of 0, plus the observed result.
# This is test cleanup, not Safari Settings or arbitrary website recovery.
expired_step() {
  local name="the gate's planted localhost cookies are expired" removed
  ask "$1"
  removed=$(tail -n +"$(($2 + 1))" "$GATE_ATTACH/logs/safari-pages.log" 2>/dev/null | awk '
    /^GET \/clear\?path=\/api document$/ { cleared = 1; count = ""; next }
    cleared && /^GET \/api\/gate-cookie-count [^ ]+ planted=[0-9]+$/ {
      count = $NF; sub(/^planted=/, "", count)
    }
    END { if (cleared && count != "") print count }
  ')
  if [ -z "$removed" ]; then
    gate_record "$name" fail '' '' '' "no fresh /clear?path=/api navigation followed by its server count"
  elif [ "$removed" != 0 ]; then
    gate_record "$name" fail '' '' '' "after the fixture expired its cookies Safari still sent $removed of them back"
  elif [ "$ANSWER" != y ]; then
    gate_record "$name" fail '' '' '' "the operator ($(id -un)) answered no: ${ANSWER_NOTE:-no note}"
  else
    gate_record "$name" pass '' '' '' "the fixture expired only its 60 named Domain=localhost cookies with Path=/api and Safari sent none back; the operator ($(id -un)) answered yes: $1; this is not general website-data recovery"
  fi
}

# safari_preview_steps LINK REPO: in Safari itself, as the person uses it:
# a Preview stays in the workbench's pane (Open full preview disabled), a
# Preview address typed into a tab, reached by a redirect from another
# site, or Command-clicked inside the Preview gets Axocoatl's refusal page
# and never reaches the app, and a Domain=localhost cookie flood from
# another *.localhost page does to the workbench what the docs say, until
# the fixture expires only its planted cookies again.
safari_preview_steps() {
  local link=$1 repo=$2 port session='' session_name='' found address pages_port before host
  port=$(daemon_port)
  host=$(printf '%s' "$link" | sed -E 's#^https?://([^/:]+).*#\1#')
  cat >"$repo/gate-preview-app.cjs" <<'APP'
// The gate's Preview app (scripts/release-gate/newuser-mac.sh): it logs each
// request's method, path and Sec-Fetch-Dest next to this file.
const http = require('http');
const log = require('path').join(__dirname, '.gate-preview-requests.log');
http.createServer((req, res) => {
  require('fs').appendFileSync(log, `${req.method} ${req.url} ${req.headers['sec-fetch-dest'] || '-'}\n`);
  res.setHeader('content-type', 'text/html; charset=utf-8');
  res.end('<!doctype html><title>Axocoatl gate Preview</title><h1>Axocoatl gate Preview</h1><p><a href="/next">Open the next page</a></p>');
}).listen(5173, '0.0.0.0');
APP
  if found=$(session_for "$repo"); then
    session=${found%%$'\t'*}
    session_name=${found#*$'\t'}
  fi
  if [ "$workbench_driver" = 1 ]; then
    # The driver made the Session in Safari's automation window, which it
    # closed when it finished. Open the workbench in an ordinary Safari
    # window with the same sign-in link, and name the Session to open.
    open -a Safari "$link"
    printf '\nsafari-workbench.mjs made the Session in its own Safari window, now closed. Safari opens the workbench again in an ordinary window: there, open the workspace %s and, in it, the Session %s.\n' \
      "$(basename -- "$repo")" "${session_name:-that the driver made (the last one in the list)}"
  fi
  printf '\nIn Safari, in that Session: open its Terminal and, in the repository'"'"'s folder, run\n\n    node gate-preview-app.cjs\n\nthen open the Session'"'"'s Browser view and enter http://localhost:%s in its address bar.\n' "$preview_port"
  confirm "the Preview shows the Session's app in the workbench" "Does the Browser view show the page 'Axocoatl gate Preview'?"
  gate_check "the app received the Preview's request from the workbench's frame" "the app's log ($repo/.gate-preview-requests.log) has no iframe request" -- \
    grep -q ' iframe$' "$repo/.gate-preview-requests.log"
  confirm "Open full preview is disabled in Safari, and the workbench says why" \
    "Is 'Open full preview' greyed out, with a line under the address bar saying that in Safari Previews stay in this pane?"
  if [ -z "$session" ] && found=$(session_for "$repo"); then
    session=${found%%$'\t'*}
  fi
  if [ -z "$session" ]; then
    gate_record "a Preview address typed into a tab gets Axocoatl's refusal page" fail '' '' '' "no open Session for $repo in GET /api/sessions"
    return
  fi
  address="http://$session-p$preview_port.localhost:$port/"
  before=$(app_requests)
  refused_top_level "a Preview address typed into a tab gets Axocoatl's refusal page" \
    "Paste $address into a new Safari tab. Does Safari show Axocoatl's page 'This Preview opens only in the workbench', and not the app?" "$before"
  node "$script_dir/safari-pages.mjs" --port-file "$GATE_TMP/pages.port" --log "$GATE_ATTACH/logs/safari-pages.log" &
  pages_pid=$!
  for _ in $(seq 1 50); do [ -s "$GATE_TMP/pages.port" ] && break; sleep 0.1; done
  pages_port=$(cat "$GATE_TMP/pages.port" 2>/dev/null)
  before=$(app_requests)
  refused_top_level "a redirect from another site to a Preview gets Axocoatl's refusal page" \
    "Open http://gate-pages.localhost:$pages_port/go?to=$address in a new tab. Does Safari end on the same refusal page, and not the app?" "$before"
  before=$(app_requests)
  refused_top_level "a Command-click inside the Preview gets Axocoatl's refusal page" \
    "Back in the workbench's Browser view, hold Command and click 'Open the next page' in the Preview. Did a tab open on Axocoatl's refusal page, and not on the app?" "$before"
  # The cookie flood: from a page on another *.localhost address, as a
  # development server the person runs could set them. Its server counts
  # the planted cookies Safari sends back, so the evidence shows that the
  # flood was in place, and later that it was gone.
  before=$(pages_lines)
  printf '\nOpen http://gate-pages.localhost:%s/plant?path=/api in a new Safari tab. It sets the cookies and shows how many of them Safari sends back.\n' "$pages_port"
  if [ "$host" = localhost ]; then
    # The workbench is on localhost, where Safari sends those cookies: the
    # docs say the API then refuses this browser and the workbench says so.
    flood_step "Reload the workbench tab. Does it show 'This browser is signed out' (after loading again at most twice), and not reload without end?" "$before"
    before=$(pages_lines)
    expired_step "Close the plant tab, then open http://gate-pages.localhost:$pages_port/clear?path=/api: does it say Safari sends 0 of the planted cookies back? This page expires only the gate's test cookies. Then open the sign-in link again ('axocoatl url'). Is the workbench signed in and working?" "$before"
  else
    # The workbench is on $host, which Domain=localhost cookies never reach.
    flood_step "Reload the workbench tab (at $host). Is it still signed in, showing your Session, with no 'signed out' message?" "$before"
    before=$(pages_lines)
    expired_step "Close the plant tab, then open http://gate-pages.localhost:$pages_port/clear?path=/api: does it say Safari sends 0 of the planted cookies back? This page expires only the gate's test cookies. Is the workbench still signed in and working?" "$before"
  fi
  kill "$pages_pid" 2>/dev/null
  pages_pid=''
}

path_workbench() {
  local browser=$1 app=$2
  path_begin "workbench-$browser"
  install_once || { path_end; return; }
  ensure_daemon
  local repo="$work/repos/workbench" n
  gate_fixture_repo node-deps "$repo"
  git -C "$repo" config --unset user.name 2>/dev/null
  local link
  link=$(axocoatl url 2>/dev/null | grep -Eo 'https?://[^ ]+' | head -n 1)
  if [ -z "$link" ]; then
    gate_record "axocoatl url prints a sign-in link" fail '' '' '' 'no link'
  else
    gate_record "axocoatl url prints a sign-in link" pass '' '' '' '(not recorded: it signs in)'
  fi
  if [ "$workbench_driver" = 1 ] && [ "$browser" = webkit ] && [ -n "$link" ]; then
    driver_answers "$link" "$repo"
  else
    [ -n "$link" ] && open -a "$app" "$link"
    printf '\nIn %s: use the workbench as a new user would, on %s\n' "$app" "$repo"
    n=0
    while IFS= read -r question; do
      confirm "${wb_names[$n]}" "$question"
      n=$((n + 1))
    done < <(workbench_questions "$app" "$repo")
  fi
  local author expected
  author=$(git -C "$repo" log -1 --format='%an <%ae>' 2>/dev/null)
  expected=$(git config --global user.name 2>/dev/null)
  gate_check "the commit is authored by this account's identity" "author: ${author:-no commit}" -- \
    sh -c '[ -n "$1" ] && [ "$1" != "New User <new-user@example.invalid>" ] && { [ -z "$2" ] || case "$1" in "$2 "*) true ;; *) false ;; esac; }' gate "$author" "$expected"
  workbench_markup_check "$repo"
  if [ "$browser" = webkit ] && [ -n "$link" ]; then
    safari_preview_steps "$link" "$repo"
  fi
  stop_daemon
  path_end
}

# workbench_markup_check REPO: no accepted answer of the workbench Session on
# REPO holds tool-call markup a model wrote as text (gate_markup_check's rule,
# for the Lead's turn), read from the Session's JSON export.
workbench_markup_check() {
  local name="no tool-call markup in the workbench Session's accepted answers" found id out="$GATE_TMP/workbench-export.json" line
  if ! found=$(session_for "$1"); then
    gate_record "$name" fail '' '' '' "no open Session on $1"
    return 1
  fi
  id=${found%%$'\t'*}
  if ! gate_api_get "/api/sessions/$id/export?format=json" "$out"; then
    gate_record "$name" fail '' '' '' "cannot export Session $id: $(head -c 200 "$out" | tr '\n' ' ')"
    return 1
  fi
  if line=$(node "$gate_dir/record-answers.mjs" --export "$out" 2>&1); then
    gate_record "$name" pass '' '' '' "$line"
  else
    gate_record "$name" fail '' '' '' "$line"
    return 1
  fi
}

path_recovery() {
  path_begin recovery
  install_once || { path_end; return; }
  ensure_daemon
  local podman readme status
  podman=$(podman_bin) || podman=podman
  gate_step "podman machine stop" -- "$podman" machine stop
  gate_check "the Podman machine is stopped before the run" "it still runs" -- \
    sh -c '! "$1" machine inspect --format "{{.State}}" | grep -qi running' gate "$podman"
  readme=$(node "$gate_commands_js" readme-run "$gate_repo/README.md")
  run_readme_on "$readme" recovery node-deps
  status=$?
  summary "a run with the machine stopped passed, its receipt and record verified" "$status" "the README's command on node-deps"
  gate_check "the run started the stopped machine" "the machine is not running" -- \
    sh -c '"$1" machine inspect --format "{{.State}}" | grep -qi running' gate "$podman"
  path_end
}

real_repo_run() {
  # real_repo_run NAME CLONE TASK: the timed run, its verdict and receipt.
  local name=$1 clone=$2 task_text=$3 status=0
  gate_step "run fix on $name (verdict 0, 1 or 2, not infrastructure)" --expect 0 -- /bin/bash -c \
    'cd "$1" && axocoatl run fix --task "$2" --junit junit.xml --record run.axorecord.jsonl --receipt receipt.json; s=$?; echo "exit $s"; [ $s -le 2 ]' \
    gate-run "$clone" "$task_text" || status=1
  mkdir -p "$GATE_ATTACH/$name"
  for file in receipt.json run.axorecord.jsonl junit.xml; do
    [ -f "$clone/$file" ] && cp "$clone/$file" "$GATE_ATTACH/$name/"
  done
  if [ -f "$clone/receipt.json" ]; then
    gate_step "receipt verify on $name (documented command)" -- /bin/bash -c \
      'cd "$1" && axocoatl receipt verify receipt.json --key "$(axocoatl receipt key)" --repo .' gate-verify "$clone" || status=1
  else
    gate_record "receipt verify on $name (documented command)" fail '' '' '' 'the run wrote no receipt'
    status=1
  fi
  gate_markup_check "$name" "$clone" "$clone/run.axorecord.jsonl" receipt.json junit.xml || status=1
  return "$status"
}

path_real_repo() {
  path_begin real-repo
  install_once || { path_end; return; }
  ensure_daemon
  if [ -n "$real_repo" ] || [ -n "$real_task" ]; then
    if [ -z "$real_repo" ] || [ -z "$real_task" ]; then
      gate_record "a timed run on --real-repo" fail '' '' '' "pass both --real-repo DIR and --real-task TEXT"
    else
      local clone="$work/repos/real-repo"
      rm -rf "$clone"
      gate_step "clone the real repository" -- git clone -q --no-hardlinks "$real_repo" "$clone" \
        && real_repo_run real-repo "$clone" "$real_task"
    fi
    path_end
    return
  fi
  # NUL-delimited fields preserve absent self ref/commit fields and every
  # literal task character (including tabs and newlines). Materialize the
  # rows first so invalid input cannot silently turn into an empty loop.
  local name url ref pinned task_text clone status=0 names='' rows="$GATE_TMP/real-repos.fields"
  if ! node -e '
    const repos = require(process.argv[1]).repos;
    if (!Array.isArray(repos) || !repos.length) throw Error("no pinned real repositories");
    for (const r of repos) {
      const fields = [r.name, r.url, r.ref ?? "", r.commit ?? "", r.task];
      if (fields.some(v => typeof v !== "string" || v.includes("\0")) || !r.name || !r.url || !r.task.trim()) {
        throw Error("invalid pinned real-repository fields");
      }
      process.stdout.write(fields.join("\0") + "\0");
    }
  ' "$script_dir/real-repos.json" >"$rows" 2>"$GATE_ATTACH/logs/real-repos.log" \
    || ! cp "$script_dir/real-repos.json" "$GATE_ATTACH/real-repos.json"; then
    gate_record "read the pinned real-repository tasks" fail '' '' '' 'cannot read or retain real-repos.json; see logs/real-repos.log'
    path_end
    return
  fi
  while IFS= read -r -d '' name && IFS= read -r -d '' url \
    && IFS= read -r -d '' ref && IFS= read -r -d '' pinned && IFS= read -r -d '' task_text; do
    clone="$work/repos/real-$name"
    names="$names${names:+, }$name"
    rm -rf "$clone"
    if [ "$url" = self ]; then
      gate_step "clone $name at the candidate commit" -- /bin/bash -c \
        'git clone -q --no-hardlinks "$1" "$2" && git -C "$2" checkout -q "$3"' gate-clone "$gate_repo" "$clone" "$commit" || { status=1; continue; }
    else
      gate_step "clone $name at $ref" -- git clone -q --depth 1 --branch "$ref" "$url" "$clone" || { status=1; continue; }
      gate_check "$name is at the pinned commit ${pinned:0:12}" "it is at $(git -C "$clone" rev-parse HEAD 2>/dev/null)" -- \
        test "$(git -C "$clone" rev-parse HEAD 2>/dev/null)" = "$pinned" || { status=1; continue; }
    fi
    real_repo_run "$name" "$clone" "$task_text" || status=1
  done <"$rows"
  summary "a timed run on every pinned real repository ended with a verdict, its receipt verified" "$status" "$names"
  path_end
}

run_path() {
  case "$1" in
    local) path_local ;;
    claude-code) path_claude_code ;;
    codex) path_codex ;;
    openrouter) path_openrouter ;;
    service) path_service ;;
    workbench | workbench-webkit) path_workbench webkit Safari ;;
    workbench-chromium) path_workbench chromium "Google Chrome" ;;
    recovery) path_recovery ;;
    real-repo) path_real_repo ;;
    *)
      gate_say "unknown path $1"
      return 1
      ;;
  esac
}

failed=0
old_ifs=$IFS
IFS=,
for path_name in $paths; do
  IFS=$old_ifs
  run_path "$path_name" || failed=1
  if [ "$onboarding_interrupted" = 1 ]; then
    failed=130
    break
  fi
done
IFS=$old_ifs
echo
echo "Evidence: $evidence_root/$version/$commit/"
echo "Next: scripts/release-gate/collect-evidence.sh --version $version --commit $commit --from $evidence_root"
exit "$failed"
