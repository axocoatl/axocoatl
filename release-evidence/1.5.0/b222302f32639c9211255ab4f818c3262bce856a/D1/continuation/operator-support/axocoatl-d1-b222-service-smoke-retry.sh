#!/usr/bin/env bash
# One separately authorized retry of the preserved external429->502 sample failure.
set -uo pipefail
umask 077
[ "$(id -un)" = d1gate ] && [ "${AXO_D1_SERVICE_RETRY_APPROVED:-}" = 1 ] || exit 2
base=/home/d1gate/gate-assets/restart-b222302f-20261027
. "$base/candidate/source/scripts/release-gate/lib.sh"
export PATH="$HOME/.local/bin:$PATH"
commit=b222302f32639c9211255ab4f818c3262bce856a
[ "$(gate_sha256 "$HOME/.local/bin/axocoatl")" = 9e03af65d461cf23f8577bc3cee14667efc44db4726a8a3556dd3a5c6419c0b8 ] || gate_die 'candidate changed'
prior="$base/continuation-retry2-evidence/1.5.0/$commit/D1-continuation2.json"
[ "$(gate_sha256 "$prior")" = 5abf12710a74c5574720d8fd9a16b5535ee37fff750c04a973ea8e411adf5182 ] || gate_die 'preserved prior outcome changed'
evidence="$base/service-smoke-retry-evidence"
[ ! -e "$evidence" ] || gate_die 'retry evidence already exists; no repeat permitted'
config=$(gate_config_path)
[ "$(gate_run_model "$config" writer)" = openrouter:deepseek/deepseek-chat-v3.1 ] && [ "$(gate_run_model "$config" reviewer)" = openrouter:openai/gpt-oss-120b ] || gate_die 'models differ'
config_sha=$(gate_sha256 "$config")
port=$(gate_server_port "$config")
[ "$(gate_daemon_state "$port" "$config")" = none ] || gate_die 'an existing daemon would invalidate owned-service retry'
work=$(mktemp -d /tmp/axo-d1-service-smoke-retry.XXXXXX)
owned_service=0
cleanup() {
  if [ "$owned_service" = 1 ]; then
    axocoatl service stop >>"$work/cleanup.log" 2>&1 || true
    axocoatl service uninstall >>"$work/cleanup.log" 2>&1 || true
  fi
  gate_say "kept isolated service sample $work"
}
trap cleanup EXIT
trap 'exit 130' INT TERM
gate_begin "$evidence" 1.5.0 "$commit" D1-service-smoke-retry D
gate_secret_stored openrouter || gate_die 'existing key unavailable; authentication forbidden'
GATE_RUNNER='Linux D1 targeted service smoke retry; Codex operator, d1gate OS account'
GATE_AXOCOATL_VERSION=$(axocoatl --version)
export GATE_RUNNER GATE_AXOCOATL_VERSION
gate_host_json "$GATE_TMP/host.json"
cp "$prior" "$GATE_ATTACH/prior-failed-continuation.json"
cat >"$GATE_ATTACH/provenance.json" <<'JSON'
{"candidate_commit":"b222302f32639c9211255ab4f818c3262bce856a","operator":"Codex","os_account":"d1gate","fresh_install":false,"fresh_authentication":false,"retry_count":1,"retry_reason":"Prior DeepSeek service smoke failed after HTTP429 endpoint failover followed by streamedHTTP502 Network connection lost; original failed outcome preserved unchanged","configuration":"unchanged DeepSeekV3.1 writer / GPT-OSS120B required reviewer, existing OpenRouter secret and $2TOTAL cap","baseline_commit":"592c14cb256afd84c2a99eb0935a489a3097a6a5","prior_sample_run":"run-c0995674-0bc6-4e0d-8ed6-bdc04a976791","scope":"only failed sample command through actual systemd service; no repeated successful fixtures; stop/uninstall after attempt"}
JSON
printf '%s\n' "$work" >"$GATE_ATTACH/work-path.txt"
printf '%s\n' "$config_sha" >"$GATE_ATTACH/config-sha256-before.txt"
axocoatl connect openrouter --status >"$GATE_ATTACH/key-status-before.txt" 2>&1 || exit 2
cat "$GATE_ATTACH/key-status-before.txt"
source_repo=/tmp/axocoatl-first-run-51f45b29243c4f27b227488afebdb86c/repo
[ "$(git -C "$source_repo" rev-parse HEAD)" = 592c14cb256afd84c2a99eb0935a489a3097a6a5 ] || gate_die 'sample baseline changed'
git clone --no-hardlinks -q "$source_repo" "$work/repo" || exit 2
[ -z "$(git -C "$work/repo" status --porcelain)" ] || gate_die 'new sample not pristine'
gate_record 'new pristine copy of exact original sample' pass '' '' '' "baseline592c14cb256afd84c2a99eb0935a489a3097a6a5; no prior failed edits copied"
owned_service=1
if gate_step 'install unchanged systemd definition for isolated retry' -- axocoatl service install --config "$config" && gate_step 'start same systemd service' -- axocoatl service start; then
  state=$(gate_wait_daemon "$port" "$config" 300)
  gate_check 'current systemd daemon answers before retry' "state $state" -- test "$state" = current
  if [ "$state" = current ]; then
    gate_step 'one retry of exact changed-model sample command' -- axocoatl run fix --task 'Fix the bug: paging skips the last item' --repo "$work/repo" --receipt "$work/receipt.json" --record "$work/record.jsonl" --config "$config" --model writer=openrouter:deepseek/deepseek-chat-v3.1 --model reviewer=openrouter:openai/gpt-oss-120b
    gate_step 'sample receipt verifies against original repository' -- axocoatl receipt verify "$work/receipt.json" --key "$(axocoatl receipt key)" --repo "$work/repo"
    gate_step 'sample record verifies' -- axocoatl record verify "$work/record.jsonl"
    gate_step 'sample receipt matches record' -- axocoatl receipt verify "$work/receipt.json" --key "$(axocoatl receipt key)" --record "$work/record.jsonl"
    gate_markup_check service-smoke-retry "$work/repo" "$work/record.jsonl" "$work/receipt.json"
    cp "$work/receipt.json" "$work/record.jsonl" "$GATE_ATTACH/" 2>/dev/null || true
  fi
fi
if gate_service_end "$port"; then owned_service=0; fi
gate_check 'original configuration unchanged by targeted retry' '' -- test "$(gate_sha256 "$config")" = "$config_sha"
printf '%s\n' "$(gate_sha256 "$config")" >"$GATE_ATTACH/config-sha256-after.txt"
axocoatl connect openrouter --status >"$GATE_ATTACH/key-status-after.txt" 2>&1 || true
gate_tools_json "$GATE_TMP/tools.json"
gate_finish --json host="$GATE_TMP/host.json" --json tools="$GATE_TMP/tools.json"
exit $?
