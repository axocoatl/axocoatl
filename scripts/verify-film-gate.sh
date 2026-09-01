#!/usr/bin/env bash
# Single entrypoint for portable, release-candidate, and incident film proofs.
set -euo pipefail

fail() {
  echo "film-gate: $*" >&2
  exit 1
}

usage() {
  cat >&2 <<'EOF'
Usage:
  verify-film-gate.sh portable
  verify-film-gate.sh source-bound
  verify-film-gate.sh candidate <base-commit> <head-commit>
  verify-film-gate.sh candidate-worktree <base-commit>
  verify-film-gate.sh release-compatibility <record.json> <frozen-release-root>
EOF
  exit 2
}

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=${AXO_FILM_REPO_ROOT:-$(CDPATH= cd -- "$script_dir/.." && pwd)}
verifier=${AXO_FILM_VERIFIER:-$repo_root/demo/one-app/films/verify-film-set.mjs}
[[ -f "$verifier" && ! -L "$verifier" ]] || fail "film verifier is missing: $verifier"

run_verifier() {
  node "$verifier" "$@"
}

manifest_version() {
  local revision=$1
  git -C "$repo_root" show "$revision:axocoatl-cli/Cargo.toml" | awk '
    $0 == "[package]" { package_section = 1; next }
    package_section && /^\[/ { exit }
    package_section && $1 == "version" {
      gsub(/"/, "", $3)
      print $3
      exit
    }
  '
}

working_manifest_version() {
  awk '
    $0 == "[package]" { package_section = 1; next }
    package_section && /^\[/ { exit }
    package_section && $1 == "version" {
      gsub(/"/, "", $3)
      print $3
      exit
    }
  ' "$repo_root/axocoatl-cli/Cargo.toml"
}

is_recording_artifact() {
  case "$1" in
    demo/one-app/films/source/* | \
    demo/one-app/films/staged/* | \
    demo/one-app/films/provenance/* | \
    sites/marketing/assets/films/*) return 0 ;;
    *) return 1 ;;
  esac
}

first_committed_blob() {
  local revision=$1 path=$2 first_commit
  first_commit=$(git -C "$repo_root" log \
    --diff-filter=A --format=%H --reverse "$revision" -- "$path" | sed -n '1p')
  [[ -n "$first_commit" ]] || return 1
  git -C "$repo_root" rev-parse "$first_commit:$path"
}

candidate_changed_paths() {
  local kind=$1 base=$2 head=${3:-}
  if [[ "$kind" == commit ]]; then
    git -C "$repo_root" diff --name-only "$base" "$head" --
  else
    {
      git -C "$repo_root" diff --name-only "$base" --
      git -C "$repo_root" ls-files --others --exclude-standard
    } | LC_ALL=C sort -u
  fi
}

candidate_blob() {
  local kind=$1 revision=$2 path=$3
  if [[ "$kind" == commit ]]; then
    git -C "$repo_root" cat-file -e "$revision:$path" 2>/dev/null || return 1
    git -C "$repo_root" rev-parse "$revision:$path"
  else
    [[ -f "$repo_root/$path" && ! -L "$repo_root/$path" ]] || return 1
    git -C "$repo_root" hash-object --path="$path" "$repo_root/$path"
  fi
}

verify_candidate() {
  local kind=$1 base=$2 head=$3 base_version head_version history_revision
  local require_source_bound=false restored_count=0 new_count=0
  git -C "$repo_root" cat-file -e "$base^{commit}" 2>/dev/null \
    || fail "candidate base is not a commit: $base"
  base_version=$(manifest_version "$base")
  if [[ "$kind" == commit ]]; then
    git -C "$repo_root" cat-file -e "$head^{commit}" 2>/dev/null \
      || fail "candidate head is not a commit: $head"
    head_version=$(manifest_version "$head")
    history_revision=$head
  else
    head_version=$(working_manifest_version)
    history_revision=HEAD
  fi
  [[ -n "$base_version" && -n "$head_version" ]] \
    || fail "could not determine candidate CLI versions"

  while IFS= read -r path; do
    [[ -n "$path" ]] || continue
    is_recording_artifact "$path" || continue
    if ! git -C "$repo_root" cat-file -e "$base:$path" 2>/dev/null; then
      current_blob=$(candidate_blob "$kind" "$head" "$path" || true)
      [[ -n "$current_blob" ]] || fail "new recording artifact is missing from the candidate: $path"
      new_count=$((new_count + 1))
      require_source_bound=true
      continue
    fi
    first_blob=$(first_committed_blob "$history_revision" "$path" || true)
    current_blob=$(candidate_blob "$kind" "$head" "$path" || true)
    if [[ -n "$first_blob" ]]; then
      [[ -n "$current_blob" && "$current_blob" == "$first_blob" ]] || fail \
        "first-seen recording artifact changed: $path; use a new versioned path for a recapture"
      restored_count=$((restored_count + 1))
    else
      [[ -n "$current_blob" ]] || fail "new recording artifact is missing from the candidate: $path"
      new_count=$((new_count + 1))
      require_source_bound=true
    fi
  done < <(candidate_changed_paths "$kind" "$base" "$head")

  run_verifier --portable
  if [[ "$base_version" != "$head_version" ]]; then
    require_source_bound=true
    echo "film-gate: product version changed $base_version -> $head_version; requiring source-bound provenance"
  fi
  if (( restored_count > 0 )); then
    echo "film-gate: restored $restored_count recording artifact(s) to their first-committed bytes"
  fi
  if (( new_count > 0 )); then
    echo "film-gate: found $new_count new versioned recording artifact(s); requiring source-bound provenance"
  fi
  if [[ "$require_source_bound" == true ]]; then
    run_verifier --source-bound
  else
    echo "film-gate: product version and versioned recording set are unchanged; portable PR proof is sufficient"
  fi
}

mode=${1:-}
case "$mode" in
  portable)
    [[ $# -eq 1 ]] || usage
    run_verifier --portable
    ;;
  source-bound)
    [[ $# -eq 1 ]] || usage
    run_verifier --source-bound
    ;;
  candidate)
    [[ $# -eq 3 ]] || usage
    verify_candidate commit "$2" "$3"
    ;;
  candidate-worktree)
    [[ $# -eq 2 ]] || usage
    verify_candidate worktree "$2" WORKTREE
    ;;
  release-compatibility)
    [[ $# -eq 3 ]] || usage
    record=$2
    release_root=$3
    [[ -f "$record" && ! -L "$record" ]] || fail "compatibility record is missing: $record"
    [[ -d "$release_root" ]] || fail "frozen release root is missing: $release_root"
    run_verifier --release-compatibility "$record" --release-root "$release_root"
    ;;
  *) usage ;;
esac
