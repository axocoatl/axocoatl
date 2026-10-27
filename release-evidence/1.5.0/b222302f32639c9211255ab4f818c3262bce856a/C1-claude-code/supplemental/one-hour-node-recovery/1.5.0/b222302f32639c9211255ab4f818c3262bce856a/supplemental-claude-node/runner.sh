#!/bin/bash
# Prepared supplemental runtime check only. Does not replace C1-claude-code.
# Root owns coordination; no concurrent OpenRouter inference during this run.
# No onboarding is run. Preserve the earlier interrupted Claude Node attempt.
# No new browser authentication: keep the existing claude-code-oauth sign-in.
set -u
umask 077

if [ "${1:-}" != --run ] || [ "$#" != 1 ]; then
  echo "Usage: /bin/bash $0 --run"
  echo 'Review first. Requires existing Claude/local-reviewer configuration; runs only the supplemental Node fixture.'
  exit 2
fi

repo=/Users/erick/.codex/worktrees/release-1-5-recovery/axocoatl
commit=b222302f32639c9211255ab4f818c3262bce856a
version=1.5.0
binary=/usr/local/bin/axocoatl
binary_sha=d25cd64f70da4d17eb781d223aeab553f4180d074dbb29af302b23492ce31d1b
archive_sha=2a9fc208948c630efab48d2f58875eb0c8f6dd24baec005479f691bdc532d659
candidate="$repo/target/recovery-2026-10-25/candidate-b222302f"
archive="$candidate/axocoatl-v1.5.0-aarch64-apple-darwin.tar.gz"
site="$candidate/site"
evidence_root="$repo/target/recovery-2026-10-25/one-hour-verification-20261026/claude-node"
work="$HOME/axocoatl-gate-work/one-hour-verification-20261026-claude-node"
path_name=supplemental-claude-node
self=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/${0##*/}
export PATH="/Users/erick/.local/share/fnm/node-versions/v22.23.2/installation/bin:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
unset AXOCOATL_URL AXOCOATL_TOKEN AXOCOATL_DATA_DIR

cd "$repo" || exit 2
# shellcheck source=/dev/null
. "$repo/scripts/release-gate/lib.sh"
[ "$(id -un)" = erick ] && [ "$(uname -s)" = Darwin ] || gate_die 'requires the existing erick Mac account'
[ -t 0 ] && [ -t 1 ] || gate_die 'launch in the tool-owned PTY'
[ "$(command -v axocoatl)" = "$binary" ] || gate_die 'unexpected installed executable'
[ "$(gate_sha256 "$binary")" = "$binary_sha" ] || gate_die 'installed executable differs from exact b222'
[ "$(gate_sha256 "$archive")" = "$archive_sha" ] || gate_die 'candidate archive checksum differs'
[ "$(awk 'NR == 1 { print $1 }' "$archive.sha256")" = "$archive_sha" ] || gate_die 'candidate checksum sidecar differs'
[ "$($binary --version)" = 'axocoatl 1.5.0' ] || gate_die 'unexpected CLI version'
git diff --quiet "$commit" -- scripts/release-gate/lib.sh scripts/release-gate/evidence.mjs scripts/release-gate/published-commands.mjs tests/fixtures/newuser/node-deps tests/fixtures/newuser/python-pytest tests/fixtures/newuser/fixtures.json || gate_die 'reused gate or fixture inputs differ from b222'
config=$(gate_config_path)
config_sha=$(gate_sha256 "$config")
port=$(gate_server_port "$config")
port=${port:-8080}
[ "$(gate_daemon_state "$port" "$config")" = none ] || gate_die 'a daemon/listener remains; do not disturb another owner; wait until the port is free'

if [ -z "${AXO_SUPPLEMENT_IN_SCRIPT:-}" ]; then
  [ ! -e "$evidence_root" ] && [ ! -e "$work" ] || gate_die 'supplemental output already exists; preserve it and choose a new reviewed destination'
  mkdir -p "$evidence_root" || exit 2
  transcript="$evidence_root/transcript.txt"
  AXO_SUPPLEMENT_IN_SCRIPT="$transcript" exec script -q -t 0 "$transcript" /bin/bash "$self" --run
fi
transcript=$AXO_SUPPLEMENT_IN_SCRIPT
[ "$transcript" = "$evidence_root/transcript.txt" ] || gate_die 'unexpected transcript destination'
[ ! -e "$evidence_root/$version/$commit/$path_name" ] && [ ! -e "$evidence_root/$version/$commit/$path_name.json" ] && [ ! -e "$work" ] || gate_die 'refusing to overwrite supplemental evidence or fixture repositories'
mkdir "$work" || exit 2
gate_begin "$evidence_root" "$version" "$commit" "$path_name" C
cp "$self" "$GATE_ATTACH/runner.sh"
gate_host_json "$GATE_TMP/host.json"
dev_pid=''

finish() {
  local result=$? final_result waited=0 daemon_status=0
  trap - EXIT
  if [ -n "$dev_pid" ]; then
    kill -INT "$dev_pid" 2>/dev/null || true
    while kill -0 "$dev_pid" 2>/dev/null && [ "$waited" -lt 180 ]; do
      sleep 1
      waited=$((waited + 1))
    done
    if kill -0 "$dev_pid" 2>/dev/null; then
      gate_record 'supplemental daemon stopped' fail '' '' '' 'owned daemon did not settle after SIGINT; preserved for operator inspection'
    else
      wait "$dev_pid" || daemon_status=$?
      gate_check 'supplemental daemon stopped with exit 0' "exit $daemon_status" -- test "$daemon_status" = 0
    fi
  fi
  gate_check 'owner configuration hash is unchanged after fixtures' 'configuration changed during supplemental checks' -- test "$(gate_sha256 "$config")" = "$config_sha"
  GATE_AXOCOATL_VERSION=$($binary --version)
  export GATE_AXOCOATL_VERSION
  gate_tools_json "$GATE_TMP/tools.json"
  cp "$transcript" "$GATE_ATTACH/transcript.txt"
  gate_finish --json host="$GATE_TMP/host.json" --json tools="$GATE_TMP/tools.json"
  final_result=$?
  [ "$result" = 0 ] || exit "$result"
  exit "$final_result"
}
trap finish EXIT

node - "$GATE_ATTACH/provenance.json" "$commit" "$binary_sha" "$archive_sha" <<'JS'
const fs = require('fs');
const [out,commit,binary_sha256,archive_sha256] = process.argv.slice(2);
fs.writeFileSync(out, JSON.stringify({scope:'Supplemental Claude runtime fixtures using existing configuration and Claude subscription authentication', candidate_commit:commit,binary_sha256,archive_sha256,operator:'Codex assistant through tool-owned PTY',human_operator:false,authentication:'Existing claude-code-oauth secret; prior browser authentication completed separately by the user',clean_install:false,fresh_authentication:false,replaces_formal_C1_claude_code:false,formal_failure_preserved:true,required_paths_satisfied:[],onboarding:false,onboarding_smoke:false,configuration_mutated:false,formal_gate_coordination:'Root holds OpenRouter before configuration writes or inference until this script settles',fixture_acceptance:'Independent artifact assessment is separate',linux:'Outside this script; owned by another verifier'},null,2)+'\n');
JS

gate_record 'exact b222 installed binary and candidate archive verified' pass '' '' '' "binary $binary_sha; archive $archive_sha"
if ! gate_secret_stored claude-code-oauth; then
  gate_record 'existing Claude subscription secret is stored' fail '' '' '' 'required existing secret is absent or unreadable; no authentication started'
  exit 1
fi
gate_record 'existing Claude subscription secret is stored' pass '' '' '' 'name-only secret listing; no fresh sign-in claim'
echo 'Fixture-only run: no onboarding, configuration writes, or new authentication.'
reviewer=$(gate_run_model "$config" reviewer)
case "$reviewer" in ollama:gpt-oss:20b) ;; *) gate_record 'reviewer is local Ollama' fail '' '' '' "unexpected reviewer $reviewer"; exit 1 ;; esac
gate_record 'reviewer is local Ollama' pass '' '' '' "$reviewer"
# Snapshot stays owner-only and is not an authentication export; config uses secret references.
cp "$config" "$work/config.snapshot.yaml"
chmod 400 "$work/config.snapshot.yaml"
[ "$(gate_sha256 "$work/config.snapshot.yaml")" = "$config_sha" ] && [ "$(gate_sha256 "$config")" = "$config_sha" ] || { gate_record 'configuration stayed unchanged while snapshotting' fail '' '' '' 'configuration changed; stop before inference'; exit 1; }
node - "$GATE_ATTACH/configuration.json" "$reviewer" "$(gate_sha256 "$work/config.snapshot.yaml")" <<'JS'
const fs=require('fs');const [out,reviewer,config_sha256]=process.argv.slice(2);
fs.writeFileSync(out,JSON.stringify({loadout:'fix-claude-code',credential:'claude-code-oauth',reviewer,configuration_snapshot_sha256:config_sha256,onboarding:false,configuration_mutated:false,configuration_source:'Existing owner config, verified unchanged before each fixture and after all runs',snapshot_mode:'0400'},null,2)+'\n');
JS
before=$GATE_FAILED
gate_doctor_json 'doctor JSON ready for supplemental Claude checks' 0 "$GATE_ATTACH/doctor.json"
gate_check 'doctor ready with no failing item' 'doctor is not ready or reports a failure' -- node -e 'const d=require(process.argv[1]);process.exit(d.ready===true&&!Object.values(d.sections||{}).flat().some(x=>x.status==="fail")?0:1)' "$GATE_ATTACH/doctor.json"
gate_step 'doctor text ready' -- "$binary" doctor
gate_doctor_text_check "$GATE_ATTACH/logs/doctor-text-ready.log" fix-claude-code
[ "$GATE_FAILED" = "$before" ] || exit 1
port=$(gate_server_port "$config"); port=${port:-8080}
[ "$(gate_daemon_state "$port" "$config")" = none ] || { gate_record 'no unowned daemon before supplemental start' fail '' '' '' 'a listener appeared'; exit 1; }
(cd "$work" && exec "$binary" dev) >"$GATE_ATTACH/logs/dev.log" 2>&1 &
dev_pid=$!
deadline=$((SECONDS + 600))
state=none
while [ "$SECONDS" -lt "$deadline" ]; do
  state=$(gate_daemon_state "$port" "$config")
  [ "$state" = current ] && break
  kill -0 "$dev_pid" 2>/dev/null || break
  sleep 1
done
[ "$state" = current ] || { gate_record 'owned supplemental daemon ready' fail '' '' '' "state $state"; exit 1; }
gate_record 'owned supplemental daemon ready' pass '' '' '' "port $port; exact installed binary started by this script"
command=$(node "$gate_commands_js" first "$site/index.html" 'axocoatl run fix-claude-code' 2>/dev/null || node "$gate_commands_js" first "$site/install/index.html" 'axocoatl run fix-claude-code') || exit 1
printf '%s\n' "$command" >"$GATE_ATTACH/published-command.txt"
status=0
for fixture in node-deps; do
  if [ "$(gate_sha256 "$config")" != "$config_sha" ]; then
    gate_record "configuration unchanged before $fixture" fail '' '' '' 'owner configuration changed; no further fixture admitted'
    exit 1
  fi
  gate_record "configuration unchanged before $fixture" pass '' '' '' "$config_sha"
  gate_run_fixture "$fixture" "$command" "$work/$fixture" "claude-$fixture" || status=1
done
exit "$status"
