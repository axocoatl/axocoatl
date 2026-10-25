#!/bin/sh
# Make ../root again with Axocoatl 1.4.0, as it was made:
#   make.sh AXOCOATL_1_4_0 WORK_DIR
# AXOCOATL_1_4_0 is Axocoatl 1.4.0 (the released binary or one built from the
# v1.4.0 tag); WORK_DIR is a new directory (the daemon's socket is ./axo.sock
# in it). The data root records WORK_DIR's paths and the fixture is public, so
# give it a neutral path that names no person, such as /tmp/axo140 (on macOS
# it must be under /private or the home folder, which the Podman machine
# shares). The test points the copy's Workspace and Sessions at a repository
# of its own; if WORK_DIR or the Ollama port changes, so do RECORDED_REPO and
# RECORDED_OLLAMA in the test. Podman must answer, and python3 runs the
# stand-in for Ollama 0.20.6 (fake_ollama.py), whose writer delegates to its
# helper or runs one bash command and whose reviewer approves. Ports 8373 and
# 8374 must be free.
set -eu
[ "$#" -eq 2 ] || { echo "usage: make.sh AXOCOATL_1_4_0 WORK_DIR" >&2; exit 2; }
axocoatl=$1
work=$2
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
"$axocoatl" --version | grep -qx 'axocoatl 1.4.0' || { echo "not Axocoatl 1.4.0: $axocoatl" >&2; exit 1; }
mkdir "$work"
work="$(CDPATH= cd -- "$work" && pwd -P)"
mkdir "$work/repo" "$work/cfg" "$work/ollama"
echo 0.20.6 > "$work/ollama/version"
cat > "$work/cfg/config.yaml" <<'EOF'
agents:
  - id: lead
    name: "Lead"
    provider: ollama
    model: "writer-model"
    role: autonomous
    tools: [read_file, list_dir, grep, glob, write_file, edit_file, bash]
    sampling:
      max_tokens: 4096
  - id: reviewer
    name: "Reviewer"
    provider: ollama
    model: "reviewer-model"
    role: worker
    tools: [read_file, list_dir, grep, glob, bash]
    writes: []
    sampling:
      max_tokens: 4096
  - id: scout
    name: "Scout"
    provider: ollama
    model: "helper-model"
    role: worker
    tools: [read_file, list_dir, grep, glob]
    writes: []
    sampling:
      max_tokens: 2048
providers:
  ollama:
    base_url: "http://127.0.0.1:8373"
server:
  port: 8374
  host: "127.0.0.1"
EOF
git -C "$work/repo" init -q -b main
printf 'A tiny repository for the Axocoatl 1.4.0 data-root fixture.\n' > "$work/repo/README.md"
git -C "$work/repo" add README.md
git -C "$work/repo" -c user.email=fixture@example.com -c user.name=Fixture commit -qm "Add the README"

python3 -I "$here/fake_ollama.py" 8373 "$work/ollama" > "$work/fake-ollama.log" 2>&1 &
ollama_pid=$!
cd "$work/cfg"
export AXOCOATL_DATA_DIR="$work/root" AXOCOATL_SOCKET_PATH=./axo.sock
"$axocoatl" serve --config "$work/cfg/config.yaml" > "$work/daemon.log" 2>&1 &
daemon_pid=$!
trap 'kill "$daemon_pid" "$ollama_pid" 2>/dev/null || true' EXIT
for _ in $(seq 1 60); do
  curl -fs -m 2 http://127.0.0.1:8374/health >/dev/null && break
  sleep 1
done

# An `axocoatl run fix` that passes.
printf '%s\n' 'printf "fixed\n" > fixed.txt' > "$work/ollama/writer-cmd"
"$axocoatl" run fix --task "Create fixed.txt saying fixed" --repo "$work/repo" \
  --check "test -f fixed.txt" --model writer=ollama:writer-model \
  --model reviewer=ollama:reviewer-model --config "$work/cfg/config.yaml"
# A workbench Session on Lead with an applied Team and one turn.
printf '%s\n' 'printf "notes\n" > notes.txt' > "$work/ollama/writer-cmd"
python3 -I "$here/workbench_session.py" http://127.0.0.1:8374 "$work/root/local-api-token" \
  "$work/repo" Notes "Add notes.txt"
# A workbench Session whose Team lets Lead delegate to its helpers (Reviewer
# and Scout, the read-only Workers), and one turn in which Lead asks Scout.
python3 -I "$here/workbench_session.py" http://127.0.0.1:8374 "$work/root/local-api-token" \
  "$work/repo" Helpers "Ask Scout what this repository is for" helpers
# And one never used: no Team, no turn.
python3 -I "$here/workbench_session.py" http://127.0.0.1:8374 "$work/root/local-api-token" \
  "$work/repo" Unused ""

kill -TERM "$daemon_pid"
wait "$daemon_pid" || true
# Without the embedding model and the supervisor (each version brings its
# own), the receipt key and the API token. Git keeps no empty directory, and
# .gitignore keeps out .axocoatl-daemon.lock, which the test renames back.
rm -rf "$here/../root"
mkdir "$here/../root"
(cd "$work/root" && tar -cf - --exclude ./models --exclude ./execution-supervisors \
  --exclude ./secrets --exclude ./local-api-token .) | (cd "$here/../root" && tar -xf -)
mv "$here/../root/.axocoatl-daemon.lock" "$here/../root/dot-axocoatl-daemon.lock"
find "$here/../root" -type d -empty -delete
