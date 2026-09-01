#!/usr/bin/env bash
# Static, non-mutating contract for the one-shot v1.0.1 incident recovery.
set -euo pipefail

script_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
repo_root="$(CDPATH= cd -- "$script_dir/.." && pwd)"
workflow="$repo_root/.github/workflows/recover-v1.0.1.yml"
attestation="$repo_root/demo/one-app/films/compatibility/v1.0.1.json"
publisher="$repo_root/scripts/publish-crate-resilient.sh"

fail() {
  echo "test-recover-v1.0.1-workflow: $*" >&2
  exit 1
}

test -f "$workflow" || fail "recovery workflow is missing"
test -f "$attestation" || fail "compatibility attestation is missing"
test -x "$publisher" || fail "resilient crate publisher is missing"

require_literal() {
  local literal=$1
  grep -Fq -- "$literal" "$workflow" || fail "missing workflow contract: $literal"
}

require_literal 'RELEASE_TAG: v1.0.1'
require_literal 'RELEASE_VERSION: 1.0.1'
require_literal 'RELEASE_COMMIT: e82902bdfabb0541e466d5f98f0013cea36bdeab'
require_literal "SOURCE_DATE_EPOCH: '1787866822'"
require_literal "SOURCE_RUN_ID: '33119338872'"
require_literal 'REQUIRED_CONFIRMATION_PREFIX: PUBLISH v1.0.1 e82902bdfabb0541e466d5f98f0013cea36bdeab 33119338872 CONTROL'
require_literal 'expected_confirmation="$REQUIRED_CONFIRMATION_PREFIX $GITHUB_SHA"'
require_literal 'test "$GITHUB_SHA" = "$main_commit"'
require_literal 'group: axocoatl-production-release'
require_literal 'queue: max'
require_literal 'cancel-in-progress: false'
require_literal 'ref: e82902bdfabb0541e466d5f98f0013cea36bdeab'
require_literal 'node recovery-control/demo/one-app/films/verify-film-set.mjs'
require_literal '--release-compatibility'
require_literal 'recovery-control/demo/one-app/films/compatibility/v1.0.1.json'
require_literal '--release-root release-source'
require_literal 'releases/$release_id/assets?name=$name'
require_literal 'https://uploads.github.com/'
require_literal '../recovery-control/scripts/publish-crate-resilient.sh'
require_literal 'recovery-control/scripts/prove-release-crates.sh'
require_literal 'name: recovery-${{ github.run_id }}-${{ matrix.target }}'
require_literal 'name: verified-recovery-${{ github.run_id }}-v1.0.1'
require_literal 'name: recovered-cli-checksum-${{ github.run_id }}'
require_literal 'overwrite: true'
require_literal 'gh run download "$GITHUB_RUN_ID"'
require_literal '--repo "$GITHUB_REPOSITORY"'
require_literal 'Prior-attempt artifact $ARTIFACT_NAME is not byte-identical.'
require_literal 'Prior-attempt verified release set is not byte-identical.'
require_literal 'Prior-attempt CLI checksum proof is not byte-identical.'
require_literal 'AXO_RELEASE_SOURCE_ROOT="$GITHUB_WORKSPACE/release-source"'
require_literal 'recovery-control/scripts/release-artifact.sh'
require_literal 'The reviewed packager did not reproduce $archive byte-for-byte.'
require_literal 'actions/setup-python@e797f83bcb11b83ae66e0230d6156d7c80228e7c # v6.0.0'
require_literal 'Existing release asset $name differs; refusing to overwrite it.'
require_literal 'A release has an invalid namespace or is newer than v1.0.1.'
require_literal 'Latest release must be exactly v1.0.0 or the exact public v1.0.1.'
require_literal 'group: axocoatl-production-marketing'
require_literal 'group: axocoatl-production-docs'
require_literal 'cp recovery-control/scripts/test-install.sh "$contract_root/scripts/test-install.sh"'
require_literal 'cp release-source/scripts/install.sh "$contract_root/scripts/install.sh"'
require_literal 'cmp -s release-source/scripts/install.sh public-install.sh'
require_literal 'cmp -s release-source/sites/marketing/index.html marketing-home.html'
require_literal 'bash recovery-control/scripts/check-public-release.sh'
require_literal 'pages deploy release-source/sites/docs/dist --project-name=axocoatl-docs --branch=main'

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

workflow_attestation_sha="$(ruby -ryaml -e '
  document = YAML.safe_load(File.read(ARGV.fetch(0)), aliases: true)
  puts document.fetch("env").fetch("COMPATIBILITY_ATTESTATION_SHA256")
' "$workflow")"
actual_attestation_sha="$(sha256_file "$attestation")"
test "$workflow_attestation_sha" = "$actual_attestation_sha" \
  || fail "workflow compatibility hash does not match the actual attestation"

test "$(grep -Fxc '              upload_status="$(curl --connect-timeout 10 --max-time 300 \' "$workflow")" = 1 \
  || fail "the recovery must contain exactly one fail-closed exact-ID asset upload command"
test "$(grep -Fc 'gh release upload' "$workflow")" = 0 \
  || fail "the recovery must never resolve an asset mutation through a movable tag"
test "$(grep -Fc 'cargo publish' "$workflow")" = 0 \
  || fail "the recovery workflow must delegate its only crate mutation to the tested resilient publisher"
test "$(grep -Fxc '    cargo publish --locked --no-verify -p "$crate"' "$publisher")" = 1 \
  || fail "the resilient publisher must contain exactly one guarded Cargo publication command"
test "$(grep -Fc 'recovery-control/scripts/prove-release-crates.sh' "$workflow")" = 2 \
  || fail "the registry API and sparse index must be reproved before release publication and in final production proof"
test "$(grep -Fc 'cloudflare/wrangler-action@9acf94ace14e7dc412b076f2c5c20b8ce93c79cd' "$workflow")" = 2 \
  || fail "the incident-local workflow must perform exactly one marketing and one docs deployment"
test "$(grep -Fc "node-version: '22'" "$workflow")" = 3 \
  || fail "both marketing jobs and the docs job must pin Node 22"
test "$(grep -Fc 'cp recovery-control/scripts/test-install.sh "$contract_root/scripts/test-install.sh"' "$workflow")" = 2 \
  || fail "both marketing gates must use the current release-aware installer harness"
test "$(grep -Fc 'cp release-source/scripts/install.sh "$contract_root/scripts/install.sh"' "$workflow")" = 2 \
  || fail "both marketing gates must exercise the frozen installer"
test "$(grep -Fc 'pages deploy /tmp/axocoatl-v1.0.1-marketing --project-name=axocoatl-marketing --branch=main' "$workflow")" = 1 \
  || fail "the recovery must contain exactly one marketing production deployment"
test "$(grep -Fc 'pages deploy release-source/sites/docs/dist --project-name=axocoatl-docs --branch=main' "$workflow")" = 1 \
  || fail "the recovery must contain exactly one frozen-source docs deployment"
test "$(grep -Fc 'overwrite: true' "$workflow")" = 3 \
  || fail "all three run-scoped ephemeral artifact uploads must allow proved rerun-all replacement"
test "$(grep -Fc -- '--repo "$GITHUB_REPOSITORY"' "$workflow")" = 3 \
  || fail "all repository-inferred gh operations must explicitly bind the current repository"

if grep -Fq -- './.github/workflows/docs-deploy.yml' "$workflow" \
  || grep -Fq -- 'run: ./scripts/test-install.sh' "$workflow" \
  || grep -Fq -- 'run: ./scripts/test-release-artifact.sh' "$workflow" \
  || grep -Fq -- './scripts/release-artifact.sh' "$workflow" \
  || grep -Fq -- 'release-source/scripts/release-artifact.sh' "$workflow"; then
  fail "recovery must not invoke helpers that are absent or stale in the frozen checkout"
fi
if grep -Fq -- 'cmp -s scripts/install.sh public-install.sh' "$workflow"; then
  fail "final production proof references an installer outside its two explicit checkouts"
fi

if grep -En -- \
  '--clobber|gh release delete|gh api[[:space:]].*--method[[:space:]]+DELETE|cargo publish[^\n]*-p[[:space:]]+(axocoatl-(core|server|daemon)|[^a])|git push|git tag[[:space:]]' \
  "$workflow"; then
  fail "recovery contains an overwrite, deletion, tag mutation, or non-CLI publication path"
fi

ruby -e '
  require "yaml"
  document = YAML.safe_load(File.read(ARGV.fetch(0)), aliases: true)
  abort "workflow is not a mapping" unless document.is_a?(Hash)
  triggers = document["on"] || document[true]
  abort "workflow_dispatch is missing" unless triggers.is_a?(Hash) && triggers.key?("workflow_dispatch")
  jobs = document.fetch("jobs")
  required = %w[
    preflight marketing-recovery-gate build prove-artifacts prepare-release
    publish-cli publish-release deploy-marketing deploy-docs prove-production
  ]
  missing = required.reject { |job| jobs.key?(job) }
  abort "missing recovery jobs: #{missing.join(", ")}" unless missing.empty?
  docs = jobs.fetch("deploy-docs")
  abort "deploy-docs must consume the exact prepared release ID" \
    unless Array(docs["needs"]) == %w[prepare-release publish-release]
  abort "deploy-docs must be recovery-owned" if docs.key?("uses")
  abort "deploy-docs must contain explicit steps" unless docs["steps"].is_a?(Array) && !docs["steps"].empty?
  abort "deploy-docs concurrency is wrong" unless docs.dig("concurrency", "group") == "axocoatl-production-docs"
  abort "deploy-docs must retain every pending production deployment" unless docs.dig("concurrency", "queue") == "max"
  abort "deploy-docs must not cancel a production deployment" unless docs.dig("concurrency", "cancel-in-progress") == false
  marketing = jobs.fetch("deploy-marketing")
  abort "deploy-marketing must consume the exact prepared release ID" \
    unless Array(marketing["needs"]) == %w[prepare-release publish-release]
  abort "deploy-marketing concurrency is wrong" unless marketing.dig("concurrency", "group") == "axocoatl-production-marketing"
  abort "deploy-marketing must retain every pending production deployment" unless marketing.dig("concurrency", "queue") == "max"
  abort "deploy-marketing must not cancel a production deployment" unless marketing.dig("concurrency", "cancel-in-progress") == false

  steps = jobs.values.flat_map { |job| job.fetch("steps", []) }
  python_setups = steps.select { |step| step["uses"] == "actions/setup-python@e797f83bcb11b83ae66e0230d6156d7c80228e7c" }
  abort "expected pinned Python in the packaging gate and build matrix" unless python_setups.length == 2
  python_setups.each do |step|
    config = step.fetch("with")
    abort "release Python version is not exact" unless config["python-version"] == "3.13.7"
    abort "setup-python may not resolve a newer interpreter" unless config["check-latest"] == false
  end
  uploads = steps.select { |step| step["uses"].to_s.start_with?("actions/upload-artifact@") }
  abort "expected exactly three ephemeral artifact uploads" unless uploads.length == 3
  uploads.each do |step|
    config = step.fetch("with")
    name = config.fetch("name")
    abort "artifact name is not run-scoped: #{name}" unless name.include?("${{ github.run_id }}")
    abort "artifact name is attempt-scoped: #{name}" if name.include?("github.run_attempt")
    abort "rerun-all overwrite must be explicit" unless config["overwrite"] == true
  end
  %w[build prove-artifacts publish-cli].each do |job_name|
    job_steps = jobs.fetch(job_name).fetch("steps")
    upload_index = job_steps.index { |step| step["uses"].to_s.start_with?("actions/upload-artifact@") }
    abort "#{job_name} is missing its artifact upload" unless upload_index
    proof = job_steps.fetch(upload_index - 1)
    abort "#{job_name} can overwrite without a prior-attempt byte proof" \
      unless proof.fetch("name", "").start_with?("Prove any prior-attempt")
  end

  downloads = steps.select { |step| step["uses"].to_s.start_with?("actions/download-artifact@") }
  abort "recovery must download run-scoped handoffs" if downloads.empty?
  downloads.each do |step|
    config = step.fetch("with")
    selector = config["name"] || config["pattern"]
    abort "download is missing an artifact selector" unless selector
    abort "download selector is attempt-scoped: #{selector}" if selector.include?("github.run_attempt")
    abort "download is not explicitly bound to this run" unless config["run-id"] == "${{ github.run_id }}"
    abort "download is missing the current-run token" unless config["github-token"] == "${{ github.token }}"
  end

  prepare_run = jobs.fetch("prepare-release").fetch("steps")
    .find { |step| step["id"] == "release" }.fetch("run")
  reconcile = prepare_run.index("# Reconcile every pre-existing asset")
  upload = prepare_run.index(%q{releases/$release_id/assets?name=$name})
  abort "draft assets are uploaded before existing bytes are reconciled" \
    unless reconcile && upload && reconcile < upload
  creation_guard = prepare_run.index("# This is the first remote mutation")
  create = prepare_run.index(%q{gh api --method POST "repos/$GITHUB_REPOSITORY/releases"})
  abort "draft creation is missing its immediate release-frontier guard" \
    unless creation_guard && create && creation_guard < create

  publish_run = jobs.fetch("publish-release").fetch("steps")
    .find { |step| step["name"] == "Repeat every source, artifact, registry, and release proof" }
    .fetch("run")
  latest_guard = publish_run.index("# This is the only mutation in this job")
  final_asset_guard = publish_run.index("before-publish-assets", latest_guard || 0)
  final_frontier = publish_run.index("prove_release_frontier", final_asset_guard || 0)
  final_state_recheck = publish_run.index("before-publish-recheck.json", final_frontier || 0)
  final_tag_guard = publish_run.index("require_remote_tag", final_state_recheck || 0)
  publish = publish_run.index(%q{gh api --method PATCH})
  abort "make_latest is missing its immediate release-frontier guard" \
    unless latest_guard && publish && latest_guard < publish
  abort "publication is not guarded by fresh bytes, frontier, state, and tag identity" \
    unless final_asset_guard && final_frontier && final_state_recheck && final_tag_guard && publish \
      && final_asset_guard < final_frontier \
      && final_frontier < final_state_recheck \
      && final_state_recheck < final_tag_guard \
      && final_tag_guard < publish
  abort "public postcondition does not bind target metadata and exact frozen notes" \
    unless publish_run.include?(".target_commitish == $commit") \
      && publish_run.include?("cmp -s release-notes.md public-release-notes.md")

  publish_cli = jobs.fetch("publish-cli")
  checkout_paths = publish_cli.fetch("steps").map { |step| step.dig("with", "path") }.compact
  abort "publish-cli must use separate reviewed-control and frozen-source checkouts" \
    unless checkout_paths.include?("recovery-control") && checkout_paths.include?("release-source")
  publish_cli_run = publish_cli.fetch("steps")
    .find { |step| step["name"] == "Package, publish only if absent, and prove the registry checksum" }
    .fetch("run")
  abort "publish-cli bypasses the resilient API/index/order publisher" \
    unless publish_cli_run.include?("../recovery-control/scripts/publish-crate-resilient.sh")

  marketing_predeploy = marketing.fetch("steps")
    .find { |step| step["name"] == "Reprove the exact public release immediately before deployment" }
    &.fetch("run", "")
  abort "marketing predeploy does not reprove exact ID/name/target/body/latest" \
    unless marketing_predeploy \
      && marketing_predeploy.include?(".id == $release_id") \
      && marketing_predeploy.include?(".target_commitish == $commit") \
      && marketing_predeploy.include?("cmp -s expected-predeploy-release-notes.md actual-predeploy-release-notes.md") \
      && marketing_predeploy.include?("/releases/latest")

  docs_predeploy = docs.fetch("steps")
    .find { |step| step["name"] == "Reprove the public release and frozen tag immediately before deployment" }
    &.fetch("run", "")
  abort "docs predeploy does not reprove exact ID/name/target/body/latest" \
    unless docs_predeploy \
      && docs_predeploy.include?(%q{test "$release_id" = "$EXPECTED_RELEASE_ID"}) \
      && docs_predeploy.include?(".target_commitish == $commit") \
      && docs_predeploy.include?("cmp -s expected-predeploy-release-notes.md actual-predeploy-release-notes.md") \
      && docs_predeploy.include?("/releases/latest")

  production_run = jobs.fetch("prove-production").fetch("steps")
    .find { |step| step["name"] == "Prove the immutable source and every public release surface" }
    .fetch("run")
  abort "production installer proof is not bound to the frozen source checkout" \
    unless production_run.include?("cmp -s release-source/scripts/install.sh public-install.sh")
  abort "production homepage proof is not bound byte-for-byte to the frozen marketing source" \
    unless production_run.include?("cmp -s release-source/sites/marketing/index.html marketing-home.html")
  abort "production release proof does not bind exact target metadata and frozen notes" \
    unless production_run.include?(".target_commitish == $commit") \
      && production_run.include?("cmp -s expected-public-release-notes.md actual-public-release-notes.md")
' "$workflow"

simulation_root="$(mktemp -d)"
trap 'rm -rf -- "$simulation_root"' EXIT
artifact_store="$simulation_root/artifacts"
mkdir -p "$artifact_store"

store_ephemeral_artifact() {
  local name=$1 source=$2 overwrite=$3 destination
  destination="$artifact_store/$name"
  if [ -d "$destination" ]; then
    diff -qr "$source" "$destination" >/dev/null || return 41
    test "$overwrite" = true || return 42
    rm -rf -- "$destination"
  fi
  mkdir -p "$destination"
  cp -R "$source/." "$destination/"
}

make_artifact() {
  local directory=$1 payload=$2
  mkdir -p "$directory"
  printf '%s\n' "$payload" > "$directory/payload"
}

# Attempt 1 succeeds on three matrix runners. Attempt 2 reruns only the failed
# fourth runner; its downstream set must consume both attempts by run-scoped name.
simulation_run_id=4242
targets="x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu aarch64-apple-darwin"
for target in $targets; do
  generated="$simulation_root/attempt1-$target"
  make_artifact "$generated" "release-$target"
  store_ephemeral_artifact "recovery-$simulation_run_id-$target" "$generated" false
done
failed_target=x86_64-apple-darwin
generated="$simulation_root/attempt2-$failed_target"
make_artifact "$generated" "release-$failed_target"
store_ephemeral_artifact "recovery-$simulation_run_id-$failed_target" "$generated" false

combined="$simulation_root/attempt2-downstream"
mkdir -p "$combined"
for target in $targets $failed_target; do
  source="$artifact_store/recovery-$simulation_run_id-$target"
  test -d "$source" || fail "attempt2 could not consume an attempt1 artifact"
  cp "$source/payload" "$combined/$target"
done
test "$(find "$combined" -type f | wc -l | tr -d '[:space:]')" = 4 \
  || fail "mixed-attempt downstream set is incomplete"

# A rerun-all may replace only these run-scoped ephemeral artifacts, and only
# after the newly generated bytes compare exactly with the prior attempt.
for target in $targets $failed_target; do
  generated="$simulation_root/rerun-all-$target"
  make_artifact "$generated" "release-$target"
  store_ephemeral_artifact "recovery-$simulation_run_id-$target" "$generated" true
done
mismatch_target=x86_64-unknown-linux-gnu
stored_payload="$artifact_store/recovery-$simulation_run_id-$mismatch_target/payload"
stored_before="$(sha256_file "$stored_payload")"
mismatch="$simulation_root/rerun-all-mismatch"
make_artifact "$mismatch" tampered
if store_ephemeral_artifact \
  "recovery-$simulation_run_id-$mismatch_target" "$mismatch" true; then
  fail "rerun-all replaced a non-identical artifact"
fi
test "$(sha256_file "$stored_payload")" = "$stored_before" \
  || fail "failed rerun-all comparison mutated the stored artifact"

remote_assets="$simulation_root/remote-assets"
mkdir -p "$remote_assets"
reconcile_release_asset() {
  local state=$1 name=$2 local_file=$3 remote_file
  remote_file="$remote_assets/$name"
  if [ -f "$remote_file" ]; then
    cmp -s "$local_file" "$remote_file" || return 51
    return 0
  fi
  test "$state" = draft || return 52
  cp "$local_file" "$remote_file"
}

# Partial drafts continue only through absent or byte-exact assets.
local_exact="$simulation_root/local-exact"
printf '%s\n' exact > "$local_exact"
reconcile_release_asset draft absent "$local_exact"
absent_digest="$(sha256_file "$remote_assets/absent")"
reconcile_release_asset draft absent "$local_exact"
test "$(sha256_file "$remote_assets/absent")" = "$absent_digest"

printf '%s\n' original > "$remote_assets/mismatch"
mismatch_before="$(sha256_file "$remote_assets/mismatch")"
if reconcile_release_asset draft mismatch "$local_exact"; then
  fail "partial draft silently clobbered a mismatched asset"
fi
test "$(sha256_file "$remote_assets/mismatch")" = "$mismatch_before" \
  || fail "mismatched partial draft asset changed"
if reconcile_release_asset public missing-public "$local_exact"; then
  fail "completed release silently filled a missing asset"
fi
if reconcile_release_asset public mismatch "$local_exact"; then
  fail "completed release accepted a mismatched asset"
fi

echo "v1.0.1 recovery workflow contract: PASS (retry-failed, rerun-all, and draft failure injection)"
