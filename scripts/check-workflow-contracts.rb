#!/usr/bin/env ruby
# Static, network-free checks for workflow syntax, pinning, shell syntax, and
# shared-gate orchestration. This complements actionlint when it is installed.
require "open3"
require "pathname"
require "tmpdir"
require "yaml"

ROOT = Pathname.new(__dir__).join("..").realpath
WORKFLOW_DIR = ROOT.join(".github/workflows")
errors = []
run_blocks = 0

def walk(value, &block)
  case value
  when Hash
    yield value
    value.each_value { |child| walk(child, &block) }
  when Array
    value.each { |child| walk(child, &block) }
  end
end

workflows = Dir[WORKFLOW_DIR.join("*.{yml,yaml}")].sort
workflows.each do |path|
  begin
    document = YAML.load_file(path)
  rescue StandardError => e
    errors << "#{path}: invalid YAML: #{e.message}"
    next
  end

  walk(document) do |mapping|
    concurrency = mapping["concurrency"]
    if concurrency.is_a?(Hash) && concurrency["group"].to_s.start_with?("axocoatl-production-")
      unless concurrency["queue"] == "max" && concurrency["cancel-in-progress"] == false
        errors << "#{path}: production concurrency #{concurrency["group"].inspect} must use queue: max and cancel-in-progress: false"
      end
    end

    if mapping["uses"]
      reference = mapping["uses"].to_s
      unless reference.start_with?("./") || reference.match?(/\A[^\s@]+@\h{40}\z/)
        errors << "#{path}: action is not pinned to a full commit: #{reference}"
      end
    end

    next unless mapping["run"]
    run_blocks += 1
    body = mapping["run"].to_s.gsub(/\$\{\{.*?\}\}/m, "GH_EXPRESSION")
    file = nil
    Dir.mktmpdir("axocoatl-workflow-shell") do |directory|
      file = File.join(directory, "run.sh")
      File.write(file, body + "\n")
      _stdout, stderr, status = Open3.capture3("bash", "-n", file)
      errors << "#{path}: invalid run-block shell syntax: #{stderr.strip}" unless status.success?
    end
  end
end

scripts = `git -C #{ROOT} ls-files --cached --others --exclude-standard -- '*.sh'`
          .lines.map(&:strip).reject(&:empty?).uniq.sort
scripts.each do |relative|
  path = ROOT.join(relative)
  interpreter = case path.read.lines.first.to_s
                when /#!\/bin\/sh/ then "sh"
                when /#!.*bash/ then "bash"
                end
  next unless interpreter
  _stdout, stderr, status = Open3.capture3(interpreter, "-n", path.to_s)
  errors << "#{relative}: invalid #{interpreter} syntax: #{stderr.strip}" unless status.success?
end

contracts = {
  ".github/workflows/ci.yml" => [
    "./scripts/run-actionlint.sh",
    "./scripts/check-workflow-contracts.rb",
    "./scripts/verify-marketing-gate.sh portable",
    "./scripts/verify-film-gate.sh candidate",
    "./scripts/verify-docs-gate.sh",
    "./scripts/verify-native-gate.sh",
    "./scripts/verify-product-browser-gate.sh",
    "./scripts/verify-cross-linux-gate.sh build",
    "cargo check --locked --workspace --all-targets --all-features --jobs 1",
  ],
  ".github/workflows/release.yml" => [
    "./scripts/verify-marketing-gate.sh source-bound",
    "./scripts/verify-docs-gate.sh",
    "./scripts/verify-native-gate.sh",
    "./scripts/verify-release-order.sh",
    "./scripts/publish-crate-resilient.sh",
    "./scripts/prove-release-crates.sh",
    "./scripts/release-plan.sh",
    "./scripts/verify-product-browser-gate.sh",
  ],
  ".github/workflows/marketing-deploy.yml" => [
    "./scripts/verify-public-release.sh",
    "./scripts/verify-marketing-gate.sh source-bound",
    "./scripts/verify-marketing-gate.sh portable",
    "if: inputs.release_tag != ''",
    "if: inputs.release_tag == ''",
  ],
  ".github/workflows/docs-deploy.yml" => [
    "./scripts/verify-public-release.sh",
    "./scripts/verify-docs-gate.sh",
  ],
  "scripts/verify-native-gate.sh" => [
    "cargo +1.88.0 check --locked --workspace --all-targets --all-features --jobs 1",
    "cargo clippy --locked --workspace --all-targets --all-features --jobs 1",
    "cargo test --locked --workspace --jobs 1",
    "cargo test --locked --doc --workspace --jobs 1",
    "cargo build --locked --release -p axocoatl-cli --jobs 1",
  ],
}

contracts.each do |relative, markers|
  source = ROOT.join(relative).read
  markers.each do |marker|
    errors << "#{relative}: missing shared contract invocation #{marker.inspect}" unless source.include?(marker)
  end
end

forbidden = {
  ".github/workflows/ci.yml" => [
    "node demo/one-app/films/verify-film-set.mjs",
    "node sites/marketing/scripts/validate.mjs",
    "cargo clippy --locked",
    "npm run check:content",
  ],
  ".github/workflows/release.yml" => [
    "node demo/one-app/films/verify-film-set.mjs",
    "node sites/marketing/scripts/validate.mjs",
    "cargo clippy --locked",
    "npm run check:content",
    "publishable_crates=",
  ],
  ".github/workflows/marketing-deploy.yml" => [
    "node demo/one-app/films/verify-film-set.mjs",
    "node sites/marketing/scripts/validate.mjs",
  ],
  ".github/workflows/docs-deploy.yml" => ["npm run check:content"],
}
forbidden.each do |relative, markers|
  source = ROOT.join(relative).read
  markers.each do |marker|
    errors << "#{relative}: duplicates shared gate implementation #{marker.inspect}" if source.include?(marker)
  end
end

release = YAML.load_file(ROOT.join(".github/workflows/release.yml"))
jobs = release.fetch("jobs")
expected_needs = {
  "build" => %w[test-gate marketing-site docs-site product-browser],
  "prove-artifacts" => %w[build],
  "prepare-release" => %w[prove-artifacts],
  "publish-crates" => %w[prepare-release],
  "release" => %w[prepare-release publish-crates],
  "deploy-marketing" => %w[release],
  "deploy-docs" => %w[release],
}
expected_needs.each do |job, expected|
  actual = Array(jobs.dig(job, "needs"))
  errors << ".github/workflows/release.yml: #{job}.needs is #{actual.inspect}, expected #{expected.inspect}" \
    unless actual == expected
end

release_steps = lambda do |job|
  jobs.fetch(job).fetch("steps")
end
step_position = lambda do |job, name|
  release_steps.call(job).index { |step| step["name"] == name }
end
[
  ["prepare-release", "Reprove release order before draft mutation", "Create or reconcile the exact release without overwriting"],
  ["publish-crates", "Reprove release order before crates.io publication", "Publish in dependency order"],
  ["release", "Reprove release order immediately before latest mutation", "Publish verified release"],
].each do |job, guard, mutation|
  guard_position = step_position.call(job, guard)
  mutation_position = step_position.call(job, mutation)
  unless guard_position && mutation_position && guard_position + 1 == mutation_position
    errors << ".github/workflows/release.yml: #{guard.inspect} must immediately precede #{mutation.inspect}"
  end
end

{
  "build" => ["Prove any prior-attempt platform artifact is byte-identical", "Upload proved platform artifact"],
  "prove-artifacts" => ["Prove any prior-attempt verified set is byte-identical", "Upload verified release set"],
}.each do |job, (proof, upload)|
  proof_position = step_position.call(job, proof)
  upload_position = step_position.call(job, upload)
  unless proof_position && upload_position && proof_position + 1 == upload_position
    errors << ".github/workflows/release.yml: #{proof.inspect} must immediately precede #{upload.inspect}"
  end
  permissions = jobs.fetch(job).fetch("permissions", {})
  unless permissions["actions"] == "read" && permissions["contents"] == "read"
    errors << ".github/workflows/release.yml: #{job} needs explicit actions: read and contents: read"
  end
end
crate_proof_position = step_position.call("publish-crates", "Prove any prior-attempt crate proof is byte-identical")
crate_upload_position = step_position.call("publish-crates", "Preserve the reviewed crate publication proof")
unless crate_proof_position && crate_upload_position && crate_proof_position + 1 == crate_upload_position
  errors << ".github/workflows/release.yml: crate retry byte proof must immediately precede its artifact upload"
end
publish_permissions = jobs.fetch("publish-crates").fetch("permissions", {})
unless publish_permissions["actions"] == "read" && publish_permissions["contents"] == "read"
  errors << ".github/workflows/release.yml: publish-crates needs explicit actions: read and contents: read"
end
prepare_permissions = jobs.fetch("prepare-release").fetch("permissions", {})
unless prepare_permissions["actions"] == "read" && prepare_permissions["contents"] == "write"
  errors << ".github/workflows/release.yml: prepare-release needs actions: read and contents: write"
end

prepare_source = ROOT.join(".github/workflows/release.yml").read
[
  "Prior-attempt artifact $ARTIFACT_NAME is not byte-identical.",
  "Prior-attempt verified release set is not byte-identical.",
  "Existing release asset $name differs; refusing to overwrite it.",
  'releases/$release_id/assets?name=$name',
  "https://uploads.github.com/",
].each do |marker|
  errors << ".github/workflows/release.yml: missing fail-closed rerun contract #{marker.inspect}" \
    unless prepare_source.include?(marker)
end
prepare_reconcile = release_steps.call("prepare-release").find do |step|
  step["name"] == "Create or reconcile the exact release without overwriting"
end&.fetch("run", "").to_s
[
  ".draft == $expected_draft",
  "Published release $GITHUB_REF_NAME is missing $name; refusing to mutate it.",
  "Published release $GITHUB_REF_NAME is no longer latest; refusing a stale rerun.",
].each do |marker|
  errors << ".github/workflows/release.yml: rerun-all exact-public contract lacks #{marker.inspect}" \
    unless prepare_reconcile.include?(marker)
end
[
  "overwrite_files: true",
  "--clobber",
  "gh release upload",
].each do |marker|
  errors << ".github/workflows/release.yml: release retry path may overwrite reviewed remote state via #{marker.inspect}" \
    if prepare_source.include?(marker)
end

final_release_proof = release_steps.call("release").find do |step|
  step["name"] == "Prove the release is public and not a prerelease"
end
release_permissions = jobs.fetch("release").fetch("permissions", {})
unless release_permissions["actions"] == "read" && release_permissions["contents"] == "write"
  errors << ".github/workflows/release.yml: release needs actions: read and contents: write"
end
release_state_proof = release_steps.call("release").find do |step|
  step["name"] == "Inspect release state, download, and reverify assets"
end
unless release_state_proof \
    && release_state_proof.fetch("run", "").include?("diff -r --no-dereference reviewed uploaded") \
    && release_steps.call("release").any? { |step| step.dig("with", "name") == "verified-release-${{ github.run_id }}-${{ github.ref_name }}" }
  errors << ".github/workflows/release.yml: final release proof is not byte-bound to the run-scoped verified artifact"
end
unless release_steps.call("release").any? { |step| step.dig("with", "name") == "published-crates-${{ github.run_id }}-${{ github.ref_name }}" }
  errors << ".github/workflows/release.yml: final release job does not consume the run-scoped crate proof"
end
pre_latest = release_steps.call("release").find do |step|
  step["name"] == "Reprove release order immediately before latest mutation"
end
publish_release = release_steps.call("release").find do |step|
  step["name"] == "Publish verified release"
end
unless pre_latest && pre_latest.fetch("run", "").include?("./scripts/prove-release-crates.sh")
  errors << ".github/workflows/release.yml: latest mutation is not preceded by exact API and sparse-index crate proofs"
end
publish_source = publish_release&.fetch("run", "").to_s
[
  ".target_commitish == $commit",
  "cmp -s release-notes.md before-publish-notes.md",
  "before-publish-assets",
  "before-publish-release.json.state before-publish-recheck.json.state",
  "./scripts/verify-release-order.sh",
  "git fetch --no-tags --force origin",
  "gh api --method PATCH",
].each do |marker|
  errors << ".github/workflows/release.yml: final exact-ID publication guard lacks #{marker.inspect}" \
    unless publish_source.include?(marker)
end
asset_guard = publish_source.index("before-publish-assets")
frontier_guard = publish_source.index("./scripts/verify-release-order.sh", asset_guard || 0)
state_recheck = publish_source.index("before-publish-recheck.json", frontier_guard || 0)
tag_guard = publish_source.index("git fetch --no-tags --force origin", state_recheck || 0)
release_mutation = publish_source.index("gh api --method PATCH", tag_guard || 0)
unless asset_guard && frontier_guard && state_recheck && tag_guard && release_mutation \
    && asset_guard < frontier_guard \
    && frontier_guard < state_recheck \
    && state_recheck < tag_guard \
    && tag_guard < release_mutation
  errors << ".github/workflows/release.yml: final asset/frontier/state/tag guards are not ordered immediately before publication"
end
unless final_release_proof && final_release_proof.fetch("run", "").include?("./scripts/prove-release-crates.sh")
  errors << ".github/workflows/release.yml: public release postcondition does not reprove crate installability"
end
unless final_release_proof \
    && final_release_proof.fetch("run", "").include?(".target_commitish == $commit") \
    && final_release_proof.fetch("run", "").include?("cmp -s release-notes.md public-release-notes.md") \
    && final_release_proof.fetch("run", "").include?("diff -r --no-dereference reviewed public-assets")
  errors << ".github/workflows/release.yml: public postcondition lacks exact metadata, notes, and asset-byte proofs"
end
unless final_release_proof && final_release_proof.fetch("run", "").include?("/releases/latest")
  errors << ".github/workflows/release.yml: final public release proof does not verify GitHub's latest pointer"
end
unless step_position.call("test-gate", "Require a monotonic stable release version")
  errors << ".github/workflows/release.yml: normal release gate lacks the stable-SemVer frontier proof"
end

release_python = release_steps.call("build").find do |step|
  step["uses"] == "actions/setup-python@e797f83bcb11b83ae66e0230d6156d7c80228e7c"
end
unless release_python \
    && release_python.dig("with", "python-version") == "3.13.7" \
    && release_python.dig("with", "check-latest") == false
  errors << ".github/workflows/release.yml: release build lacks the pinned Python 3.13.7 archive runtime"
end

crate_publisher = ROOT.join("scripts/publish-crate-resilient.sh").read
order_proof = crate_publisher.index('"$script_dir/verify-release-order.sh" "$release_tag"')
publish_call = crate_publisher.index("if publish_archive; then")
unless order_proof && publish_call && order_proof < publish_call
  errors << "scripts/publish-crate-resilient.sh: release order is not re-proved before upload retries"
end
if crate_publisher.include?("require_remote_tag ||")
  errors << "scripts/publish-crate-resilient.sh: conditional function invocation disables fail-fast tag refresh semantics"
end
[
  '|| fail "could not refresh remote tag',
  '|| fail "could not resolve refreshed remote tag',
].each do |marker|
  errors << "scripts/publish-crate-resilient.sh: remote tag refresh lacks explicit guard #{marker.inspect}" \
    unless crate_publisher.include?(marker)
end

publish_crates_source = release_steps.call("publish-crates")
  .find { |step| step["name"] == "Publish in dependency order" }
  &.fetch("run", "").to_s
if publish_crates_source.include?("git tag --merged")
  errors << ".github/workflows/release.yml: merged local tags must not define the prior public package baseline"
end
baseline_resolver = publish_crates_source.index("./scripts/resolve-previous-public-release.sh")
baseline_fetch = publish_crates_source.index("+refs/tags/$previous_tag:$previous_ref", baseline_resolver || 0)
baseline_ancestor = publish_crates_source.index('git merge-base --is-ancestor "$previous_commit"', baseline_fetch || 0)
package_diff = publish_crates_source.index('git diff --name-only', baseline_ancestor || 0)
unless baseline_resolver && baseline_fetch && baseline_ancestor && package_diff \
    && baseline_resolver < baseline_fetch \
    && baseline_fetch < baseline_ancestor \
    && baseline_ancestor < package_diff
  errors << ".github/workflows/release.yml: CLI-only scope is not based on an ancestor-proved prior public stable release"
end
all_packages_preflight = publish_crates_source.index("# Discover every pre-existing version")
preflight_sparse_proof = publish_crates_source.index("./scripts/prove-crate-index.sh", all_packages_preflight || 0)
first_publish_helper = publish_crates_source.index("./scripts/publish-crate-resilient.sh", preflight_sparse_proof || 0)
unless all_packages_preflight && preflight_sparse_proof && first_publish_helper \
    && all_packages_preflight < preflight_sparse_proof \
    && preflight_sparse_proof < first_publish_helper
  errors << ".github/workflows/release.yml: every pre-existing package must pass API and sparse-index proof before the first upload"
end

release_order_verifier = ROOT.join("scripts/verify-release-order.sh").read
[
  'releases?per_page=100&page=$page',
  "jq --slurp 'add // []'",
].each do |marker|
  errors << "scripts/verify-release-order.sh: missing complete release-frontier proof #{marker.inspect}" \
    unless release_order_verifier.include?(marker)
end
if release_order_verifier.include?("/releases/latest")
  errors << "scripts/verify-release-order.sh: movable /releases/latest pointer must not define the release frontier"
end

public_release_helper = ROOT.join("scripts/verify-public-release.sh").read
[
  'git fetch --no-tags --force "$remote_name"',
  '"$remote_commit" == "$called_sha"',
  'releases/latest',
  '"$release_id" != "$latest_id"',
  '"${GITHUB_ACTIONS:-}" != true && -n "${AXO_PUBLIC_RELEASE_REMOTE:-}"',
  '"${GITHUB_ACTIONS:-}" != true && -n "${AXO_PUBLIC_RELEASE_FIXTURE:-}"',
].each do |marker|
  errors << "scripts/verify-public-release.sh: missing non-bypassable remote-tag contract #{marker.inspect}" \
    unless public_release_helper.include?(marker)
end

unless errors.empty?
  errors.each { |error| warn "workflow-contracts: #{error}" }
  warn "Workflow contract failed with #{errors.length} error(s)."
  exit 1
end

puts "Workflow contract: PASS (#{workflows.length} YAML files, #{run_blocks} run blocks, #{scripts.length} shell scripts)"
