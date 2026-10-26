#!/usr/bin/env python3
"""Exercise real suite selection, prerequisite inventory and image setup offline."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

root = Path(__file__).resolve().parent.parent
runner = (root / "scripts/test-podman-suites.sh").read_text()
egresstest = "bootstrap::session_repository::tests::activation_tests::runtime_policy_tests::actual_egress_to_real_ollama_keeps_to_cloud_off_servers_and_local_models"
json_test = "native::observed_context::watch::a_reasoning_disabled_json_request_reads_on_a_live_ollama"
manual_test = "giving_up_on_a_load_makes_a_live_ollama_abort_it"
with tempfile.TemporaryDirectory(prefix="axocoatl-suite-routing.") as tmp:
    work = Path(tmp)
    fixture = work / "repo"
    scripts = fixture / "scripts"
    scripts.mkdir(parents=True)
    target = scripts / "test-podman-suites.sh"
    target.write_text(runner)
    # check invokes its regression programs; avoid recursion in this fixture.
    (scripts / "test-podman-suites-parser.sh").write_text("#!/bin/sh\nexit 0\n")
    (scripts / "test-podman-suites-routing.py").write_text("pass\n")
    tracked = subprocess.check_output(["git", "ls-files", "--", "crates/*.rs", "axocoatl-cli/*.rs", "examples/*.rs"], cwd=root, text=True).splitlines()
    tracked = [name for name in tracked if "#[ignore" in (root / name).read_text()]
    for name in tracked:
        dest = fixture / name
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(root / name, dest)
    for crate in ["axocoatl-daemon", "axocoatl-llm-ollama"]:
        lib = fixture / "crates" / crate / "src/lib.rs"
        lib.parent.mkdir(parents=True, exist_ok=True)
        lib.touch(exist_ok=True)
    bin_dir = work / "bin"
    bin_dir.mkdir()
    listing = work / "tracked"
    listing.write_text("\n".join(tracked) + "\n")
    (bin_dir / "git").write_text('#!/bin/sh\n[ "$1" = ls-files ] || exit 91\ncat "$AXO_ROUTING_TRACKED"\n')
    (bin_dir / "cargo").write_text('''#!/usr/bin/env python3
import sys
args=sys.argv[1:]
assert "--list" in args, "this regression must never build or run Rust tests"
crate=args[args.index("-p")+1]
filters=args[args.index("--")+1:]
skip=[filters[i+1] for i, arg in enumerate(filters[:-1]) if arg == "--skip"]
selected=[]
i=0
while i < len(filters):
    if filters[i] == "--skip": i+=2; continue
    if filters[i] == "--format": i+=2; continue
    if not filters[i].startswith("--"): selected.append(filters[i])
    i+=1
names={
"axocoatl-daemon": [
"bootstrap::session_repository::tests::activation_tests::runtime_policy_tests::actual_egress_to_real_ollama_keeps_to_cloud_off_servers_and_local_models",
"session_egress::host_ollama_tests::actual_ollama_cloud_mode_decides_each_request_on_the_host_route",
"session_dispatch::tests::ollama_tests::native_ollama_real_model_refuses_over_long_input_and_stops_at_its_limit",
"session_dispatch::tests::ollama_tests::native_ollama_real_model_through_durable_controller",
"ordinary_podman_test"],
"axocoatl-llm-ollama": [
"native::observed_context::watch::the_contract_probes_hold_on_a_live_ollama",
"native::observed_context::watch::a_reasoning_disabled_json_request_reads_on_a_live_ollama"]}
for name in names[crate]:
    if name not in skip and (not selected or name in selected): print(name+": test")
''')
    (bin_dir / "podman").write_text('''#!/bin/sh
printf '%s\n' "$*" >> "$AXO_ROUTING_PODMAN"
if [ "$1 $2" = 'image inspect' ]; then exit 1; fi
''')
    for path in bin_dir.iterdir():
        path.chmod(0o755)
    env = dict(os.environ, PATH=str(bin_dir) + os.pathsep + os.environ["PATH"], AXO_ROUTING_TRACKED=str(listing), AXO_ROUTING_PODMAN=str(work / "podman.log"))

    def run(*args, expected=0, contains=None):
        result = subprocess.run(["bash", str(target), *args], cwd=fixture, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        assert result.returncode == expected, (args, result.returncode, result.stdout)
        if contains:
            assert contains in result.stdout, (args, contains, result.stdout)
        return result.stdout

    run("check")
    ollama = run("list", "ollama")
    daemon = run("list", "daemon")
    assert egresstest in ollama and json_test in ollama and egresstest not in daemon
    assert daemon.splitlines() == ["axocoatl-daemon ordinary_podman_test"], daemon
    assert len(ollama.splitlines()) == 6, ollama
    print("ok: real-container and JSON cases select the configured Ollama group only")

    message = "has no explicit Ollama group or manual diagnostic assignment"
    entry = f'  "axocoatl-daemon {egresstest}"\n'
    assert entry in runner
    target.write_text(runner.replace(entry, "", 1))
    run("check", expected=1, contains=message)
    print("ok: removing a live-Ollama route is refused before a build")
    target.write_text(runner)
    added = "crates/axocoatl-llm-ollama/src/unassigned_test.rs"
    (fixture / added).write_text('#[ignore = "needs a live Ollama"]\nasync fn another_live_server_case() {}\n')
    listing.write_text("\n".join(tracked + [added]) + "\n")
    run("check", expected=1, contains=message)
    print("ok: a newly ignored Ollama test cannot disappear behind exact filters")
    (fixture / added).write_text('#[ignore]\nasync fn another_live_server_case() {}\n')
    run("check", expected=1, contains=message)
    print("ok: every ignored llm-ollama test needs routing even without prerequisite wording")
    daemon_added = "crates/axocoatl-daemon/src/unassigned_test.rs"
    (fixture / daemon_added).write_text('#[ignore = "requires AXO_LIVE_OLLAMA_CLOUD_OFF_PORT"]\nasync fn another_live_server_case() {}\n')
    listing.write_text("\n".join(tracked + [daemon_added]) + "\n")
    run("check", expected=1, contains=message)
    print("ok: uppercase Ollama environment prerequisites outside the exact-filtered crate are checked")
    listing.write_text("\n".join(tracked) + "\n")
    target.write_text("\n".join(line for line in runner.splitlines() if f"axocoatl-llm-ollama {manual_test}|" not in line) + "\n")
    run("check", expected=1, contains=message)
    print("ok: manual diagnostic coverage also requires an explicit reasoned entry")
    target.write_text(runner)
    run("prepare", "ollama")
    calls = (work / "podman.log").read_text().splitlines()
    pulls = [line for line in calls if line.startswith("pull ")]
    assert len(pulls) == 1 and "docker.io/library/alpine@sha256:" in pulls[0], calls
    assert any("apk add --no-cache coreutils findutils git grep" in line for line in calls), calls
    assert any(line.startswith("build ") and "supervisor-root.Containerfile" in line for line in calls), calls
    print("ok: Ollama preparation builds its actual Session tools image from pinned Alpine")
print("Podman suite routing contract: PASS (7 cases)")
