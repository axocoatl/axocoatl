#!/bin/bash
# Clean-machine new-user gate, macOS (docs/RELEASE_GATE.md, Tier C).
#
# Run by a person, in the dedicated gate account (GATE_ACCOUNT, default
# axo-newuser) right after reset-mac-account.sh, on the release candidate:
# it reads the staged site's commands and runs them as published: the
# install command exactly as written, then (after checking the site shows
# them) `axocoatl onboard` with every consent prompt answered live,
# `axocoatl doctor` and `axocoatl dev`, then the site's and README's run
# commands. Onboarding gets --start none: the gate starts the daemon itself,
# so the sign-in link never reaches the transcript. The whole session is
# recorded with script(1); each step's wall time and download are measured.
# Evidence goes to <evidence-root>/<version>/<commit>/<machine>-<path>.json
# and its folder.
#
# Usage:
#   newuser-mac.sh --release-dir DIR --site DIR [options]
#     --release-dir DIR   the candidate's axocoatl-vX.Y.Z-<mac target>.tar.gz
#                         (and .sha256): what the installer will download
#     --site DIR          the staged marketing site (index.html, install.sh, ...)
#     --machine C1|C2     C1: the main gate Mac; C2: the 16 GB Mac (default C1)
#     --paths LIST        comma-separated, default for C1:
#                         local,claude-code,codex,openrouter,workbench
#                         (also: workbench-chromium,recovery,real-repo);
#                         default for C2: local,openrouter
#     --commit SHA        commit the candidate was built from (default HEAD)
#     --evidence-root DIR default <repo>/release-evidence
#     --pr-repo OWNER/axocoatl-gate-NAME  throwaway repository for --keep pr
#     --real-repo DIR --real-task TEXT    the real-repo path on this repository
#                         instead of the pinned ones in real-repos.json
#     --yes               answer onboarding with flags instead of prompts
#                         (allowed for later commits of a release whose
#                         interactive run is already recorded)
#
# Depends on (being built in parallel; see docs/RELEASE_GATE.md): onboard's
# --update/--writer/--yes flags and its Ready summary, doctor --json, and
# the copy fixes that make the site's commands runnable as written.
# Scripts handed to `sh -c` and `bash -c` take their values as positional
# parameters.
# shellcheck disable=SC2016
set -u

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=scripts/release-gate/lib.sh
. "$script_dir/lib.sh"

usage() {
  awk 'NR == 1 { next } /^#/ { if ($0 ~ /shellcheck|positional param|take their values|read their values|^# parameters\.$/) next; sub(/^# ?/, ""); print; next } { exit }' "$0"
  exit 2
}

account=${GATE_ACCOUNT:-axo-newuser}
release_dir='' site='' machine=C1 paths='' commit='' evidence_root="$gate_repo/release-evidence"
pr_repo='' real_repo='' real_task='' assume_yes=0
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
  if [ "$machine" = C1 ]; then paths=local,claude-code,codex,openrouter,workbench; else paths=local,openrouter; fi
fi

work="$HOME/axocoatl-gate-work"
mkdir -p "$work"
if [ -z "${GATE_IN_SCRIPT:-}" ]; then
  transcript="$work/transcript-$(date -u +%Y%m%dT%H%M%SZ).txt"
  echo "Recording this session to $transcript"
  GATE_IN_SCRIPT=$transcript exec script -q "$transcript" /bin/bash "$0" ${original_args[@]+"${original_args[@]}"}
fi
transcript=$GATE_IN_SCRIPT

GATE_BASE_PATH=$(gate_new_user_path)
export GATE_BASE_PATH PATH="$GATE_BASE_PATH"
models_cached=no
grep -q 'models_cached=1' "$HOME/.axocoatl-gate-reset" 2>/dev/null && models_cached=yes
task="Paging skips the last item when the total is a multiple of the page size"
dev_pid=''
trap '[ -n "$dev_pid" ] && kill -INT "$dev_pid" 2>/dev/null' EXIT

confirm() {
  # confirm NAME QUESTION: the operator answers y or n; n asks why.
  local answer note
  printf '\n>>> %s [y/n] ' "$2" >/dev/tty
  read -r answer </dev/tty
  case "$answer" in
    y | Y | yes) gate_record "$1" pass '' '' '' "confirmed by $(id -un)" ;;
    *)
      printf '>>> What did you see instead? ' >/dev/tty
      read -r note </dev/tty
      gate_record "$1" fail '' '' '' "operator: ${note:-no note}"
      ;;
  esac
}

config_file() { gate_config_path; }
daemon_port() { gate_server_port "$(config_file)"; }
daemon_up() { curl -fsS "http://127.0.0.1:${1:-$(daemon_port)}/health/live" >/dev/null 2>&1; }

# daemon_stale PORT: the daemon on PORT started before the configuration last
# changed (`onboard --update` after it started), or does not say when it
# started (before 1.5.0), so it runs an older configuration.
daemon_stale() {
  curl -fsS "http://127.0.0.1:$1/health" 2>/dev/null | node -e '
    let text = "";
    process.stdin.on("data", (d) => (text += d)).on("end", () => {
      let started;
      try { started = JSON.parse(text).started_at_ms; } catch { process.exit(1); }
      const changed = require("fs").statSync(process.argv[1]).mtimeMs;
      process.exit(typeof started !== "number" || changed > started ? 0 : 1);
    });' "$(config_file)"
}

ensure_daemon() {
  local port
  port=$(daemon_port)
  [ -n "$port" ] || port=8080
  if daemon_up "$port"; then
    if ! daemon_stale "$port"; then
      gate_record "the daemon answers on port $port" pass '' '' '' 'already running (service or dev)'
      return 0
    fi
    # As a person would after onboarding changed the configuration: restart
    # the `axocoatl dev` this gate started. Anything else is a failure.
    if [ -z "$dev_pid" ] || ! kill -0 "$dev_pid" 2>/dev/null; then
      gate_record "the daemon on port $port runs the current configuration" fail '' '' '' \
        "it started before $(config_file) last changed, and it is not the axocoatl dev this gate started"
      return 1
    fi
    kill -INT "$dev_pid" 2>/dev/null
    wait "$dev_pid"
    dev_pid=''
    gate_record "restart axocoatl dev after the configuration changed" pass '' '' '' \
      "it started before $(config_file) last changed" optional
  fi
  cd "$work" || return 1
  axocoatl dev >"$GATE_ATTACH/logs/dev.log" 2>&1 &
  dev_pid=$!
  local waited=0
  while [ $waited -lt 600 ]; do
    daemon_up "$port" && break
    kill -0 "$dev_pid" 2>/dev/null || break
    sleep 1
    waited=$((waited + 1))
  done
  gate_check "axocoatl dev answers on port $port" "$(tail -n 3 "$GATE_ATTACH/logs/dev.log" 2>/dev/null | tr '\n' ' ')" -- daemon_up "$port"
}

stop_daemon() {
  [ -n "$dev_pid" ] || return 0
  kill -INT "$dev_pid" 2>/dev/null
  wait "$dev_pid"
  local status=$?
  dev_pid=''
  gate_check "Ctrl+C stops axocoatl dev with exit 0" "exit $status" -- test "$status" = 0
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
  gate_begin "$evidence_root" "$version" "$commit" "$machine-$1" C
  gate_host_json "$GATE_TMP/host.json"
  gate_record "free disk at the start" pass '' '' '' "$(($(gate_free_kb "$HOME") / 1024)) MiB free; models cached: $models_cached" optional
  printf '\n===== %s-%s (%s at %s) =====\n' "$machine" "$1" "$version" "${commit:0:12}"
}

path_end() {
  gate_record "free disk at the end" pass '' '' '' "$(($(gate_free_kb "$HOME") / 1024)) MiB free" optional
  GATE_AXOCOATL_VERSION=$(axocoatl --version 2>/dev/null)
  GATE_OLLAMA_VERSION=$(curl -fsS http://127.0.0.1:11434/api/version 2>/dev/null | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{process.stdout.write(JSON.parse(s).version)}catch{}})')
  export GATE_AXOCOATL_VERSION GATE_OLLAMA_VERSION
  gate_tools_json "$GATE_TMP/tools.json"
  cp "$transcript" "$GATE_ATTACH/transcript.txt" 2>/dev/null
  gate_finish --json host="$GATE_TMP/host.json" --json tools="$GATE_TMP/tools.json"
}

install_once() {
  if command -v axocoatl >/dev/null 2>&1; then
    gate_record "axocoatl is installed" pass '' '' '' "$(command -v axocoatl)"
    return 0
  fi
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

onboard() {
  # onboard NAME FLAG... : interactive unless --yes was given. Either way
  # --start none: Step 6's default would run `axocoatl dev` in the
  # foreground (blocking the gate, and printing the sign-in link into the
  # transcript); the gate starts the daemon itself.
  local name=$1
  shift
  if [ "$assume_yes" = 1 ]; then
    gate_step "$name (--yes)" -- axocoatl onboard --yes "$@" --start none
  else
    gate_step "$name (interactive; every answer is in the transcript)" --interactive -- axocoatl onboard "$@" --start none
  fi
}

run_published_on() {
  # run_published_on COMMAND LABEL FIXTURE...
  local command=$1 label=$2 fixture
  shift 2
  for fixture in "$@"; do
    gate_run_fixture "$fixture" "$command" "$work/repos/$label-$fixture" "$label-$fixture"
  done
}

run_readme_on() {
  # run_readme_on README_COMMAND LABEL FIXTURE...: the README's command, or a
  # fixture's own (gate_fixture_command: node-plain, onboarding's sample).
  local command=$1 label=$2 fixture
  shift 2
  for fixture in "$@"; do
    gate_run_fixture "$fixture" "$(gate_fixture_command "$fixture" "$command")" "$work/repos/$label-$fixture" "$label-$fixture"
  done
}

all_fixtures() { gate_fixture_names | tr '\n' ' '; }

path_local() {
  path_begin local
  install_once || { path_end; return; }
  if [ "$assume_yes" = 1 ]; then
    onboard "onboard: local models" --writer ollama --podman create --pull --smoke-test
  else
    onboard "onboard: local models"
  fi
  if [ "$machine" = C2 ]; then
    confirm "onboarding steered a 16 GB Mac to an OpenRouter reviewer" "Did onboarding recommend an OpenRouter reviewer instead of a 33 GB local pull?"
    confirm "a context below 16k was refused with its fix" "Did onboarding or the first run refuse a model context below 16,384 tokens and name OLLAMA_CONTEXT_LENGTH?"
  else
    confirm "onboarding ended Ready with a passing first run" "Did onboarding end with 'Ready' and a passing first fix run on the sample repository?"
  fi
  doctor_ready
  ensure_daemon
  local readme
  readme=$(node "$gate_commands_js" readme-run "$gate_repo/README.md")
  # shellcheck disable=SC2046
  run_readme_on "$readme" readme $(all_fixtures)
  path_end
}

path_claude_code() {
  path_begin claude-code
  install_once || { path_end; return; }
  onboard "onboard --update --writer claude-code" --update --writer claude-code
  doctor_ready fix-claude-code
  ensure_daemon
  local hero
  hero=$(node "$gate_commands_js" first "$site/index.html" "axocoatl run fix-claude-code") \
    || gate_record "the home page shows the fix-claude-code command" fail '' '' '' 'not found'
  if [ -n "$hero" ]; then
    printf '%s\n' "$hero" >"$GATE_ATTACH/hero-command.txt"
    run_published_on "$hero" hero node-deps python-pytest
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

keep_pr() {
  local hero=$1 slug=${pr_repo#*/} clone="$work/pr-repo" url
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
  workflow=$(node "$gate_commands_js" yaml-block "$gate_repo/sites/docs/src/content/docs/workbench/receipts.mdx" verify-receipt) \
    || { gate_record "the docs show a verify-receipt workflow" fail '' '' '' 'no yaml block names verify-receipt'; return; }
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
  gate_record "reset $pr_repo to the node-deps fixture" pass '' '' '' 'verify-receipt workflow from the docs, pinned to the candidate commit'
  gate_step "hero command with --keep pr" -- /bin/bash -c "cd \"\$1\" && $hero --keep pr" gate-run "$clone" || return
  url=$(grep -Eo 'https://github\.com/[^ ]+/pull/[0-9]+' "$GATE_ATTACH/logs/$(gate_slug "hero command with --keep pr").log" | tail -n 1)
  [ -n "$url" ] || { gate_record "--keep pr opened a pull request" fail '' '' '' 'no pull request URL in the output'; return; }
  gate_record "--keep pr opened a pull request" pass '' '' '' "$url"
  sleep 15
  gate_step "the verify-receipt Action passes on the pull request" -- gh pr checks "$url" --watch --interval 15
}

path_codex() {
  path_begin codex
  install_once || { path_end; return; }
  onboard "onboard --update --writer codex" --update --writer codex
  confirm "Codex billing was set to the ChatGPT plan" "Did onboarding ask how to bill Codex, and did you choose the ChatGPT plan?"
  doctor_ready fix-codex
  ensure_daemon
  local command
  command=$(node "$gate_commands_js" first "$site/index.html" "axocoatl run fix-codex" 2>/dev/null \
    || node "$gate_commands_js" first "$site/install/index.html" "axocoatl run fix-codex")
  if [ -z "$command" ]; then
    gate_record "the site shows a fix-codex command" fail '' '' '' 'not found'
  else
    command=$(printf '%s' "$command" | sed "s/--task \"…\"/--task \"$task\"/")
    run_published_on "$command" codex node-deps python-pytest
  fi
  path_end
}

path_openrouter() {
  path_begin openrouter
  install_once || { path_end; return; }
  # The OpenRouter key comes from the browser sign-in (OAuth PKCE) into the
  # secret store; nobody types, pastes or exports it. A --yes rerun signs in
  # first and lets onboarding use the connected key.
  if [ "$assume_yes" = 1 ]; then
    gate_step "connect OpenRouter in the browser (axocoatl connect openrouter)" --interactive -- axocoatl connect openrouter
    onboard "onboard --update --writer openrouter" --update --writer openrouter
  else
    onboard "onboard --update --writer openrouter (connect in the browser when it asks)" --update --writer openrouter
  fi
  confirm "onboarding asked for different writer and reviewer models" "Did onboarding ask separately for the writer and the reviewer model, and refuse the same model for both?"
  doctor_ready
  ensure_daemon
  run_published_on "axocoatl run fix --task \"$task\"" openrouter node-deps python-pytest
  if [ "$machine" = C1 ]; then
    local reviewer
    reviewer=$(awk '/^run:/ { r = 1; next } r && /^[^[:space:]]/ { r = 0 } r && /reviewer:/ { gsub(/"/, "", $2); print $2; exit }' "$(config_file)")
    case "$reviewer" in
      openrouter:*) run_published_on "axocoatl run fix-claude-code --task \"$task\" --model reviewer=$reviewer" claude-openrouter node-deps ;;
      *) gate_record "the configured reviewer is on OpenRouter" fail '' '' '' "run.models.reviewer is '${reviewer:-unset}'" ;;
    esac
  fi
  path_end
}

path_workbench() {
  local browser=$1 app=$2
  path_begin "workbench-$browser"
  install_once || { path_end; return; }
  ensure_daemon
  local repo="$work/repos/workbench"
  gate_fixture_repo node-deps "$repo"
  git -C "$repo" config --unset user.name 2>/dev/null
  local link
  link=$(axocoatl url 2>/dev/null | grep -Eo 'https?://[^ ]+' | head -n 1)
  if [ -z "$link" ]; then
    gate_record "axocoatl url prints a sign-in link" fail '' '' '' 'no link'
  else
    gate_record "axocoatl url prints a sign-in link" pass '' '' '' '(not recorded: it signs in)'
    open -a "$app" "$link"
  fi
  printf '\nIn %s: use the workbench as a new user would, on %s\n' "$app" "$repo"
  confirm "the sign-in link opened the workbench" "Did the link open the workbench in $app, signed in?"
  confirm "Open workspace refuses the home folder" "Choose Open workspace… with your home folder ($HOME). Was it refused?"
  confirm "Open workspace accepts the repository" "Now open $repo as the workspace. Was it accepted?"
  confirm "New session starts" "Create a New session. Did it start?"
  confirm "Team & budget is pre-filled and Apply works" "Open Team & budget. Were the limits pre-filled, and did Apply succeed without editing them?"
  confirm "one turn edits a file" "Send one turn asking to fix the paging bug in src/paginate.js. Did the turn finish and change the file?"
  confirm "Source Control shows Last turn 1" "Open Source Control. Does it show 'Last turn 1' with the change?"
  confirm "a commit from the workbench succeeds" "Commit the change from Source Control. Did it succeed?"
  local author expected
  author=$(git -C "$repo" log -1 --format='%an <%ae>' 2>/dev/null)
  expected=$(git config --global user.name 2>/dev/null)
  gate_check "the commit is authored by this account's identity" "author: ${author:-no commit}" -- \
    sh -c '[ -n "$1" ] && [ "$1" != "New User <new-user@example.invalid>" ] && { [ -z "$2" ] || case "$1" in "$2 "*) true ;; *) false ;; esac; }' gate "$author" "$expected"
  stop_daemon
  path_end
}

path_recovery() {
  path_begin recovery
  install_once || { path_end; return; }
  ensure_daemon
  gate_step "podman machine stop" -- podman machine stop
  local readme
  readme=$(node "$gate_commands_js" readme-run "$gate_repo/README.md")
  run_readme_on "$readme" recovery node-deps
  gate_check "the run started the stopped machine" "the machine is not running" -- \
    sh -c 'podman machine inspect --format "{{.State}}" | grep -qi running'
  path_end
}

real_repo_run() {
  # real_repo_run NAME CLONE TASK: the timed run, its verdict and receipt.
  local name=$1 clone=$2 task_text=$3
  gate_step "run fix on $name (verdict 0, 1 or 2, not infrastructure)" --expect 0 -- /bin/bash -c \
    'cd "$1" && axocoatl run fix --task "$2" --junit junit.xml --record run.axorecord.jsonl --receipt receipt.json; s=$?; echo "exit $s"; [ $s -le 2 ]' \
    gate-run "$clone" "$task_text"
  mkdir -p "$GATE_ATTACH/$name"
  for file in receipt.json run.axorecord.jsonl junit.xml; do
    [ -f "$clone/$file" ] && cp "$clone/$file" "$GATE_ATTACH/$name/"
  done
  if [ -f "$clone/receipt.json" ]; then
    gate_step "receipt verify on $name (documented command)" -- /bin/bash -c \
      'cd "$1" && axocoatl receipt verify receipt.json --key "$(axocoatl receipt key)" --repo .' gate-verify "$clone"
  fi
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
  # The pinned repositories of real-repos.json, each at its commit.
  local name url ref pinned task_text clone
  while IFS=$'\t' read -r name url ref pinned task_text; do
    clone="$work/repos/real-$name"
    rm -rf "$clone"
    if [ "$url" = self ]; then
      gate_step "clone $name at the candidate commit" -- /bin/bash -c \
        'git clone -q --no-hardlinks "$1" "$2" && git -C "$2" checkout -q "$3"' gate-clone "$gate_repo" "$clone" "$commit" || continue
    else
      gate_step "clone $name at $ref" -- git clone -q --depth 1 --branch "$ref" "$url" "$clone" || continue
      gate_check "$name is at the pinned commit ${pinned:0:12}" "it is at $(git -C "$clone" rev-parse HEAD 2>/dev/null)" -- \
        test "$(git -C "$clone" rev-parse HEAD 2>/dev/null)" = "$pinned" || continue
    fi
    real_repo_run "$name" "$clone" "$task_text"
  done < <(node -e '
    for (const r of require(process.argv[1]).repos) console.log([r.name, r.url, r.ref ?? "", r.commit ?? "", r.task].join("\t"));
  ' "$script_dir/real-repos.json")
  path_end
}

run_path() {
  case "$1" in
    local) path_local ;;
    claude-code) path_claude_code ;;
    codex) path_codex ;;
    openrouter) path_openrouter ;;
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
done
IFS=$old_ifs
echo
echo "Evidence: $evidence_root/$version/$commit/"
echo "Next: scripts/release-gate/collect-evidence.sh --version $version --commit $commit --from $evidence_root"
exit "$failed"
