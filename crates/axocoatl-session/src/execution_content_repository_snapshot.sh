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
# tool's output bound. The patch is never written whole (below), so this
# limits only the listings, which grow with the number of paths: 1 GiB, far
# more than the listings of a repository the capture reads within its time.
# A write past it fails rather than ending the process that writes, so the
# capture fails, and says that a listing reached the limit (exit 69).
trap '' XFSZ
file_limit=2097152
ulimit -f "$file_limit"
scratch=$(mktemp -d)
# What the commands below write to standard error is bounded as what the
# capture prints is: Git can warn once for each file, and ten thousand such
# lines would pass the bound the host reads standard error within and fail
# the capture. Its first and last 8 KiB are passed on; the rest is read and
# dropped, so no command waits on it or fails writing it.
mkfifo "$scratch/stderr"
{ head -c 8192; tail -c 8192; } < "$scratch/stderr" >&2 &
stderr_drain=$!
exec 3>&2 2> "$scratch/stderr"
finish() {
  status=$?
  if [ -n "$stderr_drain" ]; then
    exec 2>&3 3>&-
    wait "$stderr_drain" || :
    stderr_drain=
  fi
  # A capture that failed with a listing at the size limit says so.
  if [ "$status" -ne 0 ] && [ -d "$scratch" ] &&
    [ -n "$(find "$scratch" -type f -size "+$((file_limit * 512 - 1))c")" ]; then
    rm -rf -- "$scratch"
    echo "a listing of the repository's files reached the 1 GiB a capture may write" >&2
    exit 69
  fi
  rm -rf -- "$scratch"
}
trap finish EXIT HUP INT TERM
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
git_setting diff.submodule short
git_setting submodule.recurse false
# Line endings are still converted as configured; only the warning Git writes
# for each file whose conversion would not round-trip is left out.
git_setting core.safecrlf false
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
# Each NUL-terminated name on the input, as `od -An -v -tu1` prints its
# bytes, in base64 on a line of its own, as base64 writes it unwrapped: one
# od and one awk for every name, where base64 for each one took three
# processes. With strip=1, a name's last "/" is left out: Git lists a nested
# checkout as its directory, "name/", which is recorded as "name".
cat > "$scratch/base64.awk" <<'EOF'
BEGIN {
  digits = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
  n = 0
}
{
  for (field = 1; field <= NF; field++) {
    if ($field == 0) {
      emit()
    } else {
      bytes[n] = $field + 0
      n++
    }
  }
}
END {
  if (n > 0) emit()
}
function emit(  i, a, b, c, out) {
  if (strip && n > 1 && bytes[n - 1] == 47) n--
  out = ""
  for (i = 0; i + 2 < n; i += 3) {
    a = bytes[i]; b = bytes[i + 1]; c = bytes[i + 2]
    out = out substr(digits, int(a / 4) + 1, 1)
    out = out substr(digits, (a % 4) * 16 + int(b / 16) + 1, 1)
    out = out substr(digits, (b % 16) * 4 + int(c / 64) + 1, 1)
    out = out substr(digits, c % 64 + 1, 1)
  }
  if (n - i == 1) {
    a = bytes[i]
    out = out substr(digits, int(a / 4) + 1, 1)
    out = out substr(digits, (a % 4) * 16 + 1, 1) "=="
  } else if (n - i == 2) {
    a = bytes[i]; b = bytes[i + 1]
    out = out substr(digits, int(a / 4) + 1, 1)
    out = out substr(digits, (a % 4) * 16 + int(b / 16) + 1, 1)
    out = out substr(digits, (b % 16) * 4 + 1, 1) "="
  }
  print out
  n = 0
}
EOF
# One batch's records, a kind and what the record is on each line of the
# input, joined in order with what was read for them: each name's base64,
# each file's digest from sha256sum's output, each mode from stat's, and
# each digest read alone. Anything out of step fails the capture. A
# special record's digest is a commit or "unread".
cat > "$scratch/records.awk" <<'EOF'
function next_of(list,  line) {
  if ((getline line < (batch "/" list)) <= 0) {
    failed = 1
    exit
  }
  return line
}
BEGIN {
  FS = "\t"
  failed = 0
}
{
  kind = $1
  meta = substr($0, length(kind) + 2)
  encoded = next_of("encoded")
  if (kind == "file") {
    digest = next_of("digests")
    if (substr(digest, 1, 1) == "\\") digest = substr(digest, 2)
    digest = substr(digest, 1, 64)
    mode = next_of("modes")
  } else if (kind == "alone" || kind == "link" || kind == "special") {
    digest = next_of("alone")
    mode = next_of("modes")
    if (kind == "alone") kind = "file"
  } else if (kind == "deleted") {
    digest = "missing"
    mode = "0"
  } else {
    failed = 1
    exit
  }
  if (kind == "special") {
    if (digest != "unread" && ((length(digest) != 40 && length(digest) != 64) || digest ~ /[^0-9a-f]/)) {
      failed = 1
      exit
    }
  } else if (kind != "deleted" && (length(digest) != 64 || digest ~ /[^0-9a-f]/)) {
    failed = 1
    exit
  }
  if (kind != "deleted" && mode !~ /^[0-7]+$/) {
    failed = 1
    exit
  }
  printf "%s\t%s\t%s\t%s\t%s\n", encoded, mode, kind, digest, meta
}
END {
  if (!failed && ((getline line < (batch "/encoded")) > 0 || (getline line < (batch "/digests")) > 0 || (getline line < (batch "/modes")) > 0 || (getline line < (batch "/alone")) > 0)) failed = 1
  if (failed) exit 70
}
EOF
# The patch section of a root file named "-", which Git diffs as "./-",
# with the names Git gives every other file's: "a/-" and "b/-" in its
# header, the lines before its first hunk or binary data.
cat > "$scratch/dash.awk" <<'EOF'
!body && (/^@@ / || $0 == "GIT binary patch" || /^Binary files /) { body = 1 }
!body && $0 == "diff --git a/./- b/./-" { $0 = "diff --git a/- b/-" }
!body && $0 == "+++ b/./-" { $0 = "+++ b/-" }
{ print }
EOF
# The records of one batch of paths of one source, the arguments: one line
# per record, the path, then its mode, kind and content digest as read from
# the working tree itself, then what the record is: the index entry (flags,
# mode, object and stage) of a tracked path, "?" for an untracked path, "!"
# for an ignore file Git reads although it is ignored itself, and "git" for
# Git's own settings, hooks and exclude files.
#
# Each batch of paths xargs passes is read with one sha256sum and one stat
# for all of its files and its names encoded by one od and one awk, so a
# repository of tens of thousands of files is captured within the time a
# capture has: read one at a time, each file took six processes. A file
# whose name sha256sum would escape or split (a backslash or a newline in
# it) is read alone, as a symbolic link's target is. Operands are paths
# that start with "./" or "/", so no name is read as standard input or an
# option. A submodule or a nested checkout is one record of kind "special",
# whose files are not read: a submodule's digest is the commit it has
# checked out (commit.sh), or the one the index records when it has none
# Git can read, as Git itself compares it; a nested checkout's is "unread".
# Anything it cannot read fails it, and so the capture.
cat > "$scratch/records.sh" <<'EOF'
set -eu
tab=$(printf "\t")
newline="
"
source=$1
shift
batch=$AXO_SNAPSHOT_SCRATCH/batch.$$
rm -rf -- "$batch"
mkdir "$batch"
for list in kinds names hash stat alone; do
  : > "$batch/$list"
done
for record do
  case $source in
    index) meta=${record%%"$tab"*}; path=${record#*"$tab"}; name=$path ;;
    untracked) meta="?"; path=$record; name=${record%/} ;;
    ignored)
      case $record in
        .gitignore | */.gitignore) ;;
        *) continue ;;
      esac
      meta="!"; path=$record; name=$path ;;
    git) meta=git; path=$AXO_GIT_BASE/$record; name=.git/$record ;;
  esac
  case $path in
    /*) operand=$path ;;
    *) operand=./$path ;;
  esac
  if [ -L "$path" ]; then
    readlink -n -- "$path" > "$batch/link"
    digest=$(sha256sum < "$batch/link"); kind=link
    printf "%s\n" "${digest%% *}" >> "$batch/alone"
    printf "%s\0" "$operand" >> "$batch/stat"
  elif [ -f "$path" ]; then
    case $path in
      *"$newline"* | *\\*)
        digest=$(sha256sum < "$path"); kind=alone
        printf "%s\n" "${digest%% *}" >> "$batch/alone"
        ;;
      *)
        kind=file
        printf "%s\0" "$operand" >> "$batch/hash"
        ;;
    esac
    printf "%s\0" "$operand" >> "$batch/stat"
  elif [ ! -e "$path" ]; then
    kind=deleted
  elif [ -d "$path" ]; then
    case $source:$meta in
      # A nested checkout, which Git lists as its directory.
      untracked:*)
        kind=special
        printf "unread\n" >> "$batch/alone"
        printf "%s\0" "$operand" >> "$batch/stat"
        ;;
      # A submodule (a gitlink: flags, mode 160000, commit and stage).
      index:*" 160000 "*)
        kind=special
        commit=
        if [ -e "$path/.git" ] || [ -L "$path/.git" ]; then
          commit=$(sh "$AXO_SNAPSHOT_SCRATCH/commit.sh" "$operand") || commit=
        fi
        if [ -z "$commit" ]; then
          commit=${meta#* 160000 }
          commit=${commit%% *}
        fi
        printf "%s\n" "$commit" >> "$batch/alone"
        printf "%s\0" "$operand" >> "$batch/stat"
        ;;
      # A tracked file that is now a directory is gone; Git lists the
      # directory's own files apart.
      index:*) kind=deleted ;;
      *) continue ;;
    esac
  elif [ "$source" = index ]; then
    # A tracked file that is now a FIFO, a socket or a device, which hold
    # no content Git tracks, is gone as a file.
    kind=deleted
  else
    continue
  fi
  printf "%s\0" "$name" >> "$batch/names"
  printf "%s\t%s\n" "$kind" "$meta" >> "$batch/kinds"
done
xargs -0 -r sha256sum -- < "$batch/hash" > "$batch/digests"
xargs -0 -r stat -c %a -- < "$batch/stat" > "$batch/modes"
od -An -v -tu1 < "$batch/names" > "$batch/bytes"
awk -f "$AXO_SNAPSHOT_SCRATCH/base64.awk" < "$batch/bytes" > "$batch/encoded"
awk -v batch="$batch" -f "$AXO_SNAPSHOT_SCRATCH/records.awk" < "$batch/kinds"
rm -rf -- "$batch"
EOF
# Git's own settings, hooks and exclude files under the Git directory that
# is the argument, each name NUL-terminated.
cat > "$scratch/git-files.sh" <<'EOF'
set -eu
cd "$1"
set --
for item in config config.worktree hooks info; do
  if [ -e "$item" ] || [ -L "$item" ]; then
    set -- "$@" "$item"
  fi
done
if [ "$#" -gt 0 ]; then
  find "$@" ! -type d -print0
fi
EOF
# The commit the submodule at the argument has checked out, as Git reads it
# to compare a submodule: its own HEAD. It fails when there is none (an
# unborn HEAD, or a .git that names no repository: Git does not look above
# the submodule for one), and the record then takes the commit the index
# records, as Git does. Git reads the HEAD there and nothing else; nothing
# that checkout configures runs.
cat > "$scratch/commit.sh" <<'EOF'
set -eu
cd "$1"
dir=$(pwd -P)
GIT_CEILING_DIRECTORIES=$(dirname "$dir")
export GIT_CEILING_DIRECTORIES
export "GIT_CONFIG_KEY_$GIT_CONFIG_COUNT=safe.directory" \
  "GIT_CONFIG_VALUE_$GIT_CONFIG_COUNT=$dir"
GIT_CONFIG_COUNT=$((GIT_CONFIG_COUNT + 1))
export GIT_CONFIG_COUNT
commit=$(git rev-parse --verify -q HEAD 2> /dev/null)
printf '%s\n' "$commit"
EOF
# The records of the paths of one source (index, untracked, ignored or git)
# named on standard input. A file it cannot read fails the capture.
records() {
  xargs -0 -r sh "$scratch/records.sh" "$1" || {
    echo "the capture could not read every file it records" >&2
    exit 65
  }
}
# Every tracked and untracked path, whatever the index's flags say about it.
entries() {
  git ls-files -z --stage -v > "$scratch/index"
  git ls-files -z --others --exclude-standard > "$scratch/others"
  records index < "$scratch/index" > "$scratch/records"
  records untracked < "$scratch/others" >> "$scratch/records"
  sort -u "$scratch/records"
}
# The candidate's tree: each path's mode, kind and content digest.
tree() {
  cut -f 1-4 "$1" > "$scratch/cut"
  sort -u "$scratch/cut"
}
# What a write-scope judgement compares: every entry, each ignore file Git
# reads, and Git's own files. Ignored paths themselves are left out.
judged() {
  git ls-files -z --others --ignored --exclude-standard --directory > "$scratch/ignored"
  sh "$scratch/git-files.sh" "$common_dir" > "$scratch/git-common"
  cp "$1" "$scratch/judged"
  records ignored < "$scratch/ignored" >> "$scratch/judged"
  AXO_GIT_BASE=$common_dir
  export AXO_GIT_BASE
  records git < "$scratch/git-common" >> "$scratch/judged"
  if [ "$git_dir" != "$common_dir" ]; then
    sh "$scratch/git-files.sh" "$git_dir" > "$scratch/git-worktree"
    AXO_GIT_BASE=$git_dir
    records git < "$scratch/git-worktree" >> "$scratch/judged"
  fi
  sort -u "$scratch/judged"
}
entries > "$scratch/entries-before"
tree "$scratch/entries-before" > "$scratch/before"
if [ "$capture_mode" != observe ]; then
  judged "$scratch/entries-before" > "$scratch/judged-before"
fi
# The patch is taken against a fresh index of HEAD alone, so flags the
# repository's own index sets on a path cannot hide its change. A submodule
# is in it as Git shows one when only the commit it has checked out is
# compared: Git does not look inside it, where it would run what that
# checkout configures, and neither does its record.
if [ "$head_before" = unborn ]; then
  GIT_INDEX_FILE=$scratch/patch-index git read-tree --empty
else
  GIT_INDEX_FILE=$scratch/patch-index git read-tree "$head_before"
fi
GIT_INDEX_FILE=$scratch/patch-index \
  git ls-files --others --exclude-standard -z > "$scratch/untracked"
# The patch is read once, as Git writes it: its digest and size are taken as
# it passes, and only its first 512 KiB and one byte are kept, all of it this
# capture can print. So no change is too large to capture, whatever the size
# of its patch.
mkfifo "$scratch/patch-digest" "$scratch/patch-size"
sha256sum < "$scratch/patch-digest" > "$scratch/patch-sha256" &
digest_job=$!
wc -c < "$scratch/patch-size" > "$scratch/patch-bytes" &
size_job=$!
{
  code=0
  if [ "$head_before" != unborn ]; then
    GIT_INDEX_FILE=$scratch/patch-index \
      git diff --no-ext-diff --no-textconv --binary --ignore-submodules=dirty \
      "$head_before" -- || code=$?
  fi
  # Preserve untracked additions in the protected patch too. Paths are
  # arguments, never shell source. Git reads a path "-" as its standard
  # input, so a file of that name at the root is diffed as "./-" and its
  # section named as Git names any other file's (dash.awk). A nested
  # checkout, which Git lists as its directory, has no section: Git cannot
  # diff a directory, and its record says its files are unread.
  if [ "$code" -eq 0 ]; then
    xargs -0 -r sh -c '
      for path do
        case $path in
          */) continue ;;
        esac
        if [ "$path" = - ]; then
          {
            code=0
            git diff --no-ext-diff --no-textconv --binary --no-index -- /dev/null ./- ||
              code=$?
            echo "$code" > "$AXO_SNAPSHOT_SCRATCH/dash-status"
          } | awk -f "$AXO_SNAPSHOT_SCRATCH/dash.awk" || exit 70
          read -r code < "$AXO_SNAPSHOT_SCRATCH/dash-status"
        else
          git diff --no-ext-diff --no-textconv --binary --no-index -- /dev/null "$path"
          code=$?
        fi
        [ "$code" -le 1 ] || exit "$code"
      done
    ' sh < "$scratch/untracked" || code=$?
  fi
  echo "$code" > "$scratch/patch-status"
} | {
  code=0
  tee "$scratch/patch-digest" "$scratch/patch-size" || code=$?
  echo "$code" > "$scratch/tee-status"
} | {
  code=0
  head -c 524289 > "$scratch/patch" || code=$?
  cat > /dev/null
  echo "$code" > "$scratch/head-status"
}
wait "$digest_job"
wait "$size_job"
# Git's status, then the copies': a patch any of them did not write whole is
# not recorded.
for status in patch-status tee-status head-status; do
  read -r code < "$scratch/$status"
  [ "$code" -eq 0 ] || exit "$code"
done
# Every path whose state differs from HEAD, with its manifest line, whatever
# the repository's size: what a run can have changed, as this capture saw it.
# A path the differences name that the manifest does not hold is gone, and
# is listed as deleted. A submodule is listed when its commit differs from
# HEAD's, and a nested checkout, which Git lists as untracked, always.
if [ "$head_before" = unborn ]; then
  : > "$scratch/differ"
else
  GIT_INDEX_FILE=$scratch/patch-index \
    git diff --no-ext-diff --no-textconv --no-renames --ignore-submodules=dirty \
    --name-only -z "$head_before" -- > "$scratch/differ"
fi
cat "$scratch/untracked" >> "$scratch/differ"
od -An -v -tu1 < "$scratch/differ" > "$scratch/differ-bytes"
awk -v strip=1 -f "$scratch/base64.awk" < "$scratch/differ-bytes" > "$scratch/differ-encoded"
sort -u "$scratch/differ-encoded" > "$scratch/differ-names"
tab=$(printf '\t')
awk -F "$tab" -v OFS="$tab" -v names="$scratch/differ-names" '
  BEGIN { while ((getline name < names) > 0) wanted[name] = 1 }
  $1 in wanted { print $1, $2, $3, $4; found[$1] = 1 }
  END { for (name in wanted) if (!(name in found)) print name, "0", "deleted", "missing" }
' "$scratch/before" > "$scratch/differ-found"
sort -u "$scratch/differ-found" > "$scratch/differ-lines"
entries > "$scratch/entries-after"
tree "$scratch/entries-after" > "$scratch/after"
cmp "$scratch/before" "$scratch/after"
if [ "$capture_mode" != observe ]; then
  judged "$scratch/entries-after" > "$scratch/judged-after"
  cmp "$scratch/judged-before" "$scratch/judged-after"
fi
head_after=$(git rev-parse --verify HEAD 2>/dev/null || printf unborn)
[ "$head_before" = "$head_after" ]
if [ "$capture_mode" = compare ]; then
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
  sort "$scratch/baseline" "$scratch/judged-before" > "$scratch/both"
  uniq -u "$scratch/both" > "$scratch/once"
  cut -f 1 "$scratch/once" > "$scratch/once-names"
  sort -u "$scratch/once-names" > "$scratch/changed"
fi
tree=$(sha256sum < "$scratch/before")
patch=$(cat "$scratch/patch-sha256")
patch_size=$(tr -d ' ' < "$scratch/patch-bytes")
listed_size=0
[ ! -f "$scratch/changed" ] || listed_size=$(wc -c < "$scratch/changed" | tr -d ' ')
# The nested checkouts in the tree, whose files no capture reads, so the host
# knows where changes it cannot see could lie: at most 64 KiB of their names
# in base64 (a thousand names of 48 bytes take 64,000); past that the capture
# fails and says so (exit 71).
awk -F "$tab" '$3 == "special" && $4 == "unread" { print $1 }' "$scratch/before" > "$scratch/unread"
unread_size=$(wc -c < "$scratch/unread" | tr -d ' ')
if [ "$unread_size" -gt 65536 ]; then
  [ "$capture_mode" != compare ] || rm -rf -- "$baseline"
  echo "the repository holds more nested Git checkouts than one capture lists" >&2
  exit 71
fi
# What this prints stays within the smallest bound a capture runs under,
# whatever the change: the 768 KiB of output a check's capture takes (the
# host's own captures take 1 MiB). 16 KiB of it is kept for the fixed lines,
# which take under 1 KiB. The 8 KiB manifest prefix takes 10,924 bytes in
# base64 and, comparing, the changed list its own size: at most 512 KiB, the
# longest the host accepts, so a longer one is refused here. The names of the
# nested checkouts take theirs, at most 64 KiB. The patch is
# printed whole when it is at most the 512 KiB the host records and fits what
# is left. Otherwise only its first 12 KiB is, the most of a patch the host
# keeps when it cannot record it whole: with the whole patch's digest and
# size beside it, the host records the patch as incomplete, never as file
# input. The lines of the paths that differ from HEAD take what is left
# after that, or are left out (below). In all, under 1 KiB + 768 KiB - 16 KiB.
if [ "$listed_size" -gt 524289 ]; then
  rm -rf -- "$baseline"
  echo "the changed paths are more than one capture records" >&2
  exit 67
fi
room=$((786432 - 16384 - 10924 - listed_size - unread_size))
if [ "$patch_size" -le 524288 ] && [ $(((patch_size + 2) / 3 * 4)) -le "$room" ]; then
  shown=$patch_size
else
  shown=12288
fi
room=$((room - (shown + 2) / 3 * 4))
printf 'format=3\nhead=%s\ntree=%s\n' "$head_before" "${tree%% *}"
printf 'manifest_bytes=%s\nmanifest_b64=' "$(wc -c < "$scratch/before" | tr -d ' ')"
head -c 8192 "$scratch/before" | base64 | tr -d '\n'
printf '\npatch_sha256=%s\npatch_bytes=%s\npatch_b64=' "${patch%% *}" "$patch_size"
if [ "$shown" -eq "$patch_size" ]; then
  base64 < "$scratch/patch" | tr -d '\n'
else
  head -c "$shown" "$scratch/patch" | base64 | tr -d '\n'
fi
printf '\nunread='
tr '\n' ',' < "$scratch/unread" | sed 's/,$//'
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
    printf 'judged=%s\ncompared=%s\nchanged=' "${judged%% *}" "$expected"
    tr '\n' ',' < "$scratch/changed" | sed 's/,$//'
    printf '\n'
    rm -rf -- "$baseline"
    ;;
esac
# The lines of the paths that differ from HEAD, when they fit the room left
# above; beyond that, or beyond 10,000 paths, they are left out and say so.
differ_count=$(wc -l < "$scratch/differ-lines" | tr -d ' ')
differ_size=$(wc -c < "$scratch/differ-lines" | tr -d ' ')
if [ "$differ_count" -le 10000 ] && [ $(((differ_size + 2) / 3 * 4)) -le "$room" ]; then
  printf 'changed_complete=1\nchanged_b64='
  base64 < "$scratch/differ-lines" | tr -d '\n'
  printf '\n'
else
  printf 'changed_complete=0\nchanged_b64=\n'
fi
