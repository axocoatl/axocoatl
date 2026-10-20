#!/bin/bash
# Return the macOS gate account to a brand-new user's state before a Tier C
# run of the new-user gate (docs/RELEASE_GATE.md).
#
# It runs only as the configured gate account (GATE_ACCOUNT, default
# axo-newuser), never as root, and never as an account that owns the
# machine's Homebrew. It stops this account's axocoatl daemons and removes
# only this account's Axocoatl, Podman and Ollama state. It never touches the
# gate's signing key (~/.axocoatl-gate-signer, or $GATE_SIGNER_DIR), so the
# key listed in .axocoatl/gate-keys keeps signing. Homebrew packages are
# uninstalled only with --dedicated-machine, on a Mac that exists for the
# gate.
#
# Usage: reset-mac-account.sh [--keep-models] [--dedicated-machine]
#                             [--dry-run] [--yes]
#        reset-mac-account.sh --print-paths
#   --keep-models        keep ~/.ollama (saves the 33 GB download; the run's
#                        evidence then says "models cached")
#   --dedicated-machine  also `brew uninstall podman ollama`
#   --dry-run            print what would be removed, change nothing
#   --yes                do not ask for the account name to confirm
#   --print-paths        print the paths a reset removes under this HOME and
#                        exit; changes nothing and runs for any account
set -u

account=${GATE_ACCOUNT:-axo-newuser}
keep_models=0 dedicated=0 dry_run=0 assume_yes=0 print_paths=0
while [ $# -gt 0 ]; do
  case "$1" in
    --keep-models) keep_models=1 ;;
    --dedicated-machine) dedicated=1 ;;
    --dry-run) dry_run=1 ;;
    --yes) assume_yes=1 ;;
    --print-paths) print_paths=1 ;;
    -h | --help)
      sed -n '2,/^set -u/p' "$0" | sed -e '$d' -e 's/^# \{0,1\}//'
      exit 2
      ;;
    *)
      echo "reset-mac-account: unknown argument $1" >&2
      exit 2
      ;;
  esac
  shift
done

# Everything of Axocoatl, Podman and the gate's own work in this HOME. The
# gate's signing key is not here: it lives in ~/.axocoatl-gate-signer.
reset_paths() {
  printf '%s\n' \
    "$HOME/Library/Application Support/Axocoatl" \
    "$HOME/Library/Logs/Axocoatl" \
    "$HOME/.axocoatl" \
    "$HOME/.config/axocoatl" \
    "$HOME/.local/share/axocoatl" \
    "$HOME/.local/state/axocoatl" \
    "$HOME/.config/containers" \
    "$HOME/.local/share/containers" \
    "$HOME/.local/bin/axocoatl" \
    "$HOME/axocoatl-gate-work"
}

if [ "$print_paths" = 1 ]; then
  reset_paths
  exit 0
fi

refuse() {
  echo "reset-mac-account: refusing: $*" >&2
  exit 1
}

[ "$(uname -s)" = Darwin ] || refuse "this resets a macOS gate account; this is $(uname -s)"
[ "$(id -u)" != 0 ] || refuse "never run as root"
me=$(id -un)
[ "$me" = "$account" ] || refuse "this is account '$me', not the gate account '$account' (set GATE_ACCOUNT to change which account is the gate account)"
expected_home=$(dscl . -read "/Users/$me" NFSHomeDirectory 2>/dev/null | awk '{ print $2 }')
[ -n "$expected_home" ] && [ "$HOME" = "$expected_home" ] \
  || refuse "HOME is '$HOME', but $me's home is '${expected_home:-unknown}'"
for brew_root in /opt/homebrew /usr/local/Homebrew; do
  if [ -d "$brew_root" ] && [ "$(stat -f %Su "$brew_root")" = "$me" ] && [ "$dedicated" = 0 ]; then
    refuse "$me owns $brew_root, so this is not a separate gate account; on a Mac that exists only for the gate, pass --dedicated-machine"
  fi
done

# This account's running daemons (`axocoatl dev` or `serve`), which hold its
# socket under ~/.axocoatl/run and its data root open.
daemons=$(pgrep -u "$(id -u)" -f 'axocoatl (dev|serve)' 2>/dev/null | tr '\n' ' ')

actions=()
add() { actions+=("$*"); }
[ -n "$daemons" ] && add "stop this account's axocoatl dev/serve (pids ${daemons% })"
for plist in "$HOME"/Library/LaunchAgents/ai.axocoatl.*.plist; do
  [ -e "$plist" ] || continue
  label=$(basename "$plist" .plist)
  add "launchctl bootout gui/$(id -u)/$label"
  add "rm -f $plist"
done
if command -v podman >/dev/null 2>&1; then
  while IFS= read -r machine; do
    [ -n "$machine" ] && add "podman machine rm -f $machine"
  done < <(podman machine list --format '{{.Name}}' 2>/dev/null | tr -d '*')
fi
while IFS= read -r path; do
  [ -e "$path" ] && add "rm -rf $path"
done < <(reset_paths)
if [ "$keep_models" = 0 ]; then
  [ -e "$HOME/.ollama" ] && add "rm -rf $HOME/.ollama"
fi
if [ "$dedicated" = 1 ] && command -v brew >/dev/null 2>&1; then
  for formula in podman ollama; do
    brew list --formula "$formula" >/dev/null 2>&1 && add "brew services stop $formula" && add "brew uninstall $formula"
  done
fi

echo "Gate account $me: reset plan"
if [ ${#actions[@]} -eq 0 ]; then
  echo "  (nothing to remove: the account is already new)"
fi
for action in "${actions[@]+"${actions[@]}"}"; do
  echo "  $action"
done
[ "$keep_models" = 1 ] && echo "  (keeping ~/.ollama: the next run's evidence records 'models cached')"
echo "  (keeping the gate's signing key in ${GATE_SIGNER_DIR:-$HOME/.axocoatl-gate-signer})"
[ "$dry_run" = 1 ] && exit 0
[ ${#actions[@]} -gt 0 ] || exit 0
if [ "$assume_yes" = 0 ]; then
  printf 'Type the account name (%s) to reset it: ' "$me"
  read -r answer </dev/tty || refuse "no answer"
  [ "$answer" = "$me" ] || refuse "the answer was not $me"
fi

failed=0
run() {
  echo "+ $*"
  "$@" || { echo "reset-mac-account: failed: $*" >&2; failed=1; }
}
if [ -n "$daemons" ]; then
  run pkill -INT -u "$(id -u)" -f 'axocoatl (dev|serve)'
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    pgrep -u "$(id -u)" -f 'axocoatl (dev|serve)' >/dev/null 2>&1 || break
    sleep 1
  done
  if pgrep -u "$(id -u)" -f 'axocoatl (dev|serve)' >/dev/null 2>&1; then
    echo "reset-mac-account: an axocoatl daemon of $me is still running; stop it and run the reset again" >&2
    exit 1
  fi
fi
for plist in "$HOME"/Library/LaunchAgents/ai.axocoatl.*.plist; do
  [ -e "$plist" ] || continue
  launchctl bootout "gui/$(id -u)/$(basename "$plist" .plist)" 2>/dev/null
  run rm -f "$plist"
done
if command -v podman >/dev/null 2>&1; then
  while IFS= read -r machine; do
    [ -n "$machine" ] && run podman machine rm -f "$machine"
  done < <(podman machine list --format '{{.Name}}' 2>/dev/null | tr -d '*')
fi
while IFS= read -r path; do
  [ -e "$path" ] || continue
  chmod -R u+w "$path" 2>/dev/null
  run rm -rf "$path"
done < <(reset_paths)
if [ "$keep_models" = 0 ] && [ -e "$HOME/.ollama" ]; then
  run rm -rf "$HOME/.ollama"
fi
if [ "$dedicated" = 1 ] && command -v brew >/dev/null 2>&1; then
  for formula in podman ollama; do
    if brew list --formula "$formula" >/dev/null 2>&1; then
      brew services stop "$formula" >/dev/null 2>&1
      run brew uninstall "$formula"
    fi
  done
fi
{
  date -u +%Y-%m-%dT%H:%M:%SZ
  echo "models_cached=$keep_models dedicated_machine=$dedicated failed=$failed"
} >"$HOME/.axocoatl-gate-reset"
[ "$failed" = 0 ] && echo "Gate account $me is reset." || echo "Gate account $me: some removals failed (above)." >&2
exit "$failed"
