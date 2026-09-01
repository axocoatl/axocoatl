#!/usr/bin/env bash
# Compute the exact, dependency-ordered crates for a release without publishing.
set -euo pipefail

fail() {
  echo "release-plan: $*" >&2
  exit 1
}

usage() {
  cat >&2 <<'EOF'
Usage: release-plan.sh <tag> <workspace-version> <metadata.json> <package-changes.txt>

The metadata file must be the output of:
  cargo metadata --locked --no-deps --format-version 1

package-changes.txt is the newline-delimited output of the CLI-only release diff
over Cargo.toml, axocoatl-server, crates, and packages. It may be empty.
EOF
  exit 2
}

[[ $# -eq 4 ]] || usage

tag=$1
workspace_version=$2
metadata_file=$3
package_changes_file=$4

[[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ ]] \
  || fail "tag is not a supported semantic product version: $tag"
version=${tag#v}
[[ "$workspace_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ ]] \
  || fail "workspace version is not a supported semantic version: $workspace_version"
[[ -f "$metadata_file" && ! -L "$metadata_file" ]] \
  || fail "metadata input is missing or is not a regular file: $metadata_file"
[[ -f "$package_changes_file" && ! -L "$package_changes_file" ]] \
  || fail "package-change input is missing or is not a regular file: $package_changes_file"
command -v jq >/dev/null 2>&1 || fail "jq is required"
jq -e '.packages | type == "array"' "$metadata_file" >/dev/null \
  || fail "metadata input is not valid Cargo metadata"

ordered_crates=(
  axocoatl-core
  axocoatl-token
  axocoatl-llm
  axocoatl-config
  axocoatl-memory
  axocoatl-graph
  axocoatl-isolation
  axocoatl-a2a
  axocoatl-llm-openai
  axocoatl-llm-anthropic
  axocoatl-llm-ollama
  axocoatl-llm-mistral
  axocoatl-llm-gemini
  axocoatl-mcp
  axocoatl-tools
  axocoatl-coordination
  axocoatl-actor
  axocoatl-session
  axocoatl-service
  axocoatl-daemon
  axocoatl-server
  axocoatl-cli
)

declared_crates=$(printf '%s\n' "${ordered_crates[@]}" | LC_ALL=C sort)
publishable_crates=$(
  jq -r \
    '[.packages[] | select(.source == null and .publish == null) | .name] | sort | .[]' \
    "$metadata_file"
)
[[ "$declared_crates" == "$publishable_crates" ]] || {
  echo "release-plan: dependency-ordered inventory is not the exact publishable workspace set" >&2
  diff -u \
    <(printf '%s\n' "$publishable_crates") \
    <(printf '%s\n' "$declared_crates") >&2 || true
  exit 1
}

# Set equality does not prove publication order. Every local publishable
# dependency must appear before its consumer.
seen=$'\n'
for crate in "${ordered_crates[@]}"; do
  count=$(jq -r --arg name "$crate" '[.packages[] | select(.name == $name)] | length' "$metadata_file")
  [[ "$count" -eq 1 ]] || fail "expected exactly one metadata package named $crate"
  while IFS= read -r dependency; do
    [[ -z "$dependency" ]] && continue
    [[ "$seen" == *$'\n'"$dependency"$'\n'* ]] \
      || fail "$crate appears before local dependency $dependency in the publish order"
  done < <(
    jq -r --arg name "$crate" --argjson publishable "$(printf '%s\n' "$declared_crates" | jq -Rsc 'split("\n")[:-1]')" '
      .packages[]
      | select(.name == $name)
      | .dependencies[]
      | select(.kind != "dev")
      | select(.name as $dependency | $publishable | index($dependency))
      | .name
    ' "$metadata_file" | LC_ALL=C sort -u
  )
  seen+="$crate"$'\n'
done

release_crates=()
for crate in "${ordered_crates[@]}"; do
  package_version=$(jq -r --arg name "$crate" \
    '[.packages[] | select(.name == $name) | .version] | if length == 1 then .[0] else "" end' \
    "$metadata_file")
  [[ -n "$package_version" ]] || fail "could not determine version for $crate"
  if [[ "$package_version" == "$version" ]]; then
    release_crates+=("$crate")
  fi
done

[[ ${#release_crates[@]} -gt 0 ]] \
  || fail "no publishable workspace package has release version $version"
[[ " ${release_crates[*]} " == *" axocoatl-cli "* ]] \
  || fail "axocoatl-cli must be part of release $version"

if [[ "$version" == "$workspace_version" ]]; then
  [[ ${#release_crates[@]} -eq ${#ordered_crates[@]} ]] \
    || fail "a coordinated workspace release must include every publishable package"
  for index in "${!ordered_crates[@]}"; do
    [[ "${release_crates[$index]}" == "${ordered_crates[$index]}" ]] \
      || fail "coordinated release selection differs from the dependency order"
  done
else
  [[ ${#release_crates[@]} -eq 1 && "${release_crates[0]}" == axocoatl-cli ]] \
    || fail "a CLI-only product release may publish only axocoatl-cli"

  unexpected_changes=$(
    sed '/^axocoatl-server\/THIRD_PARTY_LICENSES\.txt$/d; /^$/d' "$package_changes_file"
  )
  [[ -z "$unexpected_changes" ]] || {
    echo "release-plan: CLI-only release contains unpublished package changes" >&2
    printf '%s\n' "$unexpected_changes" >&2
    exit 1
  }
fi

printf '%s\n' "${release_crates[@]}"
