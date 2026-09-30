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
git_safe() { git -c safe.directory="$capture_root" -c core.fsmonitor=false -c core.untrackedCache=false "$@"; }
root=$(git_safe rev-parse --show-toplevel)
[ "$root" = "$(pwd -P)" ]
cd "$root"
AXO_SNAPSHOT_SCRATCH=$scratch
export AXO_SNAPSHOT_SCRATCH
head_before=$(git_safe rev-parse --verify HEAD 2>/dev/null || printf unborn)
manifest() {
  git_safe ls-files --cached --others --exclude-standard -z > "$scratch/paths"
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
  git_safe diff --cached --no-ext-diff --no-textconv --binary > "$scratch/patch"
  git_safe diff --no-ext-diff --no-textconv --binary >> "$scratch/patch"
else
  git_safe diff --no-ext-diff --no-textconv --binary "$head_before" -- > "$scratch/patch"
fi
# Preserve untracked additions in the protected patch too. Paths are arguments,
# never shell source, and no external diff driver or text conversion is run.
git_safe ls-files --others --exclude-standard -z > "$scratch/untracked"
xargs -0 -r sh -c '
  for path do
    git -c core.fsmonitor=false diff --no-ext-diff --no-textconv --binary --no-index -- /dev/null "$path"
    code=$?
    [ "$code" -le 1 ] || exit "$code"
  done
' sh < "$scratch/untracked" >> "$scratch/patch"
manifest > "$scratch/after"
cmp "$scratch/before" "$scratch/after"
head_after=$(git_safe rev-parse --verify HEAD 2>/dev/null || printf unborn)
[ "$head_before" = "$head_after" ]
tree=$(sha256sum < "$scratch/before")
patch=$(sha256sum < "$scratch/patch")
printf 'format=1\nhead=%s\ntree=%s\n' "$head_before" "${tree%% *}"
printf 'manifest_bytes=%s\nmanifest_b64=' "$(wc -c < "$scratch/before" | tr -d ' ')"
head -c 8192 "$scratch/before" | base64 | tr -d '\n'
printf '\npatch_sha256=%s\npatch_bytes=%s\npatch_b64=' "${patch%% *}" "$(wc -c < "$scratch/patch" | tr -d ' ')"
base64 < "$scratch/patch" | tr -d '\n'
printf '\n'
