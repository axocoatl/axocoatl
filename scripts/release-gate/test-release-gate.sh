#!/usr/bin/env bash
# Tests of the new-user gate's own scripts (docs/RELEASE_GATE.md), safe on
# any developer machine: everything happens in a temporary directory with a
# temporary HOME. The installer test uses a copy of scripts/install.sh whose
# /usr/local/bin is an unwritable temporary directory, so it can never write
# to the machine's /usr/local/bin.
#
# Usage: test-release-gate.sh
# AXOCOATL_BIN (default target/{debug,release}/axocoatl when present) signs a
# real bundle; without one that part is skipped.
# Each case runs in a subshell with its own HOME and PATH, on purpose.
# The node -e programs read their values from process.argv.
# shellcheck disable=SC2016,SC2030,SC2031,SC2119
set -uo pipefail

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH='' cd -- "$script_dir/../.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-gate-test.XXXXXX")
if [ -n "${GATE_TEST_KEEP:-}" ]; then
  trap 'echo "kept $work"' EXIT
else
  trap 'chmod -R u+w "$work" 2>/dev/null; rm -rf -- "$work"' EXIT
fi
failures=0
checks=0

pass() { checks=$((checks + 1)); echo "ok - $*"; }
fail() {
  checks=$((checks + 1))
  failures=$((failures + 1))
  echo "not ok - $*" >&2
}
expect() {
  # expect DESCRIPTION -- COMMAND...: COMMAND must succeed.
  local description=$1
  shift 2
  if "$@"; then pass "$description"; else fail "$description"; fi
}

command -v node >/dev/null 2>&1 || { echo "test-release-gate: node is required" >&2; exit 1; }

# --- The Node suites.
if node --test "$script_dir"/*.test.mjs >"$work/node-tests.log" 2>&1; then
  pass "node --test scripts/release-gate/*.test.mjs ($(grep -c '^ok ' "$work/node-tests.log") passed)"
else
  cat "$work/node-tests.log" >&2
  fail "node --test scripts/release-gate/*.test.mjs"
fi

# --- lib.sh helpers, in a subshell with a temporary HOME.
(
  export HOME="$work/home"
  mkdir -p "$HOME"
  # shellcheck source=scripts/release-gate/lib.sh
  . "$script_dir/lib.sh"

  mkdir -p "$work/bin-with-axocoatl" "$work/plain-bin" "$HOME/.local/bin"
  printf '#!/bin/sh\n' >"$work/bin-with-axocoatl/axocoatl"
  chmod +x "$work/bin-with-axocoatl/axocoatl"
  result=$(PATH="$HOME/.local/bin:$work/bin-with-axocoatl:$work/plain-bin:/usr/bin:/bin" gate_new_user_path)
  [ "$result" = "$work/plain-bin:/usr/bin:/bin" ] || { echo "new-user PATH: $result" >&2; exit 1; }

  printf '#!/bin/sh\necho hidden\n' >"$work/plain-bin/hideme"
  printf '#!/bin/sh\necho kept\n' >"$work/plain-bin/keepme"
  chmod +x "$work/plain-bin/hideme" "$work/plain-bin/keepme"
  without=$(PATH="$work/plain-bin:/usr/bin:/bin" gate_path_without hideme "$work/mirrors")
  PATH="$without" command -v hideme >/dev/null 2>&1 && { echo "hideme still on PATH" >&2; exit 1; }
  [ "$(PATH="$without" keepme)" = kept ] || { echo "keepme lost" >&2; exit 1; }

  gate_guard_shims "$work/guard" "$work/guard.log"
  PATH="$work/guard:$PATH" sudo apt-get install -y podman 2>/dev/null && { echo "the sudo guard succeeded" >&2; exit 1; }
  grep -qx 'sudo apt-get install -y podman' "$work/guard.log" || { echo "the guard did not log" >&2; exit 1; }

  [ "$(gate_with_outputs 'axocoatl run fix --task "x" --junit j.xml')" = 'axocoatl run fix --task "x" --junit j.xml --record run.axorecord.jsonl --receipt receipt.json' ] \
    || { echo "gate_with_outputs" >&2; exit 1; }
  [ "$(gate_flag_value 'axocoatl run fix --record r.jsonl --receipt x.json' --receipt)" = x.json ] || { echo "gate_flag_value" >&2; exit 1; }
)
lib_status=$?
if [ "$lib_status" = 0 ]; then pass "lib.sh: new-user PATH, hiding a program, guard shims, run outputs"; else fail "lib.sh helpers"; fi

# --- The installer step, end to end, against a fake release.
(
  export HOME="$work/install-home"
  mkdir -p "$HOME"
  # shellcheck source=scripts/release-gate/lib.sh
  . "$script_dir/lib.sh"
  version=$(awk '$0 == "[package]" { p = 1; next } p && /^\[/ { exit } p && $1 == "version" { gsub(/"/, "", $3); print $3; exit }' "$repo_root/axocoatl-cli/Cargo.toml")
  case "$(uname -s):$(uname -m)" in
    Linux:x86_64) target=x86_64-unknown-linux-gnu ;;
    Linux:aarch64) target=aarch64-unknown-linux-gnu ;;
    Darwin:arm64) target=aarch64-apple-darwin ;;
    Darwin:x86_64) target=x86_64-apple-darwin ;;
  esac
  mkdir -p "$work/dist/payload" "$work/site" "$work/locked-bin"
  printf '#!/bin/sh\necho "axocoatl %s"\n' "$version" >"$work/dist/payload/axocoatl"
  chmod 0755 "$work/dist/payload/axocoatl"
  printf 'license\n' >"$work/dist/payload/LICENSE"
  printf 'notices\n' >"$work/dist/payload/THIRD_PARTY_LICENSES.txt"
  tarball="$work/dist/axocoatl-v$version-$target.tar.gz"
  tar -czf "$tarball" -C "$work/dist/payload" axocoatl LICENSE THIRD_PARTY_LICENSES.txt
  printf '%s  %s\n' "$(gate_sha256 "$tarball")" "${tarball##*/}" >"$tarball.sha256"
  # The site's installer, except that its /usr/local/bin is a locked
  # temporary directory: this test never writes the machine's /usr/local/bin.
  chmod 0555 "$work/locked-bin"
  sed "s#/usr/local/bin#$work/locked-bin#g" "$repo_root/scripts/install.sh" >"$work/site/install.sh"
  grep -q '/usr/local/bin' "$work/site/install.sh" && { echo "the test installer still names /usr/local/bin" >&2; exit 1; }
  gate_begin "$work/evidence" "$version" "$(printf 'a%.0s' $(seq 1 40))" A-test A
  GATE_BASE_PATH=$(PATH="/usr/bin:/bin:/usr/sbin:/sbin" gate_new_user_path)
  node_dir=$(dirname "$(command -v node)")
  export GATE_BASE_PATH PATH="$GATE_BASE_PATH:$node_dir"
  stage="$work/stage"
  [ "$(gate_stage_release "$tarball" "$stage")" = "v$version" ] || { echo "stage tag" >&2; exit 1; }
  gate_install_new_user "$stage" "$work/site" >"$work/install-step.log" 2>&1 || { cat "$work/install-step.log" >&2; exit 1; }
  [ "$GATE_INSTALLED" = "$HOME/.local/bin/axocoatl" ] || { echo "installed to $GATE_INSTALLED" >&2; exit 1; }
  [ "$(axocoatl --version)" = "axocoatl $version" ] || { echo "the installed axocoatl is not on PATH" >&2; exit 1; }
  grep -q "^file://" "$GATE_ATTACH/logs/install-curl-urls.log" 2>/dev/null \
    || grep -q "file://$stage" "$GATE_ATTACH/logs/install-curl-urls.log" || { echo "curl did not use file:// URLs" >&2; exit 1; }
  grep -q '^REFUSED' "$GATE_ATTACH/logs/install-curl-urls.log" && { echo "the shim refused a URL" >&2; exit 1; }
  # The shim refuses any other URL.
  gate_curl_shim "$work/shim" "$stage" "$work/site" "$work/shim.log"
  PATH="$work/shim:$PATH" curl -fsS https://example.com/ >/dev/null 2>&1 && { echo "the shim fetched example.com" >&2; exit 1; }
  grep -q '^REFUSED https://example.com/' "$work/shim.log" || { echo "the refusal was not logged" >&2; exit 1; }
  gate_step "a step that passes" -- true >/dev/null 2>&1
  gate_step "a step expected to exit 3" --expect 3 -- sh -c 'exit 3' >/dev/null 2>&1
  gate_step "a step that fails" --optional -- sh -c 'echo why it failed; exit 1' >/dev/null 2>&1
  gate_finish >/dev/null 2>&1 || { cat "$GATE_BUNDLE/A-test.json" >&2; exit 1; }
  node -e '
    const e = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
    const names = e.steps.map((s) => `${s.name}:${s.status}`);
    const want = ["install with the published command:pass", "installed binary is where the installer said:pass",
      "a new login shell finds axocoatl:pass", "a step that passes:pass", "a step expected to exit 3:pass", "a step that fails:fail"];
    for (const w of want) if (!names.includes(w)) { console.error(`missing ${w} in ${names}`); process.exit(1); }
    if (e.pass !== true) { console.error("an optional failure failed the path"); process.exit(1); }
    if (!e.attachments.includes("A-test/logs/install.log")) { console.error("install.log is not attached"); process.exit(1); }
    if (!/why it failed/.test(e.steps.find((s) => s.name === "a step that fails").detail)) process.exit(1);
  ' "$GATE_BUNDLE/A-test.json"
)
install_status=$?
if [ "$install_status" = 0 ]; then
  pass "installer step: published install.sh through the curl shim into ~/.local/bin, with steps recorded"
else
  fail "installer step"
fi

# --- The scripts refuse what they must refuse, before doing anything.
out=$(GATE_ACCOUNT=axo-gate-test-nobody "$script_dir/reset-mac-account.sh" --dry-run 2>&1)
status=$?
case "$(uname -s)" in
  Darwin) expected='not the gate account' ;;
  *) expected='this resets a macOS gate account' ;;
esac
if [ "$status" = 1 ] && printf '%s' "$out" | grep -q "$expected"; then
  pass "reset-mac-account.sh refuses an account that is not the gate account"
else
  fail "reset-mac-account.sh refusal (exit $status): $out"
fi
if [ "$(uname -s)" = Darwin ]; then
  out=$(GATE_ACCOUNT="$(id -un)" HOME="$work/not-the-home" "$script_dir/reset-mac-account.sh" --dry-run 2>&1)
  status=$?
  if [ "$status" = 1 ] && printf '%s' "$out" | grep -q 'HOME is'; then
    pass "reset-mac-account.sh refuses a HOME that is not the account's"
  else
    fail "reset-mac-account.sh HOME refusal (exit $status): $out"
  fi
fi
out=$(GATE_ACCOUNT=axo-gate-test-nobody "$script_dir/newuser-mac.sh" --release-dir "$work" --site "$work" 2>&1)
status=$?
if [ "$status" = 2 ] && printf '%s' "$out" | grep -Eq 'gate account|macOS gate'; then
  pass "newuser-mac.sh refuses outside the gate account"
else
  fail "newuser-mac.sh refusal (exit $status): $out"
fi
out=$("$script_dir/newuser-linux.sh" --help 2>&1)
status=$?
if [ "$status" = 2 ] && printf '%s' "$out" | grep -q -- '--tarball'; then
  pass "newuser-linux.sh --help"
else
  fail "newuser-linux.sh --help (exit $status)"
fi
if [ "$(uname -s)" != Linux ]; then
  out=$("$script_dir/newuser-linux.sh" --tarball /nonexistent 2>&1)
  status=$?
  if [ "$status" = 2 ] && printf '%s' "$out" | grep -q 'Linux gate'; then
    pass "newuser-linux.sh refuses outside Linux"
  else
    fail "newuser-linux.sh outside Linux (exit $status): $out"
  fi
fi

# --- newuser-linux.sh end to end against a stand-in axocoatl (it follows
# the shared onboard/doctor/run contract) and the real stub model server.
# GATE_SIMULATION=1 marks the path failed on purpose: never evidence.
(
  set -e
  sim="$work/sim"
  mkdir -p "$sim/payload" "$sim/fakebin" "$sim/dist"
  version=$(awk '$0 == "[package]" { p = 1; next } p && /^\[/ { exit } p && $1 == "version" { gsub(/"/, "", $3); print $3; exit }' "$repo_root/axocoatl-cli/Cargo.toml")
  printf '#!/bin/sh\nexec node "%s" "$@"\n' "$script_dir/testdata/fake-axocoatl.mjs" >"$sim/payload/axocoatl"
  chmod 0755 "$sim/payload/axocoatl"
  printf 'license\n' >"$sim/payload/LICENSE"
  printf 'notices\n' >"$sim/payload/THIRD_PARTY_LICENSES.txt"
  case "$(uname -s):$(uname -m)" in
    Linux:x86_64) target=x86_64-unknown-linux-gnu ;;
    Linux:aarch64) target=aarch64-unknown-linux-gnu ;;
    Darwin:arm64) target=aarch64-apple-darwin ;;
    Darwin:x86_64) target=x86_64-apple-darwin ;;
  esac
  tarball="$sim/dist/axocoatl-v$version-$target.tar.gz"
  tar -czf "$tarball" -C "$sim/payload" axocoatl LICENSE THIRD_PARTY_LICENSES.txt
  # shellcheck source=scripts/release-gate/lib.sh
  . "$script_dir/lib.sh"
  printf '%s  %s\n' "$(gate_sha256 "$tarball")" "${tarball##*/}" >"$tarball.sha256"
  cat >"$sim/fakebin/podman" <<'PODMAN'
#!/bin/sh
case "$1" in
  version) echo 4.9.3 ;;
  ps) ;;
  *) exit 1 ;;
esac
PODMAN
  chmod 0755 "$sim/fakebin/podman"
  commit=$(printf 'b%.0s' $(seq 1 40))
  set +e
  GATE_SIMULATION=1 GATE_TMPDIR="$sim" FAKE_AXOCOATL_VERSION="$version" PATH="$sim/fakebin:$PATH" \
    "$script_dir/newuser-linux.sh" --tarball "$tarball" --commit "$commit" --label A-sim \
    --evidence-root "$sim/evidence" --port 8243 >"$sim/run.log" 2>&1
  status=$?
  set -e
  [ "$status" = 1 ] || { tail -n 30 "$sim/run.log" >&2; echo "simulation exit $status, expected 1" >&2; exit 1; }
  node -e '
    const e = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
    const status = Object.fromEntries(e.steps.map((s) => [s.name, s.status]));
    const fixtures = ["node-deps", "python-pytest", "rust", "go"];
    const want = {
      "simulation: a stand-in axocoatl and a sandboxed installer": "fail",
      "start without axocoatl on PATH": "pass",
      "install with the published command": "pass",
      "installed binary is the tarball\u0027s": "pass",
      "onboard --yes --writer ollama with no terminal": "pass",
      "onboard ran no sudo and no package manager": "pass",
      "--no-pull pulled nothing": "pass",
      "config.yaml holds the run models (run.models)": "pass",
      "config.yaml uses the free port 8243": "pass",
      "doctor: ready, no FAIL in sandbox, models, writers, daemon": "pass",
      "axocoatl dev serves on port 8243": "pass",
      "a check program that is absent: exit 3": "pass",
      "refused before any model call (0 tokens)": "pass",
      "reviewer anthropic:x: exit 3": "pass",
      "no Session, container or model call for the refused run": "pass",
      "onboard without Podman: exit 1": "pass",
      "onboard prints the Podman install command": "pass",
      "onboard did not run sudo without --allow-sudo": "pass",
      "doctor FAILs an empty /etc/subuid with the usermod fix": "skip",
      "Ctrl+C stops axocoatl dev with exit 0": "pass",
    };
    for (const fixture of fixtures) {
      for (const name of [`run fix on ${fixture}`, `receipt verify on ${fixture} (documented command)`, `record verify on ${fixture}`, `the model fixed ${fixture} once`]) want[name] = "pass";
    }
    const wrong = Object.entries(want).filter(([name, value]) => status[name] !== value).map(([name, value]) => `${name}: ${status[name]} (want ${value})`);
    if (wrong.length) { console.error(wrong.join("\n")); process.exit(1); }
    if (e.pass !== false) { console.error("a simulated path passed"); process.exit(1); }
  ' "$sim/evidence/$version/$commit/A-sim.json"
  # A fixture the model cannot fix fails its run.
  node -e '
    const f = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
    f.fixes = f.fixes.filter((fix) => fix.fixture !== "go");
    require("fs").writeFileSync(process.argv[2], JSON.stringify(f));
  ' "$script_dir/stub-fixes.json" "$sim/no-go-fix.json"
  set +e
  GATE_SIMULATION=1 GATE_TMPDIR="$sim" FAKE_AXOCOATL_VERSION="$version" GATE_STUB_FIXES="$sim/no-go-fix.json" PATH="$sim/fakebin:$PATH" \
    "$script_dir/newuser-linux.sh" --tarball "$tarball" --commit "$commit" --label A-sim-broken \
    --evidence-root "$sim/evidence" --port 8244 >"$sim/broken.log" 2>&1
  set -e
  node -e '
    const e = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
    const step = (name) => e.steps.find((s) => s.name === name)?.status;
    if (step("run fix on go") !== "fail" || step("run fix on rust") !== "pass") { console.error(JSON.stringify(e.steps.filter((s) => /go|rust/.test(s.name)))); process.exit(1); }
  ' "$sim/evidence/$version/$commit/A-sim-broken.json"
)
sim_status=$?
if [ "$sim_status" = 0 ]; then
  pass "newuser-linux.sh against a stand-in axocoatl: every Tier A step recorded as the contract says; a fixture the model cannot fix fails its run"
else
  fail "newuser-linux.sh simulation"
fi

# --- collect-evidence.sh: a bundle for every required path, signed by the
# real CLI when one is built, then the release decision on it.
bin=${AXOCOATL_BIN:-}
for candidate in "$repo_root/target/release/axocoatl" "$repo_root/target/debug/axocoatl"; do
  [ -n "$bin" ] || { [ -x "$candidate" ] && bin=$candidate; }
done
(
  set -e
  repo="$work/collect-repo"
  mkdir -p "$repo/docs" "$repo/.axocoatl" "$repo/scripts"
  cp "$repo_root/docs/RELEASE_GATE.md" "$repo/docs/"
  printf '# product\n' >"$repo/README.md"
  git init -q -b main "$repo"
  git -C "$repo" -c user.name=t -c user.email=t@example.invalid add -A
  git -C "$repo" -c user.name=t -c user.email=t@example.invalid commit -qm product
  commit=$(git -C "$repo" rev-parse HEAD)
  from="$work/from"
  mkdir -p "$from"
  for name in $(node -e '
      import("'"$script_dir"'/evidence.mjs").then((m) => {
        const text = require("fs").readFileSync(process.argv[1], "utf8");
        console.log(m.parseRequiredPaths(text).filter((p) => p.when === "always").map((p) => p.name).join(" "));
      });' "$repo/docs/RELEASE_GATE.md"); do
    mkdir -p "$from/$name"
    printf '%s transcript\n' "$name" >"$from/$name/transcript.txt"
    node "$script_dir/evidence.mjs" add-step --file "$from/$name.steps" --name "run fix" --status pass --exit 0 --seconds 2 --bytes 10
    node "$script_dir/evidence.mjs" path-file --steps "$from/$name.steps" --out "$from/$name.json" --path "$name" \
      --tier "${name%%-*}" --version 1.5.0 --commit "$commit" --attach "$name/transcript.txt" >/dev/null
    rm "$from/$name.steps"
  done
  # A transcript holding something that looks like a key is refused.
  cp -R "$from" "$work/leaky"
  printf 'OPENROUTER_API_KEY=sk-or-v1-0123456789abcdef0123456789\n' >>"$work/leaky/D1/transcript.txt"
  if "$script_dir/collect-evidence.sh" --repo-root "$repo" --version 1.5.0 --commit "$commit" --from "$work/leaky" --no-sign >"$work/leaky.log" 2>&1; then
    echo "a credential-looking transcript was collected" >&2
    exit 1
  fi
  grep -q 'look like they hold a credential' "$work/leaky.log"
  rm -rf "$repo/release-evidence"
  if [ -n "$bin" ]; then
    sign_home="$work/sign-home"
    mkdir -p "$sign_home"
    HOME="$sign_home" "$bin" receipt key 2>/dev/null >"$repo/.axocoatl/gate-keys"
    HOME="$sign_home" "$script_dir/collect-evidence.sh" --repo-root "$repo" --version 1.5.0 --commit "$commit" \
      --from "$from" --axocoatl "$bin" >"$work/collect.log" 2>&1 || { cat "$work/collect.log" >&2; exit 1; }
    git -C "$repo" -c user.name=t -c user.email=t@example.invalid add -A
    git -C "$repo" -c user.name=t -c user.email=t@example.invalid commit -qm evidence
    node "$script_dir/check-evidence.mjs" --root "$repo" --version 1.5.0 --tag-commit "$(git -C "$repo" rev-parse HEAD)" >"$work/check.log" 2>&1 \
      || { cat "$work/check.log" >&2; exit 1; }
    grep -q '^New-user gate: PASS for v1.5.0' "$work/check.log"
  else
    "$script_dir/collect-evidence.sh" --repo-root "$repo" --version 1.5.0 --commit "$commit" --from "$from" --no-sign >"$work/collect.log" 2>&1
    git -C "$repo" -c user.name=t -c user.email=t@example.invalid add -A
    git -C "$repo" -c user.name=t -c user.email=t@example.invalid commit -qm evidence
    if node "$script_dir/check-evidence.mjs" --root "$repo" --version 1.5.0 --tag-commit "$(git -C "$repo" rev-parse HEAD)" >"$work/check.log" 2>&1; then
      echo "an unsigned bundle passed" >&2
      exit 1
    fi
    grep -q 'manifest.dsse.json is missing\|no gate key' "$work/check.log"
  fi
)
collect_status=$?
if [ -n "$bin" ]; then
  signed="a bundle signed by ${bin#"$repo_root"/} passes check-evidence"
else
  signed="an unsigned bundle fails check-evidence (no axocoatl built)"
fi
if [ "$collect_status" = 0 ]; then pass "collect-evidence.sh: refuses credential-looking files; $signed"; else fail "collect-evidence.sh"; fi

echo "Release gate scripts: $((checks - failures))/$checks passed"
[ "$failures" = 0 ]
