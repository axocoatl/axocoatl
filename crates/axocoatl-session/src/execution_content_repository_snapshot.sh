set -eu
export LC_ALL=C GIT_OPTIONAL_LOCKS=0
# Every capture is additionally owned by the existing 180-second supervisor.
# Limit temporary artifact growth independently from the tool's output bound.
ulimit -f 1024
scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT HUP INT TERM
# The host has already authorized this exact working root. Different sandbox
# users may share it; trust only this path for these read-only Git commands.
capture_root=$(pwd -P)
# Git reads no configuration, attributes or ignore file from outside the
# repository: not the system's, and not the home directory's, which an
# Agent's shell can write.
mkdir "$scratch/home"
HOME=$scratch/home
XDG_CONFIG_HOME=$scratch/home/.config
GIT_CONFIG_NOSYSTEM=1
GIT_CONFIG_GLOBAL=/dev/null
GIT_ATTR_NOSYSTEM=1
GIT_NO_LAZY_FETCH=1
GIT_TERMINAL_PROMPT=0
export HOME XDG_CONFIG_HOME GIT_CONFIG_NOSYSTEM GIT_CONFIG_GLOBAL GIT_ATTR_NOSYSTEM \
  GIT_NO_LAZY_FETCH GIT_TERMINAL_PROMPT
# Settings given this way outrank the repository's own configuration in every
# Git process below, so nothing configured there runs a program, reaches a
# remote, hides a path or changes the patch format.
GIT_CONFIG_COUNT=0
export GIT_CONFIG_COUNT
git_setting() {
  export "GIT_CONFIG_KEY_$GIT_CONFIG_COUNT=$1" "GIT_CONFIG_VALUE_$GIT_CONFIG_COUNT=$2"
  GIT_CONFIG_COUNT=$((GIT_CONFIG_COUNT + 1))
}
git_setting safe.directory "$capture_root"
git_setting core.fsmonitor false
git_setting core.untrackedCache false
git_setting core.hooksPath /dev/null
git_setting core.excludesFile /dev/null
git_setting core.attributesFile /dev/null
git_setting core.ignoreCase false
git_setting core.sparseCheckout false
git_setting index.sparse false
git_setting protocol.allow never
git_setting color.ui never
git_setting color.diff never
git_setting diff.noprefix false
git_setting diff.mnemonicPrefix false
git_setting diff.srcPrefix a/
git_setting diff.dstPrefix b/
git_setting diff.relative false
git_setting diff.orderFile /dev/null
# Content filters are programs the repository's configuration names; each one
# it defines is switched off. External diff and text conversion are refused
# on every diff below.
git config --name-only --get-regexp '^filter\.' > "$scratch/filters" || [ "$?" -eq 1 ]
while IFS= read -r key; do
  case $key in
    filter.*.clean | filter.*.smudge | filter.*.process) git_setting "$key" "" ;;
    filter.*.required) git_setting "$key" false ;;
  esac
done < "$scratch/filters"
root=$(git rev-parse --show-toplevel)
[ "$root" = "$capture_root" ]
cd "$root"
AXO_SNAPSHOT_SCRATCH=$scratch
export AXO_SNAPSHOT_SCRATCH
head_before=$(git rev-parse --verify HEAD 2>/dev/null || printf unborn)
manifest() {
  git ls-files --cached --others --exclude-standard -z > "$scratch/paths"
  xargs -0 -r sh -c '
    set -eu
    for path do
      encoded=$(printf %s "$path" | base64 | tr -d "\n")
      if [ -L "$path" ]; then
        readlink -n -- "$path" > "$AXO_SNAPSHOT_SCRATCH/link"
        digest=$(sha256sum < "$AXO_SNAPSHOT_SCRATCH/link"); kind=link
        mode=$(stat -c %a -- "$path")
      elif [ -f "$path" ]; then
        digest=$(sha256sum < "$path"); kind=file
        mode=$(stat -c %a -- "$path")
      elif [ ! -e "$path" ]; then
        digest=missing; kind=deleted; mode=0
      else
        # A submodule or special file requires its own checked identity.
        exit 65
      fi
      printf "%s\t%s\t%s\t%s\n" "$encoded" "$mode" "$kind" "${digest%% *}"
    done
  ' sh < "$scratch/paths" > "$scratch/manifest-raw"
  sort -u "$scratch/manifest-raw"
}
manifest > "$scratch/before"
if [ "$head_before" = unborn ]; then
  git diff --cached --no-ext-diff --no-textconv --binary > "$scratch/patch"
  git diff --no-ext-diff --no-textconv --binary >> "$scratch/patch"
else
  git diff --no-ext-diff --no-textconv --binary "$head_before" -- > "$scratch/patch"
fi
# Preserve untracked additions in the protected patch too. Paths are arguments,
# never shell source.
git ls-files --others --exclude-standard -z > "$scratch/untracked"
xargs -0 -r sh -c '
  for path do
    git diff --no-ext-diff --no-textconv --binary --no-index -- /dev/null "$path"
    code=$?
    [ "$code" -le 1 ] || exit "$code"
  done
' sh < "$scratch/untracked" >> "$scratch/patch"
manifest > "$scratch/after"
cmp "$scratch/before" "$scratch/after"
head_after=$(git rev-parse --verify HEAD 2>/dev/null || printf unborn)
[ "$head_before" = "$head_after" ]
tree=$(sha256sum < "$scratch/before")
patch=$(sha256sum < "$scratch/patch")
printf 'format=1\nhead=%s\ntree=%s\n' "$head_before" "${tree%% *}"
printf 'manifest_bytes=%s\nmanifest_b64=' "$(wc -c < "$scratch/before" | tr -d ' ')"
head -c 8192 "$scratch/before" | base64 | tr -d '\n'
printf '\npatch_sha256=%s\npatch_bytes=%s\npatch_b64=' "${patch%% *}" "$(wc -c < "$scratch/patch" | tr -d ' ')"
base64 < "$scratch/patch" | tr -d '\n'
printf '\n'
