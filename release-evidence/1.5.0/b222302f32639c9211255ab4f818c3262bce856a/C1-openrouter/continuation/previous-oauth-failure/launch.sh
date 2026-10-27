#!/bin/bash
set -eu
umask 077
if [ "$#" != 2 ] || { [ "$1" != --run ] && [ "$1" != --check ]; }; then
  echo "Usage: /bin/bash $0 --run|--check openrouter|codex" >&2
  exit 2
fi
action=$1
mode=$2
case "$mode" in openrouter) secret=openrouter ;; codex) secret=codex-chatgpt ;; *) exit 2 ;; esac
base=/private/tmp/axocoatl-b222-provider-continuation-20261027
repo=/Users/erick/.codex/worktrees/release-1-5-recovery/axocoatl
candidate="$repo/target/recovery-2026-10-25/candidate-b222302f"
commit=b222302f32639c9211255ab4f818c3262bce856a
evidence="$repo/target/recovery-2026-10-25/provider-continuation-b222302f/$mode"
work="$HOME/axocoatl-gate-work/provider-continuation-b222302f-$mode"
export PATH="$base/bin:/Users/erick/.local/share/fnm/node-versions/v22.23.2/installation/bin:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
unset AXOCOATL_URL AXOCOATL_TOKEN AXOCOATL_DATA_DIR GATE_IN_SCRIPT GATE_SIMULATION
[ "$(id -un)" = erick ] && [ "$(uname -s)" = Darwin ] || { echo 'Requires reviewed erick Mac account' >&2; exit 2; }
[ -t 0 ] && [ -t 1 ] || { echo 'Launch in root-owned interactive PTY' >&2; exit 2; }
[ "$(command -v axocoatl)" = /usr/local/bin/axocoatl ] || exit 2
python3 "$base/verify-inputs.py"
[ "$(axocoatl --version)" = 'axocoatl 1.5.0' ] || exit 2
# Name-only credential check; no secret contents are read or copied here.
. "$repo/scripts/release-gate/lib.sh"
GATE_TMP=$(mktemp -d "$base/preflight-$mode.XXXXXX")
export GATE_TMP
if gate_secret_stored "$secret"; then
  echo "Refusing: $secret is already stored. Preserve/remove it separately only with authorized root action before this fresh-sign-in path." >&2
  exit 2
else
  secret_status=$?
  [ "$secret_status" = 1 ] || { echo 'Credential-name listing failed; refusing' >&2; exit 2; }
fi
config=$(gate_config_path)
port=$(gate_server_port "$config"); port=${port:-8080}
[ "$(gate_daemon_state "$port" "$config")" = none ] || { echo 'An unowned daemon/listener remains; stop it explicitly before launching' >&2; exit 2; }
[ ! -e "$evidence" ] && [ ! -e "$work" ] || { echo 'Evidence or work directory already exists; preserve it, do not rerun this launcher' >&2; exit 2; }
if [ "$action" = --check ]; then
  echo "READ-ONLY PRECONDITIONS PASS: exact candidate; $secret absent; no daemon/listener; work/evidence destinations unused. No gate or authentication started."
  exit 0
fi
mkdir -p "${evidence%/*}"
mkdir "$evidence"
cp "$base/input-manifest.json" "$evidence/continuation-provenance.json"
cp "$base/launch.sh" "$base/newuser-mac-continuation.sh" "$base/verify-inputs.py" "$evidence/"
export AXO_CONTINUATION_WORK="$work"
export AXO_CONTINUATION_OPEN_LOG="$base/$mode-browser-launch-urls.private.txt"
[ ! -e "$AXO_CONTINUATION_OPEN_LOG" ] || { echo 'Private URL output already exists; refusing to reuse attempt' >&2; exit 2; }
: > "$AXO_CONTINUATION_OPEN_LOG"
chmod 600 "$AXO_CONTINUATION_OPEN_LOG"
export GATE_ACCOUNT=erick
echo "Continuing the preserved exact-candidate installation; fresh $mode sign-in checks are unchanged."
echo 'Automatic browser opening will fail deliberately; root opens the captured/printed launch URL in the dedicated window. User alone authenticates.'
echo "Private launch URL file: $AXO_CONTINUATION_OPEN_LOG"
echo 'Answer each OpenRouter extension prompt promptly to keep the same callback active. Do not attest unseen actions.'
exec /bin/bash "$base/newuser-mac-continuation.sh" --release-dir "$candidate" --site "$candidate/site" --machine C1 --paths "$mode" --commit "$commit" --evidence-root "$evidence"
