#!/usr/bin/env bash
# Gather the new-user gate's evidence for one release candidate into a signed
# bundle, release-evidence/<version>/<commit>/, ready to commit
# (docs/RELEASE_GATE.md). Run it in the gate account, whose receipt key is
# listed in .axocoatl/gate-keys.
#
# Usage:
#   collect-evidence.sh --version X.Y.Z --commit SHA [--from DIR]...
#       [--ci-run RUN_ID]... [--axocoatl BIN] [--keys FILE] [--no-sign]
#       [--replace]
#   --from DIR      an evidence root (DIR/<version>/<commit>/) or a bundle
#                   directory holding <path>.json files and their folders
#   --ci-run ID     download the Tier A artifacts (newuser-A-*) of that
#                   GitHub Actions run with gh
#   --axocoatl BIN  the axocoatl that signs (default: the one on PATH)
#   --keys FILE     the trusted gate keys (default .axocoatl/gate-keys)
#   --no-sign       assemble and check, but leave the manifest unsigned
#   --replace       replace a path already in the bundle
#   --repo-root DIR the checkout to write release-evidence/ into (default:
#                   this one)
# The node -e programs read their values from process.argv.
# shellcheck disable=SC2016
set -euo pipefail

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH='' cd -- "$script_dir/../.." && pwd)
evidence_js="$script_dir/evidence.mjs"

fail() {
  echo "collect-evidence: $*" >&2
  exit 1
}
usage() {
  awk 'NR == 1 { next } /^#/ { if ($0 ~ /shellcheck|positional param|take their values|read their values|^# parameters\.$/) next; sub(/^# ?/, ""); print; next } { exit }' "$0"
  exit 2
}

version='' commit='' axocoatl_bin='' keys='' sign=1 replace=0
sources=() ci_runs=()
while [ $# -gt 0 ]; do
  case "$1" in
    --version) version=${2:?}; shift ;;
    --commit) commit=${2:?}; shift ;;
    --from) sources+=("${2:?}"); shift ;;
    --ci-run) ci_runs+=("${2:?}"); shift ;;
    --axocoatl) axocoatl_bin=${2:?}; shift ;;
    --keys) keys=${2:?}; shift ;;
    --no-sign) sign=0 ;;
    --replace) replace=1 ;;
    --repo-root) repo_root=$(CDPATH='' cd -- "${2:?}" && pwd); shift ;;
    -h | --help) usage ;;
    *) fail "unknown argument $1 (see --help)" ;;
  esac
  shift
done
[ -n "$keys" ] || keys="$repo_root/.axocoatl/gate-keys"
version=${version#v}
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "--version X.Y.Z is required"
[[ "$commit" =~ ^[0-9a-f]{40}$ ]] || fail "--commit must be the full commit SHA the candidate was built from"
[ ${#sources[@]} -gt 0 ] || [ ${#ci_runs[@]} -gt 0 ] || fail "give at least one --from or --ci-run"
command -v node >/dev/null 2>&1 || fail "node is required"

bundle="$repo_root/release-evidence/$version/$commit"
mkdir -p "$bundle"
work=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-collect.XXXXXX")
trap 'rm -rf -- "$work"' EXIT

for run_id in "${ci_runs[@]+"${ci_runs[@]}"}"; do
  command -v gh >/dev/null 2>&1 || fail "gh is required for --ci-run"
  gh run download "$run_id" --pattern 'newuser-*' --dir "$work/ci-$run_id"
  for artifact in "$work/ci-$run_id"/*/; do sources+=("$artifact"); done
done

# Copy each source's path evidence (and its folder) into the bundle.
copied=0
for source in "${sources[@]}"; do
  [ -d "$source" ] || fail "no such directory: $source"
  dir=$source
  [ -d "$source/$version/$commit" ] && dir="$source/$version/$commit"
  for file in "$dir"/*.json; do
    [ -f "$file" ] || continue
    name=$(basename "$file" .json)
    case "$name" in manifest | manifest.dsse) continue ;; esac
    node -e '
      const e = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
      if (e.schema !== "axocoatl.release-evidence.path/1") process.exit(3);
      if (e.path !== process.argv[2]) { console.error(`${process.argv[1]} is evidence of ${e.path}`); process.exit(1); }
      if (e.version !== process.argv[3] || e.commit !== process.argv[4]) {
        console.error(`${process.argv[1]} is evidence of ${e.version} at ${e.commit}, not ${process.argv[3]} at ${process.argv[4]}`);
        process.exit(1);
      }' "$file" "$name" "$version" "$commit" || {
      status=$?
      [ "$status" = 3 ] && continue
      fail "refusing $file"
    }
    if [ -e "$bundle/$name.json" ] && [ "$(CDPATH='' cd -- "$dir" && pwd)" != "$bundle" ]; then
      [ "$replace" = 1 ] || fail "$name is already in $bundle; pass --replace to replace it"
      rm -rf -- "${bundle:?}/$name.json" "${bundle:?}/$name"
    fi
    if [ "$(CDPATH='' cd -- "$dir" && pwd)" != "$bundle" ]; then
      cp "$file" "$bundle/$name.json"
      [ -d "$dir/$name" ] && cp -R "$dir/$name" "$bundle/$name"
    fi
    echo "collected $name from $dir"
    copied=$((copied + 1))
  done
done
[ "$copied" -gt 0 ] || fail "no path evidence found in the sources"

# Nothing that looks like a credential may be committed with the evidence.
if grep -rIlE 'sk-or-v1-[A-Za-z0-9]{16}|sk-ant-[A-Za-z0-9_-]{16}|sk-proj-[A-Za-z0-9_-]{16}|gh[pousr]_[A-Za-z0-9]{30}|AKIA[0-9A-Z]{16}|-----BEGIN [A-Z ]*PRIVATE KEY-----|axocoatl.receipt-signing-key|[?&]token=[A-Za-z0-9_-]{16}' "$bundle" >"$work/secrets.txt" 2>/dev/null; then
  cat "$work/secrets.txt" >&2
  fail "these files look like they hold a credential; remove or redact it before collecting"
fi

node "$evidence_js" manifest "$bundle" --version "$version" --commit "$commit"
if [ "$sign" = 1 ]; then
  [ -n "$axocoatl_bin" ] || axocoatl_bin=$(command -v axocoatl) || fail "no axocoatl to sign with; pass --axocoatl"
  "$axocoatl_bin" receipt sign-evidence "$bundle/manifest.json"
  [ -f "$keys" ] || fail "$keys is missing: add the line \`axocoatl receipt key\` prints to it"
  node "$evidence_js" verify-bundle "$bundle" --keys "$keys" --version "$version" --commit "$commit"
else
  rm -f "$bundle/manifest.dsse.json"
  echo "collect-evidence: the manifest is NOT signed (--no-sign); release.yml will refuse it"
fi

# Time and download against the previous release, when it has evidence.
previous=$(find "$repo_root/release-evidence" -mindepth 1 -maxdepth 1 -type d ! -name "$version" -exec basename {} \; 2>/dev/null \
  | sort -t. -k1,1n -k2,2n -k3,3n | awk -v v="$version" '{ split($0, a, "."); split(v, b, ".");
      if (a[1] < b[1] || (a[1] == b[1] && (a[2] < b[2] || (a[2] == b[2] && a[3] < b[3])))) last = $0 } END { print last }')
if [ -n "$previous" ]; then
  for file in "$bundle"/*.json; do
    name=$(basename "$file" .json)
    case "$name" in manifest | manifest.dsse) continue ;; esac
    old=$(find "$repo_root/release-evidence/$previous" -name "$name.json" -maxdepth 2 | head -n 1)
    [ -n "$old" ] && node "$evidence_js" compare "$old" "$file"
  done
fi

cat <<EOF

Bundle: release-evidence/$version/$commit/
Next:
  git add release-evidence/$version/$commit
  git commit -m "Record the new-user gate evidence for $version"
  node scripts/release-gate/check-evidence.mjs --version $version --tag-commit "\$(git rev-parse HEAD)"
Tag the commit that check passes on; release.yml runs the same check.
EOF
