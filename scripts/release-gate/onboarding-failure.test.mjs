// Run with node --test; test-release-gate.sh includes every *.test.mjs.
// Execute real shell control flow with synthetic commands and evidence.
// No account, provider, daemon, GitHub repository or HOME is changed.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const gate = readFileSync(path.join(here, 'newuser-mac.sh'), 'utf8');
const lib = readFileSync(path.join(here, 'lib.sh'), 'utf8');
function shellFunction(source, name, optional = false) {
  const found = source.match(new RegExp('^' + name + '\\(\\) \\{[\\s\\S]*?^\\}', 'm'))?.[0];
  if (!optional) assert.ok(found, 'missing shell function ' + name);
  return found ?? '';
}
const loop = gate.slice(gate.indexOf('\nfailed=0\n'));
assert.ok(loop.startsWith('\nfailed=0\n'), 'the actual path loop is tested');

function simulate(selectedPath, assumeYes, commandExit, failAt = 1, connectExit = 0) {
  const dir = mkdtempSync(path.join(tmpdir(), 'axo-onboarding-stop-'));
  const source = [
    ...['gate_begin', 'gate_record', 'gate_skip', 'gate_finish'].map(n => shellFunction(lib, n)),
    shellFunction(lib, 'gate_step').replace(/^gate_step/, 'actual_gate_step'),
    shellFunction(gate, 'onboarding_step', true),
    ...['onboard', 'path_local', 'path_claude_code', 'path_codex', 'path_openrouter', 'path_service', 'run_path'].map(n => shellFunction(gate, n)),
  ].join('\n');
  writeFileSync(path.join(dir, 'functions.sh'), source);
  writeFileSync(path.join(dir, 'loop.sh'), loop);
  writeFileSync(path.join(dir, 'transcript.txt'), 'synthetic interaction only\n');
  const script = String.raw`
set -u
scratch=$1
gate_evidence_js=$2/evidence.mjs
gate_commands_js=$2/published-commands.mjs
gate_repo=$2/../..
. "$scratch/functions.sh"
evidence_root=$scratch/evidence
version=1.5.0
commit=cccccccccccccccccccccccccccccccccccccccc
machine=C1
work=$scratch/work
transcript=$scratch/transcript.txt
assume_yes=$4
command_exit=$5
fail_at=$6
connect_exit=$7
paths=$3,real-repo
onboarding_interrupted=0
calls=$scratch/calls
counter=$scratch/count
: >"$calls"
echo 0 >"$counter"
task='synthetic task'
site=$scratch
# Record all dependent commands without starting real tools or repositories.
dependent() { printf '%s\n' "$*" >>"$calls"; }
gate_say() { :; }
gate_die() { echo "$*" >&2; exit 97; }
gate_valid_name() { return 0; }
gate_valid_version() { return 0; }
gate_valid_commit() { return 0; }
gate_now_ms() { node -e 'process.stdout.write(String(Date.now()))'; }
gate_rx_bytes() { echo 0; }
gate_slug() { echo synthetic-step; }
path_begin() {
  gate_begin "$evidence_root" "$version" "$commit" "C1-$1" C
  gate_record 'earlier required failure is retained' fail 9 '' '' 'synthetic earlier failure'
}
path_end() { gate_finish; }
install_once() { return 0; }
signed_out_before() { :; }
signed_in_during_onboarding() { dependent sign-in-observation; }
doctor_ready() { dependent doctor; }
ensure_daemon() { dependent daemon; }
confirm() { dependent confirmation; }
local_setup_steps() { dependent local-setup; }
all_fixtures() { echo node-deps; }
run_readme_on() { dependent fixture; }
run_published_on() { dependent fixture; }
summary() { :; }
keep_pr() { dependent PR-reset; }
gate_fixture_repo() { mkdir -p "$2"; }
gate_check() { :; }
stop_own_dev() { :; }
daemon_port() { echo 1; }
config_file() { echo "$scratch/config.yaml"; }
gate_service_status() { dependent service-status; }
gate_wait_daemon() { dependent service-wait; echo current; }
gate_daemon_state_says() { echo synthetic; }
gate_run_model() { echo openrouter:synthetic; }
onboard_output() { dependent onboard-output; : >"$1"; }
gate_service_restart_steps() { dependent service-restart; }
gate_service_end() { dependent service-stop; }
path_real_repo() { dependent INDEPENDENT; }
axocoatl() {
  if [ "$1" = connect ]; then
    dependent connect
    return "$connect_exit"
  fi
  if [ "$1" = onboard ]; then
    n=$(cat "$counter"); n=$((n + 1)); echo "$n" >"$counter"
    dependent "onboard-$n"
    [ "$n" != "$fail_at" ] || return "$command_exit"
    return 0
  fi
  dependent "other-cli $*"
}
gate_step() {
  case "$*" in
    *'-- axocoatl onboard '* | *'-- axocoatl connect openrouter') actual_gate_step "$@" ;;
    *) dependent "other-step $1"; return 0 ;;
  esac
}
. "$scratch/loop.sh"
`;
  try {
    const result = spawnSync('/bin/bash', ['-c', script, 'gate-onboarding-test', dir, here,
      selectedPath, String(assumeYes), String(commandExit), String(failAt), String(connectExit)], {
      encoding: 'utf8', timeout: 15000,
    });
    assert.equal(result.error, undefined, result.error?.message);
    const evidence = JSON.parse(readFileSync(path.join(dir, 'evidence', '1.5.0',
      'c'.repeat(40), 'C1-' + selectedPath + '.json'), 'utf8'));
    return { result, evidence, calls: readFileSync(path.join(dir, 'calls'), 'utf8').trim().split('\n') };
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

for (const selectedPath of ['local', 'claude-code', 'codex', 'openrouter', 'service']) {
  for (const assumeYes of [0, 1]) {
    for (const commandExit of [1, 130]) {
      test(selectedPath + ': onboarding exit ' + commandExit + (assumeYes ? ' with --yes' : ' interactively'), () => {
        const { result, evidence, calls } = simulate(selectedPath, assumeYes, commandExit);
        assert.equal(result.status, commandExit === 130 ? 130 : 1, result.stderr);
        assert.equal(evidence.pass, false);
        assert.ok(evidence.steps.some(s => s.name === 'earlier required failure is retained' && s.status === 'fail'));
        const failure = evidence.steps.find(s => s.name.startsWith('onboard') && s.status === 'fail');
        assert.equal(failure?.exit_code, commandExit, JSON.stringify(evidence.steps));
        assert.ok(!evidence.steps.some(s => s.name === 'onboarding was answered live in the terminal'),
          'failed onboarding cannot claim completed interaction');
        const before = selectedPath === 'openrouter' && assumeYes ? ['connect'] : [];
        assert.deepEqual(calls, [...before, 'onboard-1', ...(commandExit === 130 ? [] : ['INDEPENDENT'])],
          'no fixtures, sign-in attestations, daemon actions, or PR reset after failed onboarding');
      });
    }
  }
}

for (const assumeYes of [0, 1]) {
  for (const commandExit of [1, 130]) {
    test('service: second onboarding exits ' + commandExit + (assumeYes ? ' with --yes' : ''), () => {
      const { result, evidence, calls } = simulate('service', assumeYes, commandExit, 2);
      assert.equal(result.status, commandExit === 130 ? 130 : 1, result.stderr);
      assert.deepEqual(calls, ['onboard-1', 'service-status', 'service-wait', 'onboard-2',
        ...(commandExit === 130 ? [] : ['INDEPENDENT'])]);
      assert.ok(evidence.steps.some(s => s.status === 'fail' && s.exit_code === commandExit));
      const completions = evidence.steps.filter(s => s.name === 'onboarding was answered live in the terminal');
      assert.equal(completions.length, 1, 'only the completed first onboarding may be recorded');
    });
  }
}

for (const commandExit of [1, 130]) {
  test('OpenRouter prerequisite connection exits ' + commandExit + ' before onboarding', () => {
    const { result, evidence, calls } = simulate('openrouter', 1, 0, 1, commandExit);
    assert.equal(result.status, commandExit === 130 ? 130 : 1, result.stderr);
    assert.deepEqual(calls, ['connect', ...(commandExit === 130 ? [] : ['INDEPENDENT'])]);
    assert.ok(evidence.steps.some(s => s.name.startsWith('connect OpenRouter') &&
      s.status === 'fail' && s.exit_code === commandExit));
    assert.ok(!evidence.steps.some(s => s.name.startsWith('onboard')));
  });
}

test('successful onboarding still permits its required dependent work', () => {
  const { evidence, calls } = simulate('local', 0, 0);
  assert.ok(calls.includes('fixture'));
  assert.ok(calls.includes('INDEPENDENT'));
  assert.ok(evidence.steps.some(s => s.name === 'onboarding was answered live in the terminal' && s.status === 'pass'));
});
