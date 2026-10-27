#!/usr/bin/env bash
# Supplemental D1 continuation after preserved OAuth401; never a second fresh install.
# Prepared only. Root coordinates browser authorization and confirms the new key's
# $2 TOTAL/non-resetting/zero-usage limit before answering first-smoke consent.
set -uo pipefail
umask 077
[ "${AXO_D1_CONTINUATION_APPROVED:-}" = 1 ] || { echo 'Root must explicitly launch this reviewed continuation.' >&2; exit 2; }
[ "$(id -un)" = d1gate ] && [ "$(uname -s)" = Linux ] || exit 2
base=/home/d1gate/gate-assets/restart-b222302f-20261027
candidate="$base/candidate/source"
script_dir="$candidate/scripts/release-gate"
. "$script_dir/lib.sh"
commit=b222302f32639c9211255ab4f818c3262bce856a
version=1.5.0
label=D1-continuation
evidence_root="$base/continuation-evidence"
[ ! -e "$evidence_root" ] || gate_die 'continuation output already exists; never overwrite it'
export PATH="$HOME/.local/bin:$PATH"
[ "$(gate_sha256 "$HOME/.local/bin/axocoatl")" = 9e03af65d461cf23f8577bc3cee14667efc44db4726a8a3556dd3a5c6419c0b8 ] || gate_die 'installed binary differs from exact b222 candidate; this plan must be reviewed again'
[ "$(gate_sha256 "$base/failed-oauth-401-preservation/failed-oauth-401.tar.gz")" = 51ec749cd0dc10163f951db0d70c8772b889729807b563852818e0a437f6b564 ] || gate_die 'preserved predecessor archive changed'
[ "$(gate_sha256 "$candidate/scripts/release-gate/newuser-linux.sh")" = 83472f106f85d52686d3e70ec3d68eb93caff73c64fdf8632bca65336f16116c ] || gate_die 'original gate source changed: scripts/release-gate/newuser-linux.sh'
[ "$(gate_sha256 "$candidate/scripts/release-gate/lib.sh")" = cfe0d44083779605c13986fa80061294882d1d4574119117c823b8dc4238fa0b ] || gate_die 'original gate source changed: scripts/release-gate/lib.sh'
[ "$(gate_sha256 "$candidate/scripts/release-gate/evidence.mjs")" = bcd138fe8f8c68aa668894cdc2eb4c59f76c3689237fde3569800a05649d9729 ] || gate_die 'original gate source changed: scripts/release-gate/evidence.mjs'
[ "$(gate_sha256 "$candidate/scripts/release-gate/published-commands.mjs")" = ae2334357df3d1676048a95f84db486445cb4597c6b20aa509284ac55c7b5d5f ] || gate_die 'original gate source changed: scripts/release-gate/published-commands.mjs'
if [ -z "${GATE_IN_SCRIPT:-}" ]; then
  transcript="$base/continuation-$(date -u +%Y%m%dT%H%M%SZ).txt"
  echo "Recording this continuation to $transcript"
  GATE_IN_SCRIPT=$transcript exec script -q -e -f -c "$(printf '%q ' bash "$0")" "$transcript"
fi
live=1 simulation=0 port='' port_given=0 run_command='' dev_pid='' stub_url='' system_changes=0
service_writer_model=openrouter:deepseek/deepseek-chat-v3.1
work=$(mktemp -d /tmp/axo-d1-continuation.XXXXXX)
: >"$work/guard.log"
cleanup() {
  if [ -n "$dev_pid" ] && kill -0 "$dev_pid" 2>/dev/null; then
    kill -INT "$dev_pid" 2>/dev/null
    wait "$dev_pid" 2>/dev/null
  fi
  gate_say "kept continuation work $work"
}
trap cleanup EXIT
trap 'exit 130' INT TERM
gate_begin "$evidence_root" "$version" "$commit" "$label" D
GATE_RUNNER='local Linux D1 continuation; Codex operator, d1gate OS account'
GATE_AXOCOATL_VERSION=$(axocoatl --version)
export GATE_RUNNER GATE_AXOCOATL_VERSION
gate_host_json "$GATE_TMP/host.json"
touch "$GATE_TMP/began"
cp "$base/operator.json" "$GATE_ATTACH/operator-initial.json"
cp "$base/failed-oauth-401-preservation/manifest.json" "$GATE_ATTACH/prior-failed-attempt-manifest.json"
cat >"$GATE_ATTACH/continuation-provenance.json" <<'JSON'
{"candidate_commit":"b222302f32639c9211255ab4f818c3262bce856a","operator":"Codex assistant; browser authorization by root coordinator only after explicit user authorization","os_account":"d1gate","fresh_install_repeated":false,"original_failure_preserved":true,"original_failure":"Fresh OAuth exchange succeeded; immediate GET/key HTTP401; no secret/config stored and zero inference","original_archive_sha256":"51ec749cd0dc10163f951db0d70c8772b889729807b563852818e0a437f6b564","new_account_or_sudo_grants":false,"reset_or_reinstall":false,"budget_requirement":"dedicated Linux key $2TOTAL, nonresetting, zero prior usage verified before first inference","harness_changes":["fail immediately on onboarding failure","require absent OpenRouter secret before retry","attribute provider success to continued onboarding and Codex operator"],"diagnostic_limitation":"Independent Python exchange and key lookup returned200 immediately; this supports one controlled retry but does not establish the cause of candidate401.","formal_claim":"Supplemental continuation only. Original fresh setup and failed first onboarding remain separately preserved. This path does not overwrite or claim a second fresh D1 pass."}
JSON
finish() {
  gate_keep_onboarding_logs "$GATE_TMP/began"
  [ -n "${GATE_IN_SCRIPT:-}" ] && cp "$GATE_IN_SCRIPT" "$GATE_ATTACH/transcript.txt" 2>/dev/null
  gate_tools_json "$GATE_TMP/tools.json"
  cp "$work/guard.log" "$GATE_ATTACH/logs/guard.log" 2>/dev/null || true
  gate_finish --json host="$GATE_TMP/host.json" --json tools="$GATE_TMP/tools.json"
  exit $?
}

# Original provider-observation checks, with truthful continuation attribution.
# Historical install/subid claims are not replayed.
continued_provider_steps() {
  local stored lines
  gate_secret_stored openrouter
  stored=$?
  if [ "$stored" = 0 ]; then
    gate_ask "When onboarding asked how to get the OpenRouter key, did you choose the browser sign-in and sign in to OpenRouter in the Mac's browser (no key typed or pasted)?"
    if [ "$ANSWER" = y ]; then
      gate_record "OpenRouter was connected through the browser sign-in during onboarding" pass '' '' '' \
        "the secret openrouter was stored during continued onboarding, and the Codex operator (OS account $(id -un)) answered yes"
    else
      gate_record "OpenRouter was connected through the browser sign-in during onboarding" fail '' '' '' "the operator answered no: ${ANSWER_NOTE:-no note}"
    fi
  elif [ "$stored" = 2 ]; then
    gate_record "OpenRouter was connected through the browser sign-in during onboarding" fail '' '' '' \
      "cannot tell whether onboarding stored openrouter: $GATE_SECRET_ERROR"
  else
    # What reached the sign-in's callback, one line per connection, tells a
    # sign-in nobody finished from one whose callback never arrived.
    lines=$(gate_onboarding_log_lines "$GATE_TMP/began" 'OpenRouter sign-in:')
    gate_record "OpenRouter was connected through the browser sign-in during onboarding" fail '' '' '' \
      "onboarding ended without the secret openrouter stored; $([ -n "$lines" ] && printf "onboarding's log: %s" "$lines" || printf "onboarding's log has no 'OpenRouter sign-in:' line, so no connection reached the sign-in's callback")"
  fi
  if command -v ollama >/dev/null 2>&1 || [ -e /etc/systemd/system/ollama.service ]; then
    gate_record "onboarding did not install Ollama" fail '' '' '' "$(command -v ollama || echo /etc/systemd/system/ollama.service)"
  else
    gate_record "onboarding did not install Ollama" pass '' '' '' 'no ollama on PATH, no ollama service'
  fi
}


# This controlled retry must perform a fresh browser exchange, never reuse a key.
gate_secret_stored openrouter
initial_secret_status=$?
if [ "$initial_secret_status" != 1 ]; then
  gate_record "no OpenRouter secret before controlled onboarding retry" fail '' '' '' "expected absent status1; observed status$initial_secret_status (stored or unreadable); no authentication attempted"
  finish
fi
gate_record "no OpenRouter secret before controlled onboarding retry" pass '' '' '' 'secret absent; fresh browser authorization required'

# Continue at the original interactive onboarding command. A failure ends this
# continuation before dependent doctor, fixture or service commands can cascade.
if ! gate_step "onboard continuation (interactive; --start none)" --interactive -- axocoatl onboard --start none; then
  finish
fi
gate_record "onboarding continuation was answered live" pass '' '' '' 'Codex operator, d1gate OS account; prompts and answers in transcript'
continued_provider_steps

# Unchanged original script from config validation through finish.
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

# 5. The daemon, on its configured port: an Axocoatl /health that names its
# version and started after the configuration last changed.
cd "$work" || finish
GATE_DAEMON_PORT=$port
export GATE_DAEMON_PORT
axocoatl dev >"$GATE_ATTACH/logs/dev.log" 2>&1 &
dev_pid=$!
state=none
deadline=$((SECONDS + 300))
while [ "$SECONDS" -lt "$deadline" ]; do
  state=$(gate_daemon_state "$port" "$config")
  [ "$state" = current ] && break
  kill -0 "$dev_pid" 2>/dev/null || break
  sleep 0.5
done
if [ "$state" = current ]; then
  gate_record "axocoatl dev serves on its configured port" pass '' '' '' "port $port"
else
  gate_record "axocoatl dev serves on its configured port" fail '' '' '' \
    "$(gate_daemon_state_says "$state" "$port"); $(tail -n 3 "$GATE_ATTACH/logs/dev.log" | tr '\n' ' ')"
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
fixtures_status=0
[ -n "$readme_command" ] || fixtures_status=1
for fixture in "${fixtures[@]}"; do
  [ -n "$readme_command" ] || break
  fixture_command=$readme_command
  [ -n "$run_command" ] || fixture_command=$(gate_fixture_command "$fixture" "$readme_command")
  [ "$live" = 0 ] && gate_curl -fsS -X POST "$stub_url/__gate/reset" -d '{}' >/dev/null
  gate_run_fixture "$fixture" "$fixture_command" "$work/repos/$fixture" || fixtures_status=1
  if [ "$live" = 0 ]; then
    edits=$(gate_curl -fsS "$stub_url/__gate/stats" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>process.stdout.write(JSON.parse(s).edits.map(e=>e.fixture).join(",")))')
    gate_check "the model fixed $fixture once" "the stub's edits: ${edits:-none}" -- test "$edits" = "$fixture"
  fi
done
if [ "$fixtures_status" = 0 ]; then
  gate_record "the README's command passed on every fixture, each receipt and record verified" pass '' '' '' "fixtures: ${fixtures[*]}"
else
  gate_record "the README's command passed on every fixture, each receipt and record verified" fail '' '' '' "fixtures: ${fixtures[*]}: see the failing steps above"
fi

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
  gate_curl -fsS -X POST "$stub_url/__gate/reset" -d '{}' >/dev/null
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
  gate_curl -fsS -X POST "$stub_url/__gate/reset" -d '{}' >/dev/null
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
    gate_skip "doctor FAILs the subuid range with the usermod fix" "needs --system-changes and passwordless sudo (CI)"
  fi
else
  gate_skip "negative cases with the scripted model server" "a live run (Tier D) has no stub" optional
fi

# 9. The copy-paste lint, when the tree has it.
if [ -f "$gate_repo/scripts/check-copy-paste.mjs" ]; then
  gate_copy_paste_lint "$gate_repo" "$work/npm-cache"
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

# 11. Live: the Always-On Service (a systemd user unit), as a person meets
# it. Step 6 of `onboard --update` installs and starts it; a second
# `onboard --update` changes the writer model while it runs, and its Step 7
# restarts it; a fix runs through it; then `service status`, `stop` and
# `uninstall`.
if [ "$live" = 1 ]; then
  if [ "$simulation" = 1 ] || systemctl --user show-environment >/dev/null 2>&1; then
    gate_record "this login has a systemd user session (the service needs one)" pass '' '' '' "$(id -un)"
  else
    gate_record "this login has a systemd user session (the service needs one)" fail '' '' '' \
      "systemctl --user cannot reach the user's manager: log in as $(id -un) over SSH (not with su or sudo -iu)"
  fi
  printf '\nIn Step 6 ("How should Axocoatl run?"), choose "Always on, starting at login" and agree to install it.\n'
  gate_step "onboard --update: Step 6, the Always-On Service (interactive)" --interactive -- axocoatl onboard --update
  gate_service_status "onboarding installed and started the Always-On Service (Step 6)" yes yes
  state=$(gate_wait_daemon "$port" "$config" 300)
  gate_check "the service answers on port $port" "$(gate_daemon_state_says "$state" "$port")" -- test "$state" = current
  writer=$(gate_run_model "$config" writer)
  reviewer=$(gate_run_model "$config" reviewer)
  if [ -z "$service_writer_model" ]; then
    for candidate in openrouter:z-ai/glm-4.6 openrouter:deepseek/deepseek-chat-v3.1; do
      [ "$candidate" != "$writer" ] && [ "$candidate" != "$reviewer" ] && { service_writer_model=$candidate; break; }
    done
  fi
  from=$(wc -c <"${GATE_IN_SCRIPT:-/dev/null}" 2>/dev/null || echo 0)
  gate_step "onboard --update --writer-model $service_writer_model --smoke-test (the configuration changes while the service runs)" --interactive -- \
    axocoatl onboard --update --writer-model "$service_writer_model" --smoke-test --start none
  tail -c +"$((from + 1))" "${GATE_IN_SCRIPT:-/dev/null}" >"$work/onboard-output.txt" 2>/dev/null
  gate_service_restart_steps "$config" "$port" "$work/onboard-output.txt"
  service_command=$readme_command
  [ -n "$run_command" ] || service_command=$(gate_fixture_command node-deps "$readme_command")
  if gate_run_fixture node-deps "$service_command" "$work/repos/service-node-deps" service-node-deps; then
    gate_record "a fix through the Always-On Service passed, its receipt and record verified" pass '' '' '' "node-deps, writer $service_writer_model, port $port"
  else
    gate_record "a fix through the Always-On Service passed, its receipt and record verified" fail '' '' '' "node-deps, writer $service_writer_model: see the failing steps above"
  fi
  gate_service_status "axocoatl service status: installed and running" yes yes
  gate_service_end "$port"
fi

finish
