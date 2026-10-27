#!/bin/bash
set -u
umask 077
[ "${1:-}" = --run ] && [ "$#" = 1 ] || exit 2
repo=/Users/erick/.codex/worktrees/release-1-5-recovery/axocoatl
commit=b222302f32639c9211255ab4f818c3262bce856a
binary=/usr/local/bin/axocoatl
binary_sha=d25cd64f70da4d17eb781d223aeab553f4180d074dbb29af302b23492ce31d1b
archive_sha=2a9fc208948c630efab48d2f58875eb0c8f6dd24baec005479f691bdc532d659
candidate="$repo/target/recovery-2026-10-25/candidate-b222302f"
evidence_root="$repo/target/recovery-2026-10-25/provider-continuation-b222302f/openrouter-fixtures-retry1"
work="$HOME/axocoatl-gate-work/provider-continuation-b222302f-openrouter-fixtures-retry1"
path_name=supplemental-openrouter-three-fixtures
self=/private/tmp/axo-b222-openrouter-three-fixtures.sh
export PATH=/Users/erick/.local/share/fnm/node-versions/v22.23.2/installation/bin:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin
unset AXOCOATL_TOKEN GATE_SIMULATION
export AXOCOATL_DATA_DIR="$HOME/Library/Application Support/Axocoatl/data"
export AXOCOATL_URL=http://127.0.0.1:18080
. "$repo/scripts/release-gate/lib.sh"
[ -t 0 ] && [ -t 1 ] || gate_die 'owned PTY required'
[ "$(gate_sha256 "$binary")" = "$binary_sha" ] || gate_die 'wrong candidate binary'
[ "$(gate_sha256 "$candidate/axocoatl-v1.5.0-aarch64-apple-darwin.tar.gz")" = "$archive_sha" ] || gate_die 'wrong candidate archive'
git -C "$repo" diff --quiet "$commit" -- scripts/release-gate/lib.sh scripts/release-gate/newuser-mac.sh tests/fixtures/newuser/node-deps tests/fixtures/newuser/python-pytest tests/fixtures/newuser/fixtures.json || gate_die 'gate inputs changed'
config="$HOME/Library/Application Support/Axocoatl/config.yaml"
config_sha=$(gate_sha256 "$config")
[ "$(gate_daemon_state 18080 "$config")" = none ] || gate_die 'port18080 is not free; do not disturb its owner'
if [ -z "${AXO_OR_SUPPLEMENT_IN_SCRIPT:-}" ]; then
 [ ! -e "$evidence_root" ] && [ ! -e "$work" ] || gate_die 'new output paths required'
 mkdir "$evidence_root" || exit 2
 AXO_OR_SUPPLEMENT_IN_SCRIPT="$evidence_root/transcript.txt" exec script -q -t 0 "$evidence_root/transcript.txt" /bin/bash "$self" --run
fi
transcript=$AXO_OR_SUPPLEMENT_IN_SCRIPT
[ "$transcript" = "$evidence_root/transcript.txt" ] || gate_die 'wrong transcript'
mkdir "$work" || exit 2
gate_begin "$evidence_root" 1.5.0 "$commit" "$path_name" C
cp "$self" "$GATE_ATTACH/runner.sh"
gate_host_json "$GATE_TMP/host.json"
dev_pid=''
finish() {
 local result=$? waited=0 daemon_status=0 final_result
 trap - EXIT
 if [ -n "$dev_pid" ]; then
  kill -INT "$dev_pid" 2>/dev/null || true
  while kill -0 "$dev_pid" 2>/dev/null && [ "$waited" -lt 180 ]; do sleep 1; waited=$((waited+1)); done
  if kill -0 "$dev_pid" 2>/dev/null; then gate_record 'owned isolated Mac daemon stopped' fail '' '' '' 'did not settle after SIGINT; retained for inspection'
  else wait "$dev_pid" || daemon_status=$?; gate_check 'owned isolated Mac daemon stopped with exit0' "exit $daemon_status" -- test "$daemon_status" = 0; fi
 fi
 gate_check 'global owner configuration unchanged' 'owner configuration changed' -- test "$(gate_sha256 "$config")" = "$config_sha"
 GATE_AXOCOATL_VERSION=$($binary --version); export GATE_AXOCOATL_VERSION
 gate_tools_json "$GATE_TMP/tools.json"
 cp "$transcript" "$GATE_ATTACH/transcript.txt"
 gate_finish --json host="$GATE_TMP/host.json" --json tools="$GATE_TMP/tools.json"; final_result=$?
 [ "$result" = 0 ] || exit "$result"
 exit "$final_result"
}
trap finish EXIT
snapshot="$work/config.port18080.yaml"
python3 - "$config" "$snapshot" "$GATE_ATTACH/provenance.json" "$commit" "$binary_sha" "$archive_sha" <<'PY'
import sys,pathlib,re,json,hashlib,os
src,dst,out,commit,binary,archive=sys.argv[1:]
s=pathlib.Path(src).read_text();n,count=re.subn(r'(?m)^(  port: )8080$',r'\g<1>18080',s);assert count==1 and n.replace('  port: 18080','  port: 8080')==s
pathlib.Path(dst).write_text(n);os.chmod(dst,0o400)
json.dump({'schema':'axocoatl.recovery.isolated-port-fixtures/1','candidate_commit':commit,'binary_sha256':binary,'archive_sha256':archive,'operator':'Codex assistant through owned PTY','human_operator':False,'fresh_authentication':False,'onboarding':False,'global_configuration_mutated':False,'owner_configuration_sha256':hashlib.sha256(s.encode()).hexdigest(),'execution_configuration_sha256':hashlib.sha256(n.encode()).hexdigest(),'configuration_only_difference':{'server.port':{'from':8080,'to':18080}},'data_root':os.environ['AXOCOATL_DATA_DIR'],'client_url':os.environ['AXOCOATL_URL'],'authentication_disabled':False,'intervention':'Mac8080 was LimaD1 forwarding, causingMacAPI token401 beforeadmission. OwnedMacdaemon is isolatedon18080; exactpublishedbasecommand gets explicit--config snapshot. Linux and originalfailedgate untouched.'},open(out,'w'),indent=2)
PY
[ "$?" = 0 ] || exit 2
gate_secret_stored openrouter && gate_secret_stored claude-code-oauth || { gate_record 'existing provider secrets available' fail '' '' '' 'required existing secret absent; no auth started'; exit 1; }
writer=$(gate_run_model "$snapshot" writer); reviewer=$(gate_run_model "$snapshot" reviewer)
[ "$writer" = openrouter:qwen/qwen3-coder-30b-a3b-instruct ] && [ "$reviewer" = openrouter:openai/gpt-oss-120b ] || { gate_record 'expected OpenRouter models' fail '' '' '' "writer $writer reviewer $reviewer"; exit 1; }
gate_record 'exact candidate and existing provider models verified' pass '' '' '' "writer $writer; reviewer $reviewer"
gate_step 'capped OpenRouter credit status before fixtures' -- "$binary" connect openrouter --status --config "$snapshot" || exit 1
python3 - "$GATE_ATTACH/logs/capped-openrouter-credit-status-before-fixtures.log" <<'PY'
import sys,re
s=open(sys.argv[1]).read();m=re.search(r'Credit limit: \$([0-9.]+) on this key',s);assert m and 0<float(m[1])<=0.90 and '(resets ' not in s
PY
[ "$?" = 0 ] || { gate_record 'OpenRouter cap at most0.90 nonrenewing' fail '' '' '' 'could not verify cap'; exit 1; }
gate_record 'OpenRouter cap at most0.90 nonrenewing' pass '' '' '' 'actualCLI status verified before inference'
(cd "$work" && exec "$binary" dev --config "$snapshot") >"$GATE_ATTACH/logs/dev.log" 2>&1 &
dev_pid=$!
deadline=$((SECONDS+300)); state=none
while [ "$SECONDS" -lt "$deadline" ]; do state=$(gate_daemon_state 18080 "$snapshot"); [ "$state" = current ] && break; kill -0 "$dev_pid" 2>/dev/null || break; sleep 1; done
[ "$state" = current ] || { gate_record 'owned Mac daemon healthy on18080' fail '' '' '' "state $state"; exit 1; }
/usr/sbin/lsof -nP -a -p "$dev_pid" -iTCP:18080 -sTCP:LISTEN >"$GATE_ATTACH/owned-listener.txt" || { gate_record 'listener belongs to owned Mac daemon' fail '' '' '' 'PID does not own18080'; exit 1; }
gate_record 'listener belongs to owned Mac daemon' pass '' '' '' "PID $dev_pid; localhost18080; authenticatedclients use realMacdata root"
status=0
base='axocoatl run fix --task "Paging skips the last item when the total is a multiple of the page size"'
quoted_config=$(python3 -c 'import shlex,sys;print(shlex.quote(sys.argv[1]))' "$snapshot")
for fixture in node-deps python-pytest; do
 [ "$(gate_sha256 "$config")" = "$config_sha" ] || exit 1
 gate_run_fixture "$fixture" "$base --config $quoted_config" "$work/$fixture" "openrouter-$fixture" || status=1
done
base="axocoatl run fix-claude-code --task \"Paging skips the last item when the total is a multiple of the page size\" --model reviewer=$reviewer"
[ "$(gate_sha256 "$config")" = "$config_sha" ] || exit 1
gate_run_fixture node-deps "$base --config $quoted_config" "$work/claude-node-deps" claude-openrouter-node-deps || status=1
gate_step 'capped OpenRouter credit status after fixtures' -- "$binary" connect openrouter --status --config "$snapshot" || status=1
exit "$status"
