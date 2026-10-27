#!/usr/bin/env bash
# Exercise scripts/check-workflow-contracts.rb's live-verify rules on a
# throwaway copy of this checkout: the copy as it is passes, and each change
# that would build the verification-only live-verify feature into a binary,
# or name or turn it on, fails with the expected message. Network-free.
set -euo pipefail

[[ $# -eq 0 ]] || {
  echo 'Usage: test-workflow-contracts.sh' >&2
  exit 2
}

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
bash "$script_dir/test-ci-registry.sh"
bash "$script_dir/test-product-browser-gate.sh"
ruby "$script_dir/test-marketing-deploy.rb"
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-workflow-contracts.XXXXXX")
trap 'rm -rf -- "$work_dir"' EXIT HUP INT TERM
copy="$work_dir/checkout"
mkdir "$copy"

# The tracked and untracked (not ignored) files as they are in the working
# tree, in a repository of their own so the check's git ls-files sees them.
(cd "$repo_root" && git ls-files -z --cached --others --exclude-standard \
  | while IFS= read -r -d '' path; do [[ -e $path ]] && printf '%s\0' "$path"; done \
  | tar --null -T - -cf -) | tar -xf - -C "$copy"
git -C "$copy" init -q
git -C "$copy" add -A

failures=0
pass_count=0

run_check() {
  (cd "$copy" && ruby scripts/check-workflow-contracts.rb) >"$work_dir/out" 2>&1
}

expect_pass() {
  local name=$1
  if run_check; then
    pass_count=$((pass_count + 1))
    echo "ok: $name"
  else
    failures=$((failures + 1))
    echo "FAIL: $name: the check refused it" >&2
    sed 's/^/  /' "$work_dir/out" >&2
  fi
}

expect_refused() {
  local name=$1 message=$2
  if run_check; then
    failures=$((failures + 1))
    echo "FAIL: $name: the check passed" >&2
  elif ! grep -Fq -- "$message" "$work_dir/out"; then
    failures=$((failures + 1))
    echo "FAIL: $name: refused without \"$message\"" >&2
    sed 's/^/  /' "$work_dir/out" >&2
  else
    pass_count=$((pass_count + 1))
    echo "ok: $name"
  fi
}

# Run one case: save the files it changes, change them, check, restore.
with_change() {
  local kind=$1 name=$2 message=$3 change=$4
  shift 4
  local file
  for file in "$@"; do
    if [[ -e "$copy/$file" ]]; then cp -p "$copy/$file" "$work_dir/saved.$(echo "$file" | tr / _)"; fi
  done
  (cd "$copy" && eval "$change")
  git -C "$copy" add -A
  if [[ $kind == pass ]]; then expect_pass "$name"; else expect_refused "$name" "$message"; fi
  for file in "$@"; do
    local saved="$work_dir/saved.$(echo "$file" | tr / _)"
    if [[ -e $saved ]]; then mv "$saved" "$copy/$file"; else rm -f -- "$copy/$file"; fi
  done
  git -C "$copy" add -A
}

expect_pass 'the checkout as it is'

with_change refused 'cargo test with --all-features in a gate script' \
  'runs cargo test with --all-features' \
  "printf '%s\n' 'cargo test --locked -p axocoatl-cli --all-features --jobs 1' >> scripts/verify-native-gate.sh" \
  scripts/verify-native-gate.sh

with_change refused 'cargo test continued onto an --all-features line' \
  'runs cargo test with --all-features' \
  "printf '%s\n' 'cargo test --locked \\' '  --all-features' >> scripts/verify-native-gate.sh" \
  scripts/verify-native-gate.sh

with_change refused 'cargo build with --all-features in a workflow' \
  'runs cargo build with --all-features' \
  "ruby -EUTF-8:UTF-8 -e 'p = \".github/workflows/ci.yml\"; s = File.read(p); s.sub!(\"run: ./scripts/test-film-gate.sh\", \"run: cargo build --release --all-features\") or abort(\"no anchor\"); File.write(p, s)'" \
  .github/workflows/ci.yml

with_change refused 'cargo nextest with --all-features in a script in a subdirectory' \
  'runs cargo nextest with --all-features' \
  "mkdir -p scripts/extra && printf '%s\n' '#!/usr/bin/env bash' 'cargo nextest run --all-features' > scripts/extra/run.sh" \
  scripts/extra/run.sh

with_change refused 'cargo bench with --all-features in a shell script outside scripts/' \
  'runs cargo bench with --all-features' \
  "printf '%s\n' '#!/usr/bin/env bash' 'cargo +1.88.0 bench --all-features' > demo/bench-all.sh" \
  demo/bench-all.sh

with_change pass 'cargo check, clippy and doc with --all-features' '' \
  "printf '%s\n' '#!/usr/bin/env bash' 'cargo +1.88.0 check --all-features' 'cargo clippy --all-features' 'cargo doc --all-features' > scripts/lint-all.sh" \
  scripts/lint-all.sh

with_change refused 'a script naming the feature' \
  'names the verification-only live-verify feature' \
  "printf '%s\n' 'cargo build --release -p axocoatl-cli --features live-verify' >> scripts/verify-native-gate.sh" \
  scripts/verify-native-gate.sh

with_change refused 'a default feature set that is not empty' \
  'default features must be empty' \
  "ruby -EUTF-8:UTF-8 -e 'p = \"crates/axocoatl-daemon/Cargo.toml\"; s = File.read(p); s.sub!(/^default = \\[\\]\$/, %q(default = [\"live-verify\"])) or abort(\"no anchor\"); File.write(p, s)'" \
  crates/axocoatl-daemon/Cargo.toml

with_change refused 'another manifest turning the feature on' \
  'names the verification-only live-verify feature as' \
  "ruby -EUTF-8:UTF-8 -e 'p = \"axocoatl-server/Cargo.toml\"; s = File.read(p); s.sub!(/^axocoatl-daemon = (.*)\$/) { \"axocoatl-daemon = \" + \$1.sub(/\\}\\s*\\z/, %q(, features = [\"live-verify\"] })) } or abort(\"no anchor\"); File.write(p, s)'" \
  axocoatl-server/Cargo.toml

with_change refused 'an action step expanding an input in its script' \
  'its script expands an expression; pass it through env' \
  "ruby -EUTF-8:UTF-8 -e 'p = \"actions/verify-receipt/action.yml\"; s = File.read(p); s.sub!(/run: .*verify[.]sh.*\$/, %q(run: echo \${{ inputs.keys-file }})) or abort(\"no anchor\"); File.write(p, s)'" \
  actions/verify-receipt/action.yml

with_change refused 'an action step without a shell' \
  'a run step must name its shell' \
  "ruby -EUTF-8:UTF-8 -e 'p = \"actions/verify-receipt/action.yml\"; s = File.read(p); s.sub!(/^      id: verify\n      shell: bash\n/, %Q(      id: verify\n)) or abort(\"no anchor\"); File.write(p, s)'" \
  actions/verify-receipt/action.yml

with_change refused 'an action using an undeclared input' \
  'uses undeclared input keyfile' \
  "ruby -EUTF-8:UTF-8 -e 'p = \"actions/verify-receipt/action.yml\"; s = File.read(p); s.sub!(%q(inputs.keys-file }}), %q(inputs.keyfile }})) or abort(\"no anchor\"); File.write(p, s)'" \
  actions/verify-receipt/action.yml

with_change refused 'a build that does not wait for the new-user evidence' \
  'build.needs is' \
  "ruby -EUTF-8:UTF-8 -e 'p = \".github/workflows/release.yml\"; s = File.read(p); s.sub!(\"product-browser, new-user-evidence]\", \"product-browser]\") or abort(\"no anchor\"); File.write(p, s)'" \
  .github/workflows/release.yml

with_change refused 'a publish job that no longer checks the new-user evidence' \
  'the release job must run node scripts/release-gate/check-evidence.mjs before' \
  "ruby -EUTF-8:UTF-8 -e 'p = \".github/workflows/release.yml\"; s = File.read(p); i = s.rindex(\"node scripts/release-gate/check-evidence.mjs\") or abort(\"no anchor\"); s[i, 44] = \"echo the new-user evidence check was removed\"; File.write(p, s)'" \
  .github/workflows/release.yml

with_change refused 'the override input expanded in a script' \
  'expands an input in its script' \
  "ruby -EUTF-8:UTF-8 -e 'p = \".github/workflows/release.yml\"; s = File.read(p); s.sub!(%q(--override \"\${NEW_USER_GATE_OVERRIDE:-}\"), %q(--override \"\${{ inputs.new_user_gate_override }}\")) or abort(\"no anchor\"); File.write(p, s)'" \
  .github/workflows/release.yml

with_change refused 'an evidence check that no longer names who ran the override' \
  'must pass --actor and --triggering-actor' \
  "ruby -EUTF-8:UTF-8 -e 'p = \".github/workflows/release.yml\"; s = File.read(p); s.sub!(%q(--triggering-actor \"\$TRIGGERING_ACTOR\"), %q(--triggering-actor \"\$ACTOR\")) or abort(\"no anchor\"); File.write(p, s)'" \
  .github/workflows/release.yml

with_change refused 'a Node 20 action in a workflow' \
  'is not a reviewed Node 24 or composite release' \
  "ruby -EUTF-8:UTF-8 -e 'p = \".github/workflows/ci.yml\"; s = File.read(p); s.sub!(\"actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09\", \"actions/checkout@11d5960a326750d5838078e36cf38b85af677262\") or abort(\"no anchor\"); File.write(p, s)'" \
  .github/workflows/ci.yml

with_change refused 'a Node 20 action in a docs example' \
  'its example uses actions/checkout@11d5960a326750d5838078e36cf38b85af677262' \
  "ruby -EUTF-8:UTF-8 -e 'p = \"sites/docs/src/content/docs/workbench/receipts.mdx\"; s = File.read(p); s.sub!(\"actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09\", \"actions/checkout@11d5960a326750d5838078e36cf38b85af677262\") or abort(\"no anchor\"); File.write(p, s)'" \
  sites/docs/src/content/docs/workbench/receipts.mdx

with_change refused 'a release whose archives are not attested' \
  'prove-artifacts must attest the verified archives' \
  "ruby -EUTF-8:UTF-8 -e 'p = \".github/workflows/release.yml\"; s = File.read(p); s.sub!(%r{      - name: Attest the build provenance of every release archive\\n.*?(?=      - name: Prove every)}m, \"\") or abort(\"no anchor\"); File.write(p, s)'" \
  .github/workflows/release.yml

with_change refused 'another release job asking for an OIDC token' \
  'test-gate must not ask for an OIDC token' \
  "ruby -EUTF-8:UTF-8 -e 'p = \".github/workflows/release.yml\"; s = File.read(p); s.sub!(\"  test-gate:\\n    name: Product test gate\\n\", \"  test-gate:\\n    name: Product test gate\\n    permissions:\\n      id-token: write\\n\") or abort(\"no anchor\"); File.write(p, s)'" \
  .github/workflows/release.yml

with_change refused 'a release browser gate without the macOS WebKit leg' \
  'product-browser must run Chromium on ubuntu-24.04' \
  "ruby -EUTF-8:UTF-8 -e 'p = \".github/workflows/release.yml\"; s = File.read(p); s.sub!(\"runner: macos-26\", \"runner: ubuntu-24.04\") or abort(\"no anchor\"); File.write(p, s)'" \
  .github/workflows/release.yml

with_change refused 'a Podman pull before origin routing' \
  'new-user-linux pulls images before ./scripts/ci-registry.sh' \
  "ruby -EUTF-8:UTF-8 -e 'p = \".github/workflows/release.yml\"; s = File.read(p); s.sub!(\"        run: ./scripts/ci-registry.sh\\n      - name: A brand-new user\", \"        run: echo no registry setup\\n      - name: A brand-new user\") or abort(\"no anchor\"); File.write(p, s)'" \
  .github/workflows/release.yml

with_change refused 'a Podman suite group left out of CI' \
  'podman-suites must run the isolation, daemon, cli and other groups' \
  "ruby -EUTF-8:UTF-8 -e 'p = \".github/workflows/ci.yml\"; s = File.read(p); s.sub!(\"group: [isolation, daemon, cli, other]\", \"group: [isolation, cli, other]\") or abort(\"no anchor\"); File.write(p, s)'" \
  .github/workflows/ci.yml

with_change refused 'the live Ollama job without its Session image preparation' \
  'ollama-suites must prepare its Podman images before running tests' \
  "ruby -EUTF-8:UTF-8 -e 'p = \".github/workflows/ci.yml\"; s = File.read(p); s.sub!(\"run: ./scripts/test-podman-suites.sh prepare ollama\", \"run: echo missing image preparation\") or abort(\"no anchor\"); File.write(p, s)'" \
  .github/workflows/ci.yml

if ((failures)); then
  echo "Workflow contract self-test: FAIL ($failures failed, $pass_count passed)" >&2
  exit 1
fi
echo "Workflow contract self-test: PASS ($pass_count cases)"
