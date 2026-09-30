set -eu
export LC_ALL=C GIT_OPTIONAL_LOCKS=0
# The host prefixes one of three modes. observe: report the tree and patch.
# keep: also keep the complete manifest a write-scope judgement compares in a
# new directory under /tmp. compare: verify that kept manifest against the
# digest the host recorded, then report every path whose entry changed.
case $capture_mode in
  observe | keep) ;;
  compare)
    case $baseline in
      /tmp/axocoatl-baseline.*/* | *..*) exit 64 ;;
      /tmp/axocoatl-baseline.?*) ;;
      *) exit 64 ;;
    esac
    case $expected in
      *[!0-9a-f]*) exit 64 ;;
    esac
    [ "${#expected}" -eq 64 ] || exit 64
    ;;
  *) exit 64 ;;
esac
# Every capture is additionally owned by the existing 180-second supervisor.
# Limit temporary artifact growth (512-byte blocks) independently from the
# tool's output bound.
ulimit -f 131072
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
git_dir=$(cd "$(git rev-parse --git-dir)" && pwd -P)
common_dir=$(cd "$(git rev-parse --git-common-dir)" && pwd -P)
AXO_SNAPSHOT_SCRATCH=$scratch
export AXO_SNAPSHOT_SCRATCH
head_before=$(git rev-parse --verify HEAD 2>/dev/null || printf unborn)
# One line per record: the path, then its mode, kind and content digest as
# read from the working tree itself, then what the record is: the index
# entry (flags, mode, object and stage) of a tracked path, "?" for an
# untracked path, "!" for an ignore file Git reads although it is ignored
# itself, and "git" for Git's own settings, hooks and exclude files.
records() {
  xargs -0 -r sh -c '
    set -eu
    tab=$(printf "\t")
    source=$1
    shift
    for record do
      case $source in
        index) meta=${record%%"$tab"*}; path=${record#*"$tab"}; name=$path ;;
        untracked) meta="?"; path=$record; name=$path ;;
        ignored)
          case $record in
            .gitignore | */.gitignore) ;;
            *) continue ;;
          esac
          meta="!"; path=$record; name=$path ;;
        git) meta=git; path=$AXO_GIT_BASE/$record; name=.git/$record ;;
      esac
      encoded=$(printf %s "$name" | base64 | tr -d "\n")
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
      printf "%s\t%s\t%s\t%s\t%s\n" "$encoded" "$mode" "$kind" "${digest%% *}" "$meta"
    done
  ' sh "$1"
}
# Every tracked and untracked path, whatever the index's flags say about it.
entries() {
  git ls-files -z --stage -v > "$scratch/index"
  git ls-files -z --others --exclude-standard > "$scratch/others"
  {
    records index < "$scratch/index"
    records untracked < "$scratch/others"
  } | sort -u
}
# The candidate's tree: each path's mode, kind and content digest.
tree() {
  cut -f 1-4 "$1" | sort -u
}
# Git's own settings, hooks and exclude files.
git_files() {
  base=$1
  shift
  set --
  for item in config config.worktree hooks info; do
    if [ -e "$base/$item" ] || [ -L "$base/$item" ]; then
      set -- "$@" "$item"
    fi
  done
  if [ "$#" -gt 0 ]; then
    (cd "$base" && find "$@" ! -type d -print0)
  fi
}
# What a write-scope judgement compares: every entry, each ignore file Git
# reads, and Git's own files. Ignored paths themselves are left out.
judged() {
  git ls-files -z --others --ignored --exclude-standard --directory > "$scratch/ignored"
  git_files "$common_dir" > "$scratch/git-common"
  {
    cat "$1"
    records ignored < "$scratch/ignored"
    AXO_GIT_BASE=$common_dir
    export AXO_GIT_BASE
    records git < "$scratch/git-common"
    if [ "$git_dir" != "$common_dir" ]; then
      git_files "$git_dir" > "$scratch/git-worktree"
      AXO_GIT_BASE=$git_dir
      records git < "$scratch/git-worktree"
    fi
  } | sort -u
}
entries > "$scratch/entries-before"
tree "$scratch/entries-before" > "$scratch/before"
if [ "$capture_mode" != observe ]; then
  judged "$scratch/entries-before" > "$scratch/judged-before"
fi
# The patch is taken against a fresh index of HEAD alone, so flags the
# repository's own index sets on a path cannot hide its change.
if [ "$head_before" = unborn ]; then
  GIT_INDEX_FILE=$scratch/patch-index git read-tree --empty
  : > "$scratch/patch"
else
  GIT_INDEX_FILE=$scratch/patch-index git read-tree "$head_before"
  GIT_INDEX_FILE=$scratch/patch-index \
    git diff --no-ext-diff --no-textconv --binary "$head_before" -- > "$scratch/patch"
fi
# Preserve untracked additions in the protected patch too. Paths are arguments,
# never shell source.
GIT_INDEX_FILE=$scratch/patch-index \
  git ls-files --others --exclude-standard -z > "$scratch/untracked"
xargs -0 -r sh -c '
  for path do
    git diff --no-ext-diff --no-textconv --binary --no-index -- /dev/null "$path"
    code=$?
    [ "$code" -le 1 ] || exit "$code"
  done
' sh < "$scratch/untracked" >> "$scratch/patch"
entries > "$scratch/entries-after"
tree "$scratch/entries-after" > "$scratch/after"
cmp "$scratch/before" "$scratch/after"
if [ "$capture_mode" != observe ]; then
  judged "$scratch/entries-after" > "$scratch/judged-after"
  cmp "$scratch/judged-before" "$scratch/judged-after"
fi
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
case $capture_mode in
  keep)
    judged=$(sha256sum < "$scratch/judged-before")
    kept=$(mktemp -d /tmp/axocoatl-baseline.XXXXXX)
    cp "$scratch/judged-before" "$kept/manifest"
    printf 'judged=%s\nbaseline=%s\n' "${judged%% *}" "$kept"
    ;;
  compare)
    judged=$(sha256sum < "$scratch/judged-before")
    # An Agent's shell can reach the kept manifest; only the exact bytes the
    # host recorded the digest of are compared.
    if [ ! -f "$baseline/manifest" ] || [ -L "$baseline/manifest" ]; then
      echo "the kept Before manifest is missing" >&2
      exit 66
    fi
    cp "$baseline/manifest" "$scratch/baseline"
    kept=$(sha256sum < "$scratch/baseline")
    if [ "${kept%% *}" != "$expected" ]; then
      echo "the kept Before manifest differs from the one recorded" >&2
      exit 66
    fi
    sort "$scratch/baseline" "$scratch/judged-before" | uniq -u | cut -f 1 | sort -u > "$scratch/changed"
    printf 'judged=%s\ncompared=%s\nchanged=' "${judged%% *}" "$expected"
    tr '\n' ',' < "$scratch/changed" | sed 's/,$//'
    printf '\n'
    rm -rf -- "$baseline"
    ;;
esac
