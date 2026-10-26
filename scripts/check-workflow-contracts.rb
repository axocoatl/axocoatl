#!/usr/bin/env ruby
# Static, network-free checks for workflow syntax, pinning, shell syntax, and
# shared-gate orchestration. The actionlint wrapper supplies its pinned
# ShellCheck binary on every supported development and CI host.
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

# Every action a workflow, a composite action or a docs example uses is a
# reviewed release that runs on Node.js 24 or is a composite action: GitHub's
# runners force Node 20 actions onto Node 24 and warn on every job. To move
# one, check its action.yml's `runs.using` at the new commit and change it here.
REVIEWED_ACTIONS = {
  "actions/checkout" => "fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09", # v5.1.0, node24
  "actions/setup-node" => "249970729cb0ef3589644e2896645e5dc5ba9c38", # v6.5.0, node24
  "actions/setup-go" => "924ae3a1cded613372ab5595356fb5720e22ba16", # v6.5.0, node24
  "actions/setup-python" => "e797f83bcb11b83ae66e0230d6156d7c80228e7c", # v6.0.0, node24
  "actions/upload-artifact" => "cf430e030ddbb5b0abf93d22962f4752f3646cd9", # v7.0.2, node24
  "actions/download-artifact" => "9000827ccba6bdab643e8b6fd33ac0654aef8333", # v8.0.2, node24
  "actions/attest-build-provenance" => "4d101475d8b20a2381f78447822ac1eab6504dd8", # v4.2.2, composite
  "Swatinem/rust-cache" => "6323deb102c322ba6fcbdcafc7e3dddab59af2b6", # v2.9.2, node24
  "dtolnay/rust-toolchain" => "4360b52568e2003a75bf9bc1d59f33a8e3fc893c", # composite
  "cloudflare/wrangler-action" => "953926a2e2182532811c01a25e53647d93bf07c0", # v4.1.3, node24
}.freeze
reviewed_action = lambda do |reference|
  name, sha = reference.split("@", 2)
  REVIEWED_ACTIONS[name] == sha
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
      unless reference.start_with?("./") || reviewed_action.call(reference)
        errors << "#{path}: #{reference} is not a reviewed Node 24 or composite release (REVIEWED_ACTIONS)"
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

# Composite actions under actions/: actionlint reads only workflows, so
# their structure is checked here and their scripts by run-actionlint.sh's
# pinned ShellCheck. A step's script never expands an expression (an input
# reaches it only through env, so no input becomes shell code), names its
# shell, and pins any action it uses.
composite_actions = Dir[ROOT.join("actions/*/action.{yml,yaml}")].sort
composite_actions.each do |path|
  begin
    document = YAML.load_file(path)
  rescue StandardError => e
    errors << "#{path}: invalid YAML: #{e.message}"
    next
  end
  runs = document.is_a?(Hash) ? document["runs"] : nil
  unless runs.is_a?(Hash) && runs["using"] == "composite" && runs["steps"].is_a?(Array)
    errors << "#{path}: must be a composite action with steps"
    next
  end
  declared = document["inputs"].is_a?(Hash) ? document["inputs"].keys : []
  File.read(path).scan(/\binputs\.([A-Za-z0-9_-]+)/).flatten.uniq.each do |name|
    errors << "#{path}: uses undeclared input #{name}" unless declared.include?(name)
  end
  runs["steps"].each_with_index do |step, index|
    label = "#{path}: step #{index + 1}"
    if step["uses"]
      reference = step["uses"].to_s
      unless reference.start_with?("./") || reference.match?(/\A[^\s@]+@\h{40}\z/)
        errors << "#{label}: action is not pinned to a full commit: #{reference}"
      end
      unless reference.start_with?("./") || reviewed_action.call(reference)
        errors << "#{label}: #{reference} is not a reviewed Node 24 or composite release (REVIEWED_ACTIONS)"
      end
    end
    next unless step["run"]
    run_blocks += 1
    errors << "#{label}: a run step must name its shell" unless step["shell"]
    if step["run"].to_s.include?("${{")
      errors << "#{label}: its script expands an expression; pass it through env"
    end
    Dir.mktmpdir("axocoatl-action-shell") do |directory|
      file = File.join(directory, "run.sh")
      File.write(file, step["run"].to_s + "\n")
      _stdout, stderr, status = Open3.capture3("bash", "-n", file)
      errors << "#{label}: invalid run-block shell syntax: #{stderr.strip}" unless status.success?
    end
  end
end

# The workflow examples people copy from the docs pin the same releases.
docs_examples = `git -C #{ROOT} ls-files --cached --others --exclude-standard -- 'sites/docs/src/content/*.md' 'sites/docs/src/content/*.mdx' 'README.md'`
                .lines.map(&:strip).reject(&:empty?).uniq.sort
docs_examples.each do |relative|
  ROOT.join(relative).read(encoding: "UTF-8").scan(/^\s*-?\s*uses:\s*([^\s#]+@[^\s#]+)/).flatten.each do |reference|
    next if reference.start_with?("./", "axocoatl/")
    unless reviewed_action.call(reference)
      errors << "#{relative}: its example uses #{reference}, not a reviewed Node 24 release (REVIEWED_ACTIONS)"
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

actionlint_runner = ROOT.join("scripts/run-actionlint.sh").read
[
  "shellcheck_version=v0.11.0",
  "339b930feb1ea764467013cc1f72d09cd6b869ebf1013296ba9055ab2ffbd26f",
  "c2c15e08df0e8fbc374c335b230a7ee958c313fa5714817a59aa59f1aa594f51",
  "68a8133197a50beb8803f8d42f9908d1af1c5540d4bb05fdfca8c1fa47decefc",
  "b7af85e41cc99489dcc21d66c6d5f3685138f06d34651e6d34b42ec6d54fe6f6",
  "https://github.com/koalaman/shellcheck/releases/download/",
  "unset SHELLCHECK_OPTS",
  '-shellcheck "$shellcheck_path"',
].each do |marker|
  errors << "scripts/run-actionlint.sh: missing pinned ShellCheck contract #{marker.inspect}" \
    unless actionlint_runner.include?(marker)
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
    "./scripts/release-gate/newuser-linux.sh",
    "./scripts/release-gate/newuser-mac-ci.sh",
    "./scripts/release-gate/test-release-gate.sh",
    "node --test scripts/release-gate/*.test.mjs",
    "./scripts/test-podman-suites.sh check",
    "./scripts/test-podman-suites.sh run",
    "./scripts/test-exec-supervisor-runtime.sh run",
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
    "./scripts/check-no-live-verify.sh target/${{ matrix.target }}/release/axocoatl",
    "node scripts/release-gate/check-evidence.mjs",
    "./scripts/release-gate/newuser-linux.sh",
  ],
  ".github/workflows/marketing-deploy.yml" => [
    "./scripts/verify-public-release.sh",
    "./scripts/verify-marketing-gate.sh source-bound /tmp/axocoatl-marketing --installer-from-release",
    "./scripts/verify-marketing-gate.sh portable /tmp/axocoatl-marketing --installer-from-release",
    "if: inputs.release_tag != ''",
    "if: inputs.release_tag == ''",
  ],
  ".github/workflows/docs-deploy.yml" => [
    "./scripts/verify-public-release.sh",
    "./scripts/verify-docs-gate.sh",
  ],
  ".github/workflows/security.yml" => [
    "node scripts/audit-gate.mjs --package-lock-only",
  ],
  "scripts/verify-docs-gate.sh" => [
    "node ./scripts/audit-gate.mjs",
    'node --test "$repo_root/scripts/check-copy-paste.test.mjs"',
    'node "$repo_root/scripts/check-copy-paste.mjs"',
  ],
  "scripts/verify-product-browser-gate.sh" => [
    'node "$repo_root/scripts/check-copy-paste.mjs" --axocoatl "$binary"',
  ],
  "scripts/verify-native-gate.sh" => [
    "cargo +1.88.0 check --locked --workspace --all-targets --all-features --jobs 1",
    "cargo clippy --locked --workspace --all-targets --all-features --jobs 1",
    "cargo test --locked --workspace --jobs 1",
    "cargo test --locked --doc --workspace --jobs 1",
    # The tests pass under Ubuntu's login umask, not only under 022.
    "(umask 002 && cargo test --locked --workspace --jobs 1",
    "cargo build --locked --release -p axocoatl-cli --jobs 1",
    './scripts/check-no-live-verify.sh "$release_target_dir/release/axocoatl"',
  ],
  "scripts/verify-cross-linux-gate.sh" => [
    "./scripts/check-no-live-verify.sh",
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

# The verification-only `live-verify` cargo feature (the ChatGPT broker's
# stale-access switch) is declared by axocoatl-daemon and axocoatl-cli only,
# is off by default, is turned on by no other manifest, and is named by no
# workflow, file under scripts/ or shell script but the checks that refuse
# it. None of those runs a cargo command that builds a binary (build, run,
# install, test, bench, nextest, ...) with every feature either: only
# `cargo check`, `cargo clippy` and `cargo doc`, which build no binary, may
# use `--all-features`, to type-check and lint it.
live_verify_declarations = {
  "crates/axocoatl-daemon/Cargo.toml" => ["live-verify = []"],
  "axocoatl-cli/Cargo.toml" => ['live-verify = ["axocoatl-daemon/live-verify"]'],
}
uncommented = lambda do |text|
  text.lines.map { |line| line.sub(/#.*/, "").strip }.reject(&:empty?)
end
manifests = `git -C #{ROOT} ls-files --cached --others --exclude-standard -- 'Cargo.toml' '*/Cargo.toml'`
            .lines.map(&:strip).reject(&:empty?).uniq.sort
live_verify_declarations.each_key do |relative|
  errors << "#{relative}: missing from the manifests checked for live-verify" unless manifests.include?(relative)
end
manifests.each do |relative|
  lines = uncommented.call(ROOT.join(relative).read(encoding: "UTF-8"))
  naming = lines.select { |line| line.include?("live-verify") }
  expected = live_verify_declarations.fetch(relative, [])
  unless naming == expected
    errors << "#{relative}: names the verification-only live-verify feature as #{naming.inspect}, expected #{expected.inspect}"
  end
  default = lines.find { |line| line.match?(/\Adefault\s*=/) }
  if expected.any? && default != "default = []"
    errors << "#{relative}: default features must be empty (#{default.inspect}) so live-verify stays off"
  end
end
tracked_shell_scripts = `git -C #{ROOT} ls-files --cached --others --exclude-standard -- '*.sh'`
                        .lines.map(&:strip).reject(&:empty?).map { |relative| ROOT.join(relative).to_s }
build_sources = (Dir[WORKFLOW_DIR.join("*.{yml,yaml}")] + Dir[ROOT.join("scripts/**/*")] +
                 tracked_shell_scripts).uniq.sort
all_features_allowed = %w[check clippy doc]
build_sources.each do |path|
  relative = Pathname.new(path).relative_path_from(ROOT).to_s
  next if %w[scripts/check-workflow-contracts.rb scripts/check-no-live-verify.sh
             scripts/test-workflow-contracts.sh].include?(relative)
  next unless File.file?(path)
  text = File.read(path, encoding: "UTF-8")
  if uncommented.call(text.gsub("check-no-live-verify.sh", "")).any? { |line| line.include?("live-verify") }
    errors << "#{relative}: names the verification-only live-verify feature"
  end
  lines = text.lines
  lines.each_with_index do |line, index|
    subcommand = line[/\bcargo\s+(?:\+\S+\s+)?([a-z][a-z0-9-]*)/, 1]
    next if subcommand.nil? || all_features_allowed.include?(subcommand)
    # The command, with the flag lines that continue it.
    command = line
    following = index + 1
    while command.rstrip.end_with?("\\") && lines[following].to_s.match?(/\A\s*-/)
      command += lines[following]
      following += 1
    end
    if command.include?("--all-features")
      errors << "#{relative}: runs cargo #{subcommand} with --all-features, which would build live-verify into a binary (only #{all_features_allowed.join(", ")} may): #{line.strip}"
    end
  end
end

release = YAML.load_file(ROOT.join(".github/workflows/release.yml"))
jobs = release.fetch("jobs")
expected_needs = {
  "build" => %w[test-gate marketing-site docs-site product-browser new-user-evidence],
  "prove-artifacts" => %w[build],
  "new-user-linux" => %w[build],
  "prepare-release" => %w[prove-artifacts new-user-linux],
  "publish-crates" => %w[prepare-release],
  "release" => %w[prepare-release publish-crates],
  "deploy-marketing" => %w[release marketing-site],
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

# The clean-machine new-user gate (docs/RELEASE_GATE.md): nothing is built
# without signed evidence, the draft waits for Tier A on the release's own
# archives, and the publish job checks the evidence again before touching the
# release. The override input reaches the check only through env, and only a
# workflow_dispatch run can carry it.
evidence_check = "node scripts/release-gate/check-evidence.mjs"
evidence_steps = lambda do |job|
  release_steps.call(job).select { |step| step["run"].to_s.include?(evidence_check) }
end
if evidence_steps.call("new-user-evidence").empty?
  errors << ".github/workflows/release.yml: new-user-evidence does not run #{evidence_check}"
end
publish_check = release_steps.call("release").index { |step| step["run"].to_s.include?(evidence_check) }
publish_mutation = release_steps.call("release").index { |step| step["name"] == "Publish verified release" }
unless publish_check && publish_mutation && publish_check < publish_mutation
  errors << ".github/workflows/release.yml: the release job must run #{evidence_check} before \"Publish verified release\""
end
unless Array(jobs.dig("new-user-linux", "steps")).any? { |step| step["run"].to_s.include?("./scripts/release-gate/newuser-linux.sh") }
  errors << ".github/workflows/release.yml: new-user-linux does not run ./scripts/release-gate/newuser-linux.sh"
end
%w[new-user-evidence release].each do |job|
  evidence_steps.call(job).each do |step|
    unless step.dig("env", "NEW_USER_GATE_OVERRIDE") == "${{ inputs.new_user_gate_override }}" \
        && step["run"].to_s.include?('--override "${NEW_USER_GATE_OVERRIDE:-}"') \
        && step["run"].to_s.include?('--event "$EVENT_NAME"')
      errors << ".github/workflows/release.yml: #{job}'s evidence check must take the override from env NEW_USER_GATE_OVERRIDE and pass the event"
    end
    # Only a login listed in .axocoatl/gate-override-actors may override:
    # the check must see who started the run and who ran it.
    unless step.dig("env", "ACTOR") == "${{ github.actor }}" \
        && step.dig("env", "TRIGGERING_ACTOR") == "${{ github.triggering_actor }}" \
        && step["run"].to_s.include?('--actor "$ACTOR"') \
        && step["run"].to_s.include?('--triggering-actor "$TRIGGERING_ACTOR"')
      errors << ".github/workflows/release.yml: #{job}'s evidence check must pass --actor and --triggering-actor from github.actor and github.triggering_actor"
    end
  end
end
release_triggers = release.fetch(true, release["on"]) || {}
unless release_triggers.is_a?(Hash) && release_triggers.keys.sort == %w[push workflow_dispatch] \
    && Array(release_triggers.dig("push", "tags")) == ["v*"] \
    && release_triggers.dig("workflow_dispatch", "inputs").is_a?(Hash) \
    && release_triggers.dig("workflow_dispatch", "inputs").keys == ["new_user_gate_override"]
  errors << ".github/workflows/release.yml: triggers must be v* tag pushes and a workflow_dispatch whose only input is new_user_gate_override"
end
jobs.each do |job_name, job|
  Array(job["steps"]).each do |step|
    next unless step["run"].to_s.match?(/\$\{\{\s*inputs\./)
    errors << ".github/workflows/release.yml: #{job_name} step #{step["name"].inspect} expands an input in its script; pass it through env"
  end
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

# Build provenance: prove-artifacts, which verified the four archives, signs
# an attestation of each and proves it verifies for this workflow at this
# tag; only that job may ask for an OIDC token.
attest_steps = release_steps.call("prove-artifacts")
attest = attest_steps.index { |step| step["uses"].to_s.start_with?("actions/attest-build-provenance@") }
verify_set = attest_steps.index { |step| step["name"] == "Verify checksums, contents, and completeness" }
attest_verify = attest_steps.index { |step| step["run"].to_s.include?("gh attestation verify") }
unless attest && verify_set && attest_verify && verify_set < attest && attest < attest_verify
  errors << ".github/workflows/release.yml: prove-artifacts must attest the verified archives (actions/attest-build-provenance) and then run gh attestation verify"
end
subjects = attest ? attest_steps[attest].dig("with", "subject-path").to_s : ""
%w[x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin].each do |target|
  errors << ".github/workflows/release.yml: the attestation does not name the #{target} archive" \
    unless subjects.include?("-#{target}.tar.gz")
end
verify_source = attest_verify ? attest_steps[attest_verify]["run"].to_s : ""
["--signer-workflow", "--source-ref", "--source-digest", "--deny-self-hosted-runners"].each do |flag|
  errors << ".github/workflows/release.yml: gh attestation verify lacks #{flag}" unless verify_source.include?(flag)
end
attest_permissions = jobs.fetch("prove-artifacts").fetch("permissions", {})
unless attest_permissions["attestations"] == "write" && attest_permissions["id-token"] == "write"
  errors << ".github/workflows/release.yml: prove-artifacts needs attestations: write and id-token: write"
end
jobs.each do |job_name, job|
  next if job_name == "prove-artifacts"
  if job.is_a?(Hash) && job.dig("permissions", "id-token")
    errors << ".github/workflows/release.yml: #{job_name} must not ask for an OIDC token (id-token)"
  end
end

# The browser gate: ci.yml's product-browser legs, which release.yml repeats
# before anything is built: the release build in Chromium on x86_64 and
# aarch64 Linux, and in WebKit on x86_64 Linux and macOS.
ci = YAML.load_file(ROOT.join(".github/workflows/ci.yml"))
{ "ci.yml" => ci, "release.yml" => release }.each do |file, document|
  job = document.dig("jobs", "product-browser") || {}
  legs = Array(job.dig("strategy", "matrix", "include"))
  wanted = [%w[chromium ubuntu-24.04], %w[chromium ubuntu-24.04-arm], %w[webkit ubuntu-24.04], %w[webkit macos-26]]
  unless legs.map { |leg| [leg["engine"], leg["runner"]] }.sort == wanted.sort
    errors << ".github/workflows/#{file}: product-browser must run Chromium on ubuntu-24.04 and ubuntu-24.04-arm and WebKit on ubuntu-24.04 and macos-26"
  end
  gate = Array(job["steps"]).find { |step| step["run"].to_s.include?("./scripts/verify-product-browser-gate.sh") }
  unless gate && gate.dig("env", "AXO_BROWSER_PROFILE") == "release" \
      && gate.dig("env", "AXO_BROWSER_ENGINE") == "${{ matrix.engine }}"
    errors << ".github/workflows/#{file}: product-browser must run the release build in each leg's engine"
  end
end

# Every job that pulls with Podman or runs the new-user gate first removes
# CI mirror routing, so pinned manifests cannot select an incomplete cache.
{ "ci.yml" => ci, "release.yml" => release }.each do |file, document|
  document.fetch("jobs").each do |job_name, job|
    steps = Array(job["steps"])
    pulls = steps.index do |step|
      text = step["run"].to_s
      text.include?("podman pull") || text.include?("newuser-linux.sh") || text.include?("test-podman-suites.sh prepare") \
        || text.include?("test-exec-supervisor-runtime.sh prepare")
    end
    next unless pulls
    registry = steps.index { |step| step["run"].to_s.include?("./scripts/ci-registry.sh") }
    unless registry && registry < pulls
      errors << ".github/workflows/#{file}: #{job_name} pulls images before ./scripts/ci-registry.sh"
    end
  end
end

# The ignored suites run in CI, and the check that none is missing runs too.
podman_groups = Array(ci.dig("jobs", "podman-suites", "strategy", "matrix", "group"))
unless podman_groups.sort == %w[cli daemon isolation other]
  errors << ".github/workflows/ci.yml: podman-suites must run the isolation, daemon, cli and other groups"
end
unless Array(ci.dig("jobs", "supervisor-runtime", "strategy", "matrix", "include")).map { |leg| leg["arch"] }.sort == %w[aarch64 x86_64]
  errors << ".github/workflows/ci.yml: supervisor-runtime must run on x86_64 and aarch64"
end
ollama_steps = Array(ci.dig("jobs", "ollama-suites", "steps"))
ollama_run = ollama_steps.index { |step| step["run"].to_s.include?("./scripts/test-podman-suites.sh run ollama") }
ollama_prepare = ollama_steps.index { |step| step["run"].to_s.include?("./scripts/test-podman-suites.sh prepare ollama") }
unless ollama_prepare && ollama_run && ollama_prepare < ollama_run
  errors << ".github/workflows/ci.yml: ollama-suites must prepare its Podman images before running tests"
end
unless ollama_run
  errors << ".github/workflows/ci.yml: ollama-suites must run ./scripts/test-podman-suites.sh run ollama"
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

puts "Workflow contract: PASS (#{workflows.length} YAML files, #{composite_actions.length} composite actions, #{run_blocks} run blocks, #{scripts.length} shell scripts)"
