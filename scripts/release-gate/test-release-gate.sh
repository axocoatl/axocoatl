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

# Nothing here may reach the developer's own Axocoatl: every variable that
# would point the CLI at another data root or configuration goes, so each
# case's temporary HOME is the only one it sees.
unset AXOCOATL_DATA_DIR AXOCOATL_CONFIG AXOCOATL_TOKEN AXOCOATL_SOCKET_PATH \
  XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME GATE_SIGNER_DIR
# The installer must never ask on this terminal (scripts/install.sh reads
# AXOCOATL_INSTALL_TTY in place of /dev/tty, for tests).
export AXOCOATL_INSTALL_TTY="$work/no-terminal"

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
  [ "$(gate_without_models 'axocoatl run fix --task "a b" --model writer=ollama:x:1 --model reviewer=openrouter:y/z --junit j.xml')" = 'axocoatl run fix --task "a b" --junit j.xml' ] \
    || { echo "gate_without_models" >&2; exit 1; }

  # The config readers.
  printf 'server:\n  host: "127.0.0.1"\n  port: 8231\nrun:\n  models:\n    writer: "ollama:qwen3-coder:30b"\n    reviewer: openrouter:openai/gpt-oss-120b\nother:\n  port: 1\n' >"$work/config.yaml"
  [ "$(gate_server_port "$work/config.yaml")" = 8231 ] || { echo "gate_server_port" >&2; exit 1; }
  [ "$(gate_run_model "$work/config.yaml" writer)" = ollama:qwen3-coder:30b ] || { echo "gate_run_model writer" >&2; exit 1; }
  [ "$(gate_run_model "$work/config.yaml" reviewer)" = openrouter:openai/gpt-oss-120b ] || { echo "gate_run_model reviewer" >&2; exit 1; }

  # Names that could climb out of a bundle are refused before anything is
  # removed.
  mkdir -p "$work/bundles/1.5.0/keep-me"
  for bad in '../keep-me' 'a/b' '-x' ''; do
    (gate_begin "$work/bundles" 1.5.0 "$(printf 'c%.0s' $(seq 1 40))" "$bad" A) 2>/dev/null && { echo "gate_begin took '$bad'" >&2; exit 1; }
  done
  (gate_begin "$work/bundles" '../..' "$(printf 'c%.0s' $(seq 1 40))" A-x A) 2>/dev/null && { echo "gate_begin took a version of ../.." >&2; exit 1; }
  (gate_begin "$work/bundles" 1.5.0 '..' A-x A) 2>/dev/null && { echo "gate_begin took a commit of .." >&2; exit 1; }
  [ -d "$work/bundles/1.5.0/keep-me" ] || { echo "gate_begin removed another bundle" >&2; exit 1; }

  # The installer's printed profile line, and a new shell that finds the
  # binary once it is added.
  printf 'axocoatl-install: installed axocoatl v1.5.0 -> %s/.local/bin/axocoatl\naxocoatl-install: ~/.local/bin is not on your PATH. For new terminals to find axocoatl, add this line to ~/.zprofile:\n\n    export PATH="$HOME/.local/bin:$PATH"\n' "$HOME" >"$work/persistent.log"
  [ "$(gate_printed_profile_line "$work/persistent.log")" = "$HOME/.zprofile"$'\t''export PATH="$HOME/.local/bin:$PATH"' ] \
    || { echo "gate_printed_profile_line: $(gate_printed_profile_line "$work/persistent.log")" >&2; exit 1; }
  printf 'axocoatl-install: add /x/.local/bin to your PATH:  export PATH="/x/.local/bin:$PATH"\n' >"$work/transient.log"
  [ -z "$(gate_printed_profile_line "$work/transient.log")" ] || { echo "a transient export counted as a profile line" >&2; exit 1; }
  mkdir -p "$work/shell-bin"
  printf '#!/bin/sh\n' >"$work/shell-bin/axocoatl"
  chmod +x "$work/shell-bin/axocoatl"
  GATE_BASE_PATH=/usr/bin:/bin
  gate_new_shells_find "$work/shell-bin/axocoatl" >/dev/null && { echo "a new shell found a binary not on its PATH" >&2; exit 1; }
  while read -r shell _; do
    case "${shell##*/}" in
      zsh) printf 'export PATH="%s:$PATH"\n' "$work/shell-bin" >>"$HOME/.zprofile" ;;
      *) printf 'export PATH="%s:$PATH"\n' "$work/shell-bin" | tee -a "$HOME/.profile" >>"$HOME/.bashrc" ;;
    esac
  done < <(gate_new_shells)
  gate_new_shells_find "$work/shell-bin/axocoatl" >/dev/null || { echo "new shells: $(gate_new_shells_find "$work/shell-bin/axocoatl")" >&2; exit 1; }

  # Doctor's verdict as a new user reads it.
  gate_begin "$work/evidence-lib" 1.5.0 "$(printf 'd%.0s' $(seq 1 40))" A-lib A
  printf 'Axocoatl doctor\n  [OK] Podman\n\nReady for: axocoatl run fix, axocoatl run fix-codex\n  ✓ axocoatl run fix (ollama)\nReady: yes\n' >"$work/doctor-ready.log"
  printf 'Axocoatl doctor\n  [FAIL] Podman\n\nReady for: axocoatl run fix\nReady: yes\n' >"$work/doctor-fail.log"
  printf 'Axocoatl doctor\n\nAll required checks passed\n' >"$work/doctor-old.log"
  gate_doctor_text_check "$work/doctor-ready.log" 2>/dev/null
  gate_doctor_text_check "$work/doctor-fail.log" 2>/dev/null
  gate_doctor_text_check "$work/doctor-old.log" 2>/dev/null

  # sudo by any path, from the system journal (a stand-in journalctl).
  mkdir -p "$work/journal-bin"
  cat >"$work/journal-bin/journalctl" <<JOURNAL
#!/bin/sh
case " \$* " in
  *" _UID=0 "*) { [ -f "$work/journal-readable" ] || [ -n "\${JOURNAL_AS_ROOT:-}" ]; } && echo "systemd: started" ;;
  *" --show-cursor "*) echo "last entry"; echo "-- cursor: s=1;i=2" ;;
  *" --after-cursor "*) cat "$work/journal-sudo" 2>/dev/null ;;
esac
exit 0
JOURNAL
  chmod +x "$work/journal-bin/journalctl"
  (
    export PATH="$work/journal-bin:$PATH"
    unset GITHUB_ACTIONS
    [ -z "$(gate_sudo_mark)" ] || { echo "an unreadable journal gave a mark" >&2; exit 1; }
    gate_sudo_check "sudo: journal unreadable here" "" 2>/dev/null
    GITHUB_ACTIONS=true gate_sudo_check "sudo: journal unreadable on CI" "" 2>/dev/null
    # On CI with passwordless sudo, a journal only root reads is read with
    # sudo, and those reads are not counted (a stand-in sudo here).
    printf '#!/bin/sh\n[ "$1" = -n ] && shift\nJOURNAL_AS_ROOT=1 exec "$@"\n' >"$work/journal-bin/sudo"
    chmod +x "$work/journal-bin/sudo"
    mark=$(GITHUB_ACTIONS=true gate_sudo_mark)
    [ "$mark" = 'sudo s=1;i=2' ] || { echo "sudo mark: $mark" >&2; exit 1; }
    printf '  runner : PWD=/x ; USER=root ; COMMAND=/usr/bin/journalctl -q --after-cursor s=1;i=2 -t sudo -o cat\n' >"$work/journal-sudo"
    GITHUB_ACTIONS=true gate_sudo_check "sudo: none (journal read with sudo)" "$mark" 2>/dev/null
    rm -f "$work/journal-sudo"
    touch "$work/journal-readable"
    mark=$(gate_sudo_mark)
    [ "$mark" = 'plain s=1;i=2' ] || { echo "mark: $mark" >&2; exit 1; }
    gate_sudo_check "sudo: none" "$mark" 2>/dev/null
    printf '  runner : PWD=/home/runner ; USER=root ; COMMAND=/usr/bin/apt-get install -y podman\n' >"$work/journal-sudo"
    gate_sudo_check "sudo: by absolute path" "$mark" 2>/dev/null
    cp "$GATE_STEPS" "$work/sudo-steps.jsonl"
  ) || exit 1
  node -e '
    const steps = require("fs").readFileSync(process.argv[1], "utf8").trim().split("\n").map(JSON.parse);
    const status = Object.fromEntries(steps.map((s) => [s.name, `${s.status}${s.required ? "" : " (optional)"}`]));
    const want = {
      "doctor says Ready for: axocoatl run fix, Ready: yes, no [FAIL]": undefined,
      "sudo: journal unreadable here": "skip (optional)",
      "sudo: journal unreadable on CI": "fail",
      "sudo: none (journal read with sudo)": "pass",
      "sudo: none": "pass",
      "sudo: by absolute path": "fail",
    };
    const doctor = steps.filter((s) => s.name.startsWith("doctor says")).map((s) => s.status);
    if (doctor.join() !== "pass,fail,fail") { console.error(`doctor text checks: ${doctor}`); process.exit(1); }
    for (const [name, value] of Object.entries(want)) {
      if (value && status[name] !== value) { console.error(`${name}: ${status[name]} (want ${value})`); process.exit(1); }
    }
    if (!/apt-get install -y podman/.test(steps.find((s) => s.name === "sudo: by absolute path").detail)) process.exit(1);
  ' "$work/sudo-steps.jsonl" || exit 1
  # This record is never finished; its temporary directory goes now.
  rm -rf -- "$GATE_TMP"

  # The command each fixture runs: the README's, or onboarding's own for its
  # sample (fixtures.json "command": "fixture-readme").
  [ "$(gate_fixture_command node-deps 'axocoatl run fix --task "README task"')" = 'axocoatl run fix --task "README task"' ] \
    || { echo "gate_fixture_command node-deps" >&2; exit 1; }
  if [ "$(gate_fixture_expect node-plain command)" != fixture-readme ]; then
    echo "node-plain does not run its own README's command" >&2
    exit 1
  fi
  if [ -f "$gate_repo/tests/fixtures/newuser/node-plain/README.md" ]; then
    [ "$(gate_fixture_command node-plain 'axocoatl run fix --task "README task"')" = \
      "$(node "$gate_commands_js" readme-run "$gate_repo/tests/fixtures/newuser/node-plain/README.md")" ] \
      || { echo "gate_fixture_command node-plain" >&2; exit 1; }
  else
    [ -z "$(gate_fixture_command node-plain 'axocoatl run fix --task "README task"')" ] \
      || { echo "gate_fixture_command for an absent node-plain printed a command" >&2; exit 1; }
  fi
)
lib_status=$?
if [ "$lib_status" = 0 ]; then
  pass "lib.sh: new-user PATH, hiding a program, guard shims, run outputs, config readers, name checks, new shells, doctor's verdict, sudo in the journal, fixture commands"
else
  fail "lib.sh helpers"
fi

# The installer under test either prints the line that keeps axocoatl on a
# new terminal's PATH (the gate then adds it and checks), or only an export
# for the current shell, which the gate fails.
if grep -q 'add this line to' "$repo_root/scripts/install.sh"; then new_shell_expect=pass; else new_shell_expect=fail; fi

# --- The installer step, end to end, against a fake release.
(
  export HOME="$work/install-home"
  mkdir -p "$HOME"
  # A new user's HOME and login shell, as newuser-linux.sh makes them.
  [ -d /etc/skel ] && cp -R /etc/skel/. "$HOME/" 2>/dev/null
  if [ "$(uname -s)" = Darwin ]; then export SHELL=/bin/zsh; else export SHELL=/bin/bash; fi
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
  GITHUB_PATH="$work/github-path" gate_install_new_user "$stage" "$work/site" >"$work/install-step.log" 2>&1 || { cat "$work/install-step.log" >&2; exit 1; }
  [ ! -s "$work/github-path" ] || { echo "the installer wrote GITHUB_PATH: a new user's shell has none" >&2; exit 1; }
  [ "$(cat "$GATE_ATTACH/install-command.txt")" = "$GATE_DEFAULT_INSTALL" ] || { echo "install command not recorded" >&2; exit 1; }
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
  gate_finish >/dev/null 2>&1
  [ -f "$GATE_BUNDLE/A-test.json" ] || { echo "gate_finish wrote no evidence" >&2; exit 1; }
  node -e '
    const e = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
    const names = e.steps.map((s) => `${s.name}:${s.status}`);
    const want = ["install with the published command:pass", "installed binary is where the installer said:pass",
      `a new shell finds axocoatl:${process.argv[2]}`, "a step that passes:pass", "a step expected to exit 3:pass", "a step that fails:fail"];
    for (const w of want) if (!names.includes(w)) { console.error(`missing ${w} in ${names}`); process.exit(1); }
    if (e.pass !== (process.argv[2] === "pass")) { console.error("the path verdict does not follow its required steps"); process.exit(1); }
    if (!e.attachments.includes("A-test/logs/install.log")) { console.error("install.log is not attached"); process.exit(1); }
    if (!/why it failed/.test(e.steps.find((s) => s.name === "a step that fails").detail)) process.exit(1);
  ' "$GATE_BUNDLE/A-test.json" "$new_shell_expect"
)
install_status=$?
if [ "$install_status" = 0 ]; then
  pass "installer step: published install.sh through the curl shim into ~/.local/bin, no GITHUB_PATH, a new shell's PATH $new_shell_expect (as this installer prints), steps recorded"
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

# --- Tier B's Podman stand-in answers each state as Podman does, and only
# `machine start` changes anything.
(
  set -e
  shim="$script_dir/podman-shim.sh"
  export PODMAN_SHIM_STATE="$work/shim.state" PODMAN_SHIM_LOG="$work/shim.log"
  json() { node -e 'const v = JSON.parse(require("fs").readFileSync(0, "utf8")); process.stdout.write(String(eval(process.argv[1])))' "$1"; }
  echo no-machine >"$PODMAN_SHIM_STATE"
  [ "$(sh "$shim" machine list --format json)" = '[]' ]
  if sh "$shim" info --format json >/dev/null 2>&1; then echo "info answered without a machine" >&2; exit 1; fi
  echo stopped >"$PODMAN_SHIM_STATE"
  [ "$(sh "$shim" machine list --format json | json 'v[0].Running')" = false ]
  if sh "$shim" info --format json >/dev/null 2>&1; then echo "info answered with the machine stopped" >&2; exit 1; fi
  sh "$shim" machine start >/dev/null
  [ "$(cat "$PODMAN_SHIM_STATE")" = running ]
  [ "$(sh "$shim" info --format json | json 'v.host.security.rootless')" = true ]
  echo rootful >"$PODMAN_SHIM_STATE"
  [ "$(sh "$shim" info --format json | json 'v.host.security.rootless')" = false ]
  [ "$(sh "$shim" machine inspect podman-machine-default | json 'v[0].Rootful')" = true ]
  echo podman6 >"$PODMAN_SHIM_STATE"
  [ "$(sh "$shim" --version)" = 'podman version 6.1.4' ]
  [ "$(sh "$shim" machine list --format json | json 'typeof v[0].Memory')" = number ]
  [ "$(sh "$shim" machine info --format json | json 'v.Host.VMType')" = libkrun ]
  if sh "$shim" machine init >/dev/null 2>&1; then echo "machine init succeeded" >&2; exit 1; fi
  grep -qx 'machine init' "$PODMAN_SHIM_LOG"
)
shim_status=$?
if [ "$shim_status" = 0 ]; then
  pass "podman-shim.sh: no machine, stopped (machine start runs it), rootful, Podman 6; anything else fails and is logged"
else
  fail "podman-shim.sh"
fi
out=$("$script_dir/newuser-mac-ci.sh" --help 2>&1)
status=$?
if [ "$status" = 2 ] && printf '%s' "$out" | grep -q -- '--no-plan'; then
  pass "newuser-mac-ci.sh --help"
else
  fail "newuser-mac-ci.sh --help (exit $status)"
fi
case "$(uname -s)" in
  Darwin) expected='--tarball must be' ;;
  *) expected='macOS plumbing gate' ;;
esac
out=$("$script_dir/newuser-mac-ci.sh" --tarball "$work/nonexistent.tar.gz" 2>&1)
status=$?
if [ "$status" = 2 ] && printf '%s' "$out" | grep -q -- "$expected"; then
  pass "newuser-mac-ci.sh refuses before doing anything ($expected)"
else
  fail "newuser-mac-ci.sh refusal (exit $status): $out"
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
  env -u GITHUB_ACTIONS GATE_SIMULATION=1 GATE_TMPDIR="$sim" FAKE_AXOCOATL_VERSION="$version" PATH="$sim/fakebin:$PATH" \
    "$script_dir/newuser-linux.sh" --tarball "$tarball" --commit "$commit" --label A-sim \
    --evidence-root "$sim/evidence" --port 8243 >"$sim/run.log" 2>&1
  status=$?
  set -e
  [ "$status" = 1 ] || { tail -n 30 "$sim/run.log" >&2; echo "simulation exit $status, expected 1" >&2; exit 1; }
  # node-plain is onboarding's sample: checked once it is in the tree.
  present=$(cd "$repo_root/tests/fixtures/newuser" && for f in node-plain node-deps python-pytest rust go; do [ -d "$f" ] && printf '%s ' "$f"; done)
  node -e '
    const e = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
    const status = Object.fromEntries(e.steps.map((s) => [s.name, s.status]));
    const fixtures = process.argv[3].trim().split(" ");
    const want = {
      "simulation: a stand-in axocoatl and a sandboxed installer": "fail",
      "start without axocoatl on PATH": "pass",
      "install with the published command": "pass",
      "a new shell finds axocoatl": process.argv[2],
      "installed binary is the tarball\u0027s": "pass",
      "onboard --yes --writer ollama with no terminal": "pass",
      "onboard ran no sudo and no package manager through PATH": "pass",
      "onboard ran no sudo by any path (system journal)": ["pass", "skip"],
      "--no-pull pulled nothing": "pass",
      "config.yaml holds the run models (run.models)": "pass",
      "config.yaml uses the free port 8243": "pass",
      "doctor: ready, no FAIL in sandbox, models, writers, daemon": "pass",
      "doctor says Ready for: axocoatl run fix, Ready: yes, no [FAIL]": "pass",
      "axocoatl dev serves on port 8243": "pass",
      "a check program that is absent: exit 3": "pass",
      "refused before any model call (0 tokens)": "pass",
      "reviewer anthropic:x: exit 3": "pass",
      "no Session, container or model call for the refused run": "pass",
      "onboard without Podman: exit 1": "pass",
      "onboard prints the Podman install command": "pass",
      "onboard did not run sudo without --allow-sudo (PATH)": "pass",
      "onboard did not run sudo without --allow-sudo (system journal)": ["pass", "skip"],
      "doctor FAILs an empty /etc/subuid with the usermod fix": "skip",
      "Ctrl+C stops axocoatl dev with exit 0": "pass",
    };
    for (const fixture of fixtures) {
      for (const name of [`run fix on ${fixture}`, `receipt verify on ${fixture} (documented command)`, `record verify on ${fixture}`, `the model fixed ${fixture} once`]) want[name] = "pass";
    }
    if (!fixtures.includes("node-plain")) want["fixture node-plain exists"] = "fail";
    const ok = (name, value) => (Array.isArray(value) ? value : [value]).includes(status[name]);
    const wrong = Object.entries(want).filter(([name, value]) => !ok(name, value)).map(([name, value]) => `${name}: ${status[name]} (want ${value})`);
    if (wrong.length) { console.error(wrong.join("\n")); process.exit(1); }
    if (e.pass !== false) { console.error("a simulated path passed"); process.exit(1); }
  ' "$sim/evidence/$version/$commit/A-sim.json" "$new_shell_expect" "$present"
  # A fixture the model cannot fix fails its run.
  node -e '
    const f = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
    f.fixes = f.fixes.filter((fix) => fix.fixture !== "go");
    require("fs").writeFileSync(process.argv[2], JSON.stringify(f));
  ' "$script_dir/stub-fixes.json" "$sim/no-go-fix.json"
  set +e
  env -u GITHUB_ACTIONS GATE_SIMULATION=1 GATE_TMPDIR="$sim" FAKE_AXOCOATL_VERSION="$version" GATE_STUB_FIXES="$sim/no-go-fix.json" PATH="$sim/fakebin:$PATH" \
    "$script_dir/newuser-linux.sh" --tarball "$tarball" --commit "$commit" --label A-sim-broken \
    --evidence-root "$sim/evidence" --port 8244 >"$sim/broken.log" 2>&1
  set -e
  node -e '
    const e = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
    const step = (name) => e.steps.find((s) => s.name === name)?.status;
    if (step("run fix on go") !== "fail" || step("run fix on rust") !== "pass") { console.error(JSON.stringify(e.steps.filter((s) => /go|rust/.test(s.name)))); process.exit(1); }
  ' "$sim/evidence/$version/$commit/A-sim-broken.json"

  # --live (D1): this account's own brand-new HOME, interactive onboarding
  # that is told --start none, and the daemon checked on the port
  # onboarding chose (FAKE_DEFAULT_PORT stands for "8080 when free"). The
  # stub stands for the models onboarding configured.
  live_home="$sim/live-home"
  mkdir -p "$live_home"
  node "$script_dir/stub-model-server.mjs" --port 0 --port-file "$sim/live-stub.port" >"$sim/live-stub.log" 2>&1 &
  live_stub=$!
  for _ in $(seq 1 100); do [ -s "$sim/live-stub.port" ] && break; sleep 0.1; done
  live_env=(env -u GITHUB_ACTIONS HOME="$live_home" GATE_SIMULATION=1 GATE_TMPDIR="$sim" GATE_IN_SCRIPT="$sim/live-transcript.txt"
    FAKE_AXOCOATL_VERSION="$version" FAKE_ONBOARD_OPERATOR=1 FAKE_DEFAULT_PORT=8245
    FAKE_OLLAMA_URL="http://127.0.0.1:$(cat "$sim/live-stub.port")" PATH="$sim/fakebin:$PATH")
  set +e
  "${live_env[@]}" "$script_dir/newuser-linux.sh" --live --tarball "$tarball" --commit "$commit" --label D1-sim \
    --evidence-root "$sim/evidence" </dev/null >"$sim/live.log" 2>&1
  status=$?
  # The same account again is no longer brand-new: refused before anything.
  "${live_env[@]}" "$script_dir/newuser-linux.sh" --live --tarball "$tarball" --commit "$commit" --label D1-again \
    --evidence-root "$sim/evidence" </dev/null >"$sim/live-again.log" 2>&1
  again=$?
  set -e
  kill "$live_stub" 2>/dev/null || true
  [ "$status" = 1 ] || { tail -n 30 "$sim/live.log" >&2; echo "live simulation exit $status, expected 1" >&2; exit 1; }
  node -e '
    const e = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
    const status = Object.fromEntries(e.steps.map((s) => [s.name, s.status]));
    const fixtures = process.argv[2].trim().split(" ");
    const want = {
      "onboard (interactive, every consent answered by the operator; --start none)": "pass",
      "doctor says Ready for: axocoatl run fix, Ready: yes, no [FAIL]": "pass",
      "axocoatl dev serves on port 8245": "pass",
      "negative cases with the scripted model server": "skip",
    };
    for (const fixture of fixtures) for (const name of [`run fix on ${fixture}`, `record verify on ${fixture}`]) want[name] = "pass";
    const config = e.steps.find((s) => s.name.startsWith("onboarding wrote "));
    if (config?.status !== "pass" || !/server\.port 8245/.test(config.detail)) { console.error(JSON.stringify(config)); process.exit(1); }
    const wrong = Object.entries(want).filter(([name, value]) => status[name] !== value).map(([name, value]) => `${name}: ${status[name]} (want ${value})`);
    if (wrong.length) { console.error(wrong.join("\n")); process.exit(1); }
    if (e.tier !== "D") { console.error(`tier ${e.tier}`); process.exit(1); }
  ' "$sim/evidence/$version/$commit/D1-sim.json" "$present"
  grep -q -- '--model' "$sim/evidence/$version/$commit/D1-sim/readme-command.txt" && { echo "the live run kept the README's --model options" >&2; exit 1; }
  if [ "$again" != 2 ] || ! grep -q 'needs a brand-new account' "$sim/live-again.log"; then
    cat "$sim/live-again.log" >&2
    echo "a second live run in the same account was not refused" >&2
    exit 1
  fi
  [ ! -e "$sim/evidence/$version/$commit/D1-again.json" ] || { echo "the refused live run wrote evidence" >&2; exit 1; }
  [ -d "$live_home" ] || { echo "the live run removed the account's HOME" >&2; exit 1; }
)
sim_status=$?
if [ "$sim_status" = 0 ]; then
  pass "newuser-linux.sh against a stand-in axocoatl: every Tier A step recorded as the contract says; a fixture the model cannot fix fails its run; --live keeps the account's HOME, starts nothing from onboarding and finds the daemon on the port onboarding chose"
else
  fail "newuser-linux.sh simulation"
fi

# --- newuser-linux.sh refuses what it must, before it changes anything.
(
  set -e
  refusal_tar="$work/refusal/axocoatl-v1.5.0-x86_64-unknown-linux-gnu.tar.gz"
  mkdir -p "${refusal_tar%/*}"
  : >"$refusal_tar"
  set +e
  out=$(env -u GITHUB_ACTIONS GATE_SIMULATION=1 "$script_dir/newuser-linux.sh" --tarball "$refusal_tar" \
    --commit "$(printf 'e%.0s' $(seq 1 40))" --system-changes 2>&1)
  status=$?
  set -e
  if [ "$status" != 2 ] || ! printf '%s' "$out" | grep -q 'runs only on a CI runner'; then
    echo "--system-changes outside CI: $status $out" >&2
    exit 1
  fi
  set +e
  out=$(GATE_SIMULATION=1 "$script_dir/newuser-linux.sh" --tarball "$refusal_tar" \
    --commit "$(printf 'e%.0s' $(seq 1 40))" --label '../other-commit' 2>&1)
  status=$?
  set -e
  if [ "$status" != 2 ] || ! printf '%s' "$out" | grep -q -- '--label must be'; then
    echo "--label ../: $status $out" >&2
    exit 1
  fi
)
refusal_status=$?
if [ "$refusal_status" = 0 ]; then
  pass "newuser-linux.sh refuses --system-changes outside CI and a label that leaves its bundle"
else
  fail "newuser-linux.sh refusals"
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
  # The paths a release needs; Tier A comes from a CI run, the rest from
  # the live tiers.
  required=$(node -e '
      import("'"$script_dir"'/evidence.mjs").then((m) => {
        const text = require("fs").readFileSync(process.argv[1], "utf8");
        console.log(m.parseRequiredPaths(text).filter((p) => p.when === "always").map((p) => p.name).join(" "));
      });' "$repo/docs/RELEASE_GATE.md")
  make_path() { # make_path DIR NAME STATUS
    mkdir -p "$1/$2"
    printf '%s transcript\n' "$2" >"$1/$2/transcript.txt"
    node "$script_dir/evidence.mjs" add-step --file "$1/$2.steps" --name "run fix" --status "$3" --exit 0 --seconds 2 --bytes 10
    node "$script_dir/evidence.mjs" path-file --steps "$1/$2.steps" --out "$1/$2.json" --path "$2" \
      --tier "${2%%-*}" --version 1.5.0 --commit "$commit" --attach "$2/transcript.txt" >/dev/null || true
    rm "$1/$2.steps"
  }
  from="$work/from"
  mkdir -p "$from"
  for name in $required; do
    case "$name" in A-*) ;; *) make_path "$from" "$name" pass ;; esac
  done
  # A path the release does not require may fail: a warning, not a refusal.
  make_path "$from" C1-real-repo fail

  # A stand-in gh: run 111 is CI's push run of this commit; 222 is a fork's
  # pull request; 333 ran another commit; 444 is the release workflow.
  mkdir -p "$work/gh-bin"
  cat >"$work/gh-bin/gh" <<GH
#!/bin/sh
run() { printf '{"head_sha":"%s","event":"%s","path":"%s","status":"completed","head_branch":"main","head_repository":{"full_name":"%s"},"repository":{"full_name":"axocoatl/axocoatl"}}\n' "\$1" "\$2" "\$3" "\$4"; }
case "\$1 \$2" in
  "api repos/axocoatl/axocoatl/actions/runs/111") run $commit push .github/workflows/ci.yml axocoatl/axocoatl ;;
  "api repos/axocoatl/axocoatl/actions/runs/222") run $commit pull_request .github/workflows/ci.yml someone/axocoatl ;;
  "api repos/axocoatl/axocoatl/actions/runs/333") run $(printf 'f%.0s' $(seq 1 40)) push .github/workflows/ci.yml axocoatl/axocoatl ;;
  "api repos/axocoatl/axocoatl/actions/runs/444") run $commit push .github/workflows/release.yml axocoatl/axocoatl ;;
  "run download")
    [ "\$3" = 111 ] || { echo "gh stand-in: downloaded run \$3" >>"$work/gh-downloads"; }
    shift 2; dir=''
    while [ \$# -gt 0 ]; do [ "\$1" = --dir ] && dir=\$2; shift; done
    # Each leg uploads its own release-evidence/ (one path).
    for label in A-x86 A-arm; do
      mkdir -p "\$dir/newuser-\$label/1.5.0/$commit"
      cp -R "$work/ci-evidence/1.5.0/$commit/\$label.json" "$work/ci-evidence/1.5.0/$commit/\$label" \
        "\$dir/newuser-\$label/1.5.0/$commit/"
    done ;;
  *) echo "gh stand-in: unexpected \$*" >&2; exit 1 ;;
esac
GH
  chmod +x "$work/gh-bin/gh"
  for name in A-x86 A-arm; do
    make_path "$work/ci-evidence/1.5.0/$commit" "$name" pass
  done
  for bad in 222:'pull_request run' 333:'not '"$commit" 444:'not .github/workflows/ci.yml'; do
    if PATH="$work/gh-bin:$PATH" "$script_dir/collect-evidence.sh" --repo-root "$repo" --version 1.5.0 --commit "$commit" \
      --ci-run "${bad%%:*}" --no-sign >"$work/bad-run.log" 2>&1; then
      echo "run ${bad%%:*} was collected" >&2
      exit 1
    fi
    grep -q "refusing run ${bad%%:*}: .*${bad#*:}" "$work/bad-run.log" || { cat "$work/bad-run.log" >&2; exit 1; }
  done
  [ ! -s "$work/gh-downloads" ] || { echo "a refused run was downloaded" >&2; exit 1; }
  rm -rf "$repo/release-evidence"

  # A transcript or file that looks like a credential is refused.
  for leak in 'OPENROUTER_API_KEY=sk-or-v1-0123456789abcdef0123456789' \
    'id_token=eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ1c2VyMTIzNDU2In0.c2lnbmF0dXJl' \
    'GH=github_pat_11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz' \
    'OPENAI_API_KEY=sk-svcacct-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789' \
    'file:C1-local/secrets/openrouter'; do
    rm -rf "$work/leaky" "$repo/release-evidence"
    cp -R "$from" "$work/leaky"
    case "$leak" in
      file:*) mkdir -p "$(dirname "$work/leaky/${leak#file:}")" && printf 'x\n' >"$work/leaky/${leak#file:}" ;;
      *) printf '%s\n' "$leak" >>"$work/leaky/D1/transcript.txt" ;;
    esac
    if "$script_dir/collect-evidence.sh" --repo-root "$repo" --version 1.5.0 --commit "$commit" --from "$work/leaky" --no-sign >"$work/leaky.log" 2>&1; then
      echo "a credential-looking transcript was collected: $leak" >&2
      exit 1
    fi
    grep -q 'look like they hold a credential' "$work/leaky.log" || { cat "$work/leaky.log" >&2; exit 1; }
  done
  # Prose that merely contains "sk-" is not a key.
  rm -rf "$work/leaky" "$repo/release-evidence"
  cp -R "$from" "$work/leaky"
  printf 'the risk-assessment-of-the-installer-and-everything-else step\n' >>"$work/leaky/D1/transcript.txt"
  "$script_dir/collect-evidence.sh" --repo-root "$repo" --version 1.5.0 --commit "$commit" --from "$work/leaky" --no-sign >"$work/prose.log" 2>&1 \
    || { cat "$work/prose.log" >&2; exit 1; }
  rm -rf "$repo/release-evidence"

  if [ -n "$bin" ]; then
    # The gate account: its own Axocoatl data root (which every reset
    # removes) and the gate's signing key, made once, apart from it. A data
    # root named by AXOCOATL_DATA_DIR must never be touched.
    sign_home="$work/sign-home"
    pretend="$work/pretend-real-data-root"
    mkdir -p "$sign_home" "$pretend"
    export AXOCOATL_DATA_DIR="$pretend"
    HOME="$sign_home" "$script_dir/collect-evidence.sh" --init-signer --axocoatl "$bin" 2>/dev/null >"$repo/.axocoatl/gate-keys"
    grep -q '^ed25519 ' "$repo/.axocoatl/gate-keys"
    HOME="$sign_home" env -u AXOCOATL_DATA_DIR "$bin" receipt key >"$work/product-key" 2>/dev/null
    cmp -s "$work/product-key" "$repo/.axocoatl/gate-keys" && { echo "the gate key is the product's own receipt key" >&2; exit 1; }
    # A reset between the key's registration and the next collect.
    while IFS= read -r path; do rm -rf "$path"; done < <(HOME="$sign_home" "$script_dir/reset-mac-account.sh" --print-paths)
    HOME="$sign_home" PATH="$work/gh-bin:$PATH" "$script_dir/collect-evidence.sh" --repo-root "$repo" --version 1.5.0 --commit "$commit" \
      --from "$from" --ci-run 111 --axocoatl "$bin" >"$work/collect.log" 2>&1 || { cat "$work/collect.log" >&2; exit 1; }
    grep -q 'C1-real-repo: warning: failed (not a required path)' "$work/collect.log" || { cat "$work/collect.log" >&2; exit 1; }
    grep -q 'collected A-arm from' "$work/collect.log"
    grep -q '^Next:' "$work/collect.log"
    [ -z "$(ls -A "$pretend")" ] || { echo "AXOCOATL_DATA_DIR was written: $(ls -A "$pretend")" >&2; exit 1; }
    [ -n "$(find "$sign_home/.axocoatl-gate-signer" -name 'receipt-signing-key*' 2>/dev/null)" ] \
      || { echo "the gate key is not in the signer" >&2; exit 1; }
    git -C "$repo" -c user.name=t -c user.email=t@example.invalid add -A
    git -C "$repo" -c user.name=t -c user.email=t@example.invalid commit -qm evidence
    node "$script_dir/check-evidence.mjs" --root "$repo" --version 1.5.0 --tag-commit "$(git -C "$repo" rev-parse HEAD)" >"$work/check.log" 2>&1 \
      || { cat "$work/check.log" >&2; exit 1; }
    grep -q '^New-user gate: PASS for v1.5.0' "$work/check.log"
    # A required path that fails: written and signed, but collect exits 1
    # after the comparison and the next steps, naming what is missing.
    make_path "$work/failing" D1 fail
    set +e
    HOME="$sign_home" "$script_dir/collect-evidence.sh" --repo-root "$repo" --version 1.5.0 --commit "$commit" \
      --from "$work/failing" --replace --axocoatl "$bin" >"$work/collect-fail.log" 2>&1
    status=$?
    set -e
    if [ "$status" != 1 ] || ! grep -q 'release needs: D1' "$work/collect-fail.log" || ! grep -q '^Next:' "$work/collect-fail.log"; then
      cat "$work/collect-fail.log" >&2
      exit 1
    fi
  else
    PATH="$work/gh-bin:$PATH" "$script_dir/collect-evidence.sh" --repo-root "$repo" --version 1.5.0 --commit "$commit" \
      --from "$from" --ci-run 111 --no-sign >"$work/collect.log" 2>&1
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
  signed="a bundle signed by ${bin#"$repo_root"/}'s gate signer, after a reset, passes check-evidence; a failing optional path warns, a failing required one exits 1"
else
  signed="an unsigned bundle fails check-evidence (no axocoatl built)"
fi
if [ "$collect_status" = 0 ]; then
  pass "collect-evidence.sh: takes Tier A only from the canonical repository's ci.yml push run of the commit; refuses credential-looking files and secret file names; $signed"
else
  fail "collect-evidence.sh"
fi

echo "Release gate scripts: $((checks - failures))/$checks passed"
[ "$failures" = 0 ]
