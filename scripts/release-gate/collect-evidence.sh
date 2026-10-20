#!/usr/bin/env bash
# Gather the new-user gate's evidence for one release candidate into a signed
# bundle, release-evidence/<version>/<commit>/, ready to commit
# (docs/RELEASE_GATE.md). Run it in the gate account, whose gate signing key
# is listed in .axocoatl/gate-keys.
#
# Usage:
#   collect-evidence.sh --version X.Y.Z --commit SHA [--from DIR]...
#       [--ci-run RUN_ID]... [--axocoatl BIN] [--keys FILE] [--signer DIR]
#       [--github-repo OWNER/NAME] [--no-sign] [--replace]
#   collect-evidence.sh --init-signer [--axocoatl BIN] [--signer DIR]
#   --from DIR      an evidence root (DIR/<version>/<commit>/) or a bundle
#                   directory holding <path>.json files and their folders
#   --ci-run ID     download the Tier A artifacts (newuser-*) of that GitHub
#                   Actions run with gh. The run must be a push run of
#                   .github/workflows/ci.yml in --github-repo (default
#                   axocoatl/axocoatl) whose head commit is --commit.
#   --axocoatl BIN  the axocoatl that signs (default: the one on PATH)
#   --keys FILE     the trusted gate keys (default .axocoatl/gate-keys)
#   --signer DIR    the gate's signing data root (default
#                   $GATE_SIGNER_DIR or ~/.axocoatl-gate-signer). It is kept
#                   apart from Axocoatl's own data, which reset-mac-account.sh
#                   removes before every run, so the key registered in
#                   .axocoatl/gate-keys stays the one that signs.
#   --init-signer   make the signing key once (when there is none) and print
#                   the line to add to .axocoatl/gate-keys
#   --no-sign       assemble and check, but leave the manifest unsigned
#   --replace       replace a path already in the bundle
#   --repo-root DIR the checkout to write release-evidence/ into (default:
#                   this one)
# Exit: 0 when the signed bundle verifies and every required path passes
# (with --no-sign: when it was assembled), 1 otherwise; the bundle is still
# written, and what the release lacks is printed.
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

version='' commit='' axocoatl_bin='' keys='' sign=1 replace=0 init_signer=0
signer=${GATE_SIGNER_DIR:-$HOME/.axocoatl-gate-signer} github_repo=axocoatl/axocoatl
sources=() ci_runs=()
while [ $# -gt 0 ]; do
  case "$1" in
    --version) version=${2:?}; shift ;;
    --commit) commit=${2:?}; shift ;;
    --from) sources+=("${2:?}"); shift ;;
    --ci-run) ci_runs+=("${2:?}"); shift ;;
    --axocoatl) axocoatl_bin=${2:?}; shift ;;
    --keys) keys=${2:?}; shift ;;
    --signer) signer=${2:?}; shift ;;
    --github-repo) github_repo=${2:?}; shift ;;
    --init-signer) init_signer=1 ;;
    --no-sign) sign=0 ;;
    --replace) replace=1 ;;
    --repo-root) repo_root=$(CDPATH='' cd -- "${2:?}" && pwd); shift ;;
    -h | --help) usage ;;
    *) fail "unknown argument $1 (see --help)" ;;
  esac
  shift
done

# The gate's own signing data root: only the receipt key lives there, and
# nothing in the user's environment may point the CLI elsewhere.
signer_axocoatl() {
  env -u AXOCOATL_CONFIG -u XDG_CONFIG_HOME -u XDG_DATA_HOME AXOCOATL_DATA_DIR="$signer/data" \
    "$axocoatl_bin" "$@" --config "$signer/config.yaml"
}

if [ "$sign" = 1 ] || [ "$init_signer" = 1 ]; then
  [ -n "$axocoatl_bin" ] || axocoatl_bin=$(command -v axocoatl) || fail "no axocoatl to sign with; pass --axocoatl"
fi
if [ "$init_signer" = 1 ]; then
  mkdir -p "$signer"
  chmod 0700 "$signer"
  signer_axocoatl receipt key
  echo "collect-evidence: add the line above to .axocoatl/gate-keys in a reviewed commit (signer: $signer)" >&2
  exit 0
fi

[ -n "$keys" ] || keys="$repo_root/.axocoatl/gate-keys"
version=${version#v}
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "--version X.Y.Z is required"
[[ "$commit" =~ ^[0-9a-f]{40}$ ]] || fail "--commit must be the full commit SHA the candidate was built from"
[[ "$github_repo" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || fail "--github-repo must be OWNER/NAME"
[ ${#sources[@]} -gt 0 ] || [ ${#ci_runs[@]} -gt 0 ] || fail "give at least one --from or --ci-run"
command -v node >/dev/null 2>&1 || fail "node is required"

bundle="$repo_root/release-evidence/$version/$commit"
mkdir -p "$bundle"
work=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-collect.XXXXXX")
trap 'rm -rf -- "$work"' EXIT

# Tier A evidence comes only from CI's own push runs of the candidate commit
# in the canonical repository: a fork's pull request controls its run and
# could write evidence that claims any commit.
for run_id in "${ci_runs[@]+"${ci_runs[@]}"}"; do
  [[ "$run_id" =~ ^[0-9]+$ ]] || fail "--ci-run takes a run ID (digits), not $run_id"
  command -v gh >/dev/null 2>&1 || fail "gh is required for --ci-run"
  gh api "repos/$github_repo/actions/runs/$run_id" >"$work/run-$run_id.json" \
    || fail "could not read run $run_id of $github_repo"
  node -e '
    const run = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
    const [repo, commit, id] = process.argv.slice(2);
    const problems = [];
    if (run.head_repository?.full_name !== repo) problems.push(`its code is from ${run.head_repository?.full_name ?? "an unknown repository"}, not ${repo}`);
    if (run.repository?.full_name !== repo) problems.push(`it ran in ${run.repository?.full_name ?? "an unknown repository"}, not ${repo}`);
    if (run.event !== "push") problems.push(`it is a ${run.event} run, not a push run`);
    if (run.path !== ".github/workflows/ci.yml") problems.push(`it is a run of ${run.path}, not .github/workflows/ci.yml`);
    if (run.head_sha !== commit) problems.push(`it ran commit ${run.head_sha}, not ${commit}`);
    if (run.status !== "completed") problems.push(`it is ${run.status}, not completed`);
    if (problems.length) {
      console.error(`collect-evidence: refusing run ${id}: ${problems.join("; ")}`);
      process.exit(1);
    }
    console.log(`run ${id}: ci.yml, push to ${run.head_branch} at ${commit.slice(0, 12)} in ${repo}`);
  ' "$work/run-$run_id.json" "$github_repo" "$commit" "$run_id" || exit 1
  gh run download "$run_id" --repo "$github_repo" --pattern 'newuser-*' --dir "$work/ci-$run_id"
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

# Nothing that looks like a credential may be committed with the evidence:
# API keys (OpenRouter, Anthropic, OpenAI, AWS), GitHub tokens, JWTs (the
# ChatGPT sign-in's tokens), private keys, sign-in links, or a file named
# like one of Axocoatl's or SSH's secrets.
secret_text='sk-or-v1-[A-Za-z0-9]{16}|sk-ant-[A-Za-z0-9_-]{16}|sk-proj-[A-Za-z0-9_-]{16}|(^|[^A-Za-z0-9_-])sk-[A-Za-z0-9_-]{32,}'
secret_text="$secret_text|gh[pousr]_[A-Za-z0-9]{30}|github_pat_[A-Za-z0-9_]{20,}|eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\."
secret_text="$secret_text|AKIA[0-9A-Z]{16}|-----BEGIN [A-Z ]*PRIVATE KEY-----|axocoatl.receipt-signing-key|[?&]token=[A-Za-z0-9_-]{16}"
{
  grep -rIlE "$secret_text" "$bundle" 2>/dev/null || true
  find "$bundle" \( -name 'receipt-signing-key*' -o -name 'local-api-token*' -o -name 'id_ed25519*' -o -name 'id_rsa*' \
    -o -name '*.pem' -o -path '*/secrets/*' \) -print
} >"$work/secrets.txt"
if [ -s "$work/secrets.txt" ]; then
  sort -u "$work/secrets.txt" >&2
  fail "these files look like they hold a credential; remove or redact it before collecting"
fi

node "$evidence_js" manifest "$bundle" --version "$version" --commit "$commit"
decision=0
if [ "$sign" = 1 ]; then
  [ -f "$keys" ] || fail "$keys is missing: add the line \`collect-evidence.sh --init-signer\` prints to it"
  signer_axocoatl receipt sign-evidence "$bundle/manifest.json" \
    || fail "signing failed; make the gate's signing key once with --init-signer (signer: $signer)"
  # As release.yml decides: the bundle verifies and every required path
  # passes; a failing path the release does not require is a warning.
  node "$evidence_js" verify-bundle "$bundle" --keys "$keys" --version "$version" --commit "$commit" \
    --required "$repo_root/docs/RELEASE_GATE.md" --root "$repo_root" || decision=1
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
if [ "$decision" != 0 ]; then
  echo "collect-evidence: NOT releasable yet: see what the release needs above" >&2
fi
exit "$decision"
