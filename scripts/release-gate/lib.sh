# shellcheck shell=bash
# Scripts handed to `bash -c` take their values as positional parameters,
# so their single quotes are deliberate.
# shellcheck disable=SC2016
# Shared by the new-user gate scripts (docs/RELEASE_GATE.md). Source it from
# bash 3.2 or later (macOS's /bin/bash included); it needs node and git.
#
# A gate path records steps with gate_step/gate_record and ends with
# gate_finish, which writes <bundle>/<path>.json; every file the path keeps
# (logs, receipts, records, transcripts) goes under <bundle>/<path>/.

gate_dir=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
gate_repo=$(CDPATH='' cd -- "$gate_dir/../.." && pwd)
gate_evidence_js="$gate_dir/evidence.mjs"
# shellcheck disable=SC2034 # used by the scripts that source this file
gate_commands_js="$gate_dir/published-commands.mjs"

gate_say() { printf 'gate: %s\n' "$*" >&2; }
gate_die() {
  printf 'gate: %s\n' "$*" >&2
  exit 2
}

gate_now_ms() { node -e 'process.stdout.write(String(Date.now()))'; }

# Bytes received on every non-loopback interface so far.
gate_rx_bytes() {
  case "$(uname -s)" in
    Linux)
      local total=0 dev value
      for dev in /sys/class/net/*; do
        [ "${dev##*/}" = lo ] && continue
        [ -r "$dev/statistics/rx_bytes" ] || continue
        value=$(cat "$dev/statistics/rx_bytes" 2>/dev/null || echo 0)
        total=$((total + value))
      done
      echo "$total"
      ;;
    Darwin)
      netstat -ibn 2>/dev/null | awk '
        $3 ~ /^<Link#/ && $1 !~ /^lo/ { sum += (NF >= 11 ? $7 : $6) }
        END { printf "%.0f\n", sum }'
      ;;
    *) echo 0 ;;
  esac
}

# Free space in KiB on the volume holding $1.
gate_free_kb() { df -Pk "$1" 2>/dev/null | awk 'NR == 2 { print $4 }'; }

gate_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{ print $1 }'
  else
    shasum -a 256 "$1" | awk '{ print $1 }'
  fi
}

gate_slug() { printf '%s' "$1" | tr -c 'A-Za-z0-9._-' '-' | cut -c1-80; }

# gate_begin BUNDLE_ROOT VERSION COMMIT PATH_NAME TIER
# Starts a fresh record of PATH_NAME in BUNDLE_ROOT/VERSION/COMMIT/.
gate_begin() {
  GATE_VERSION=$2
  GATE_COMMIT=$3
  GATE_PATH_NAME=$4
  GATE_TIER=$5
  GATE_BUNDLE="$1/$GATE_VERSION/$GATE_COMMIT"
  GATE_ATTACH="$GATE_BUNDLE/$GATE_PATH_NAME"
  rm -rf -- "$GATE_ATTACH" "$GATE_BUNDLE/$GATE_PATH_NAME.json"
  mkdir -p "$GATE_ATTACH/logs"
  GATE_TMP=$(mktemp -d "${TMPDIR:-/tmp}/axo-gate-record.XXXXXX")
  GATE_STEPS="$GATE_TMP/steps.jsonl"
  : >"$GATE_STEPS"
  GATE_STARTED=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  GATE_FAILED=0
  export GATE_VERSION GATE_COMMIT GATE_PATH_NAME GATE_TIER GATE_BUNDLE GATE_ATTACH GATE_TMP GATE_STEPS
}

# gate_record NAME STATUS EXIT SECONDS BYTES DETAIL [optional]
gate_record() {
  local args=(add-step --file "$GATE_STEPS" --name "$1" --status "$2")
  [ -n "$3" ] && args+=(--exit "$3")
  [ -n "$4" ] && args+=(--seconds "$4")
  [ -n "$5" ] && args+=(--bytes "$5")
  [ -n "$6" ] && args+=(--detail "$6")
  [ "${7:-}" = optional ] && args+=(--optional)
  node "$gate_evidence_js" "${args[@]}"
  if [ "$2" != pass ] && [ "${7:-}" != optional ]; then
    GATE_FAILED=$((GATE_FAILED + 1))
  fi
  case "$2" in
    pass) gate_say "PASS  $1${4:+ (${4}s)}" ;;
    skip) gate_say "SKIP  $1${6:+: $6}" ;;
    *) gate_say "FAIL  $1${6:+: $6}" ;;
  esac
}

gate_skip() { gate_record "$1" skip '' '' '' "$2" "${3:-}"; }

# gate_check NAME FAIL_DETAIL [optional] -- TEST...: record NAME as pass when
# TEST succeeds, else as fail with FAIL_DETAIL.
gate_check() {
  local name=$1 detail=$2 optional=''
  shift 2
  if [ "${1:-}" = optional ]; then
    optional=optional
    shift
  fi
  [ "${1:-}" = -- ] && shift
  if "$@" >/dev/null 2>&1; then
    gate_record "$name" pass '' '' '' '' "$optional"
  else
    gate_record "$name" fail '' '' '' "$detail" "$optional"
  fi
}

# gate_step NAME [--optional] [--expect N] [--interactive] -- COMMAND...
# Runs COMMAND, keeps its output in <path>/logs/<name>.log (an interactive
# step talks to the terminal and is kept by the transcript instead), and
# records pass when it exits with the expected status (default 0).
gate_step() {
  local name=$1 optional='' expect=0 interactive=0
  shift
  while [ $# -gt 0 ]; do
    case "$1" in
      --optional) optional=optional ;;
      --expect) expect=$2; shift ;;
      --interactive) interactive=1 ;;
      --) shift; break ;;
      *) gate_die "gate_step: unknown option $1" ;;
    esac
    shift
  done
  local log
  log="$GATE_ATTACH/logs/$(gate_slug "$name").log"
  local t0 b0 status
  t0=$(gate_now_ms)
  b0=$(gate_rx_bytes)
  gate_say "RUN   $name"
  if [ "$interactive" = 1 ]; then
    "$@"
    status=$?
    printf 'interactive step; see the transcript (exit %s)\n' "$status" >"$log"
  else
    "$@" </dev/null 2>&1 | tee "$log" >&2
    status=${PIPESTATUS[0]}
  fi
  local seconds bytes detail
  seconds=$(node -e 'process.stdout.write(((Date.now() - Number(process.argv[1])) / 1000).toFixed(1))' "$t0")
  bytes=$(($(gate_rx_bytes) - b0))
  if [ "$status" = "$expect" ]; then
    gate_record "$name" pass "$status" "$seconds" "$bytes" '' "$optional"
    return 0
  fi
  detail="exit $status, expected $expect: $(grep -v '^[[:space:]]*$' "$log" | tail -n 3 | tr '\n' ' ' | cut -c1-600)"
  gate_record "$name" fail "$status" "$seconds" "$bytes" "$detail" "$optional"
  return 1
}

# gate_doctor_json NAME EXPECT FILE: run `axocoatl doctor --json` as step
# NAME, expecting exit EXPECT, with its standard output (the document) in
# FILE and its standard error in the step's log.
gate_doctor_json() {
  gate_step "$1" --expect "$2" -- sh -c 'axocoatl doctor --json >"$1"' gate-doctor "$3"
}

# gate_finish [--json KEY=FILE]... : write <bundle>/<path>.json; its exit is
# 0 when the path passed.
gate_finish() {
  local attachments=() file status
  while IFS= read -r file; do
    attachments+=(--attach "${file#"$GATE_BUNDLE"/}")
  done < <(find "$GATE_ATTACH" -type f | LC_ALL=C sort)
  node "$gate_evidence_js" path-file --steps "$GATE_STEPS" \
    --out "$GATE_BUNDLE/$GATE_PATH_NAME.json" --path "$GATE_PATH_NAME" --tier "$GATE_TIER" \
    --version "$GATE_VERSION" --commit "$GATE_COMMIT" --started "$GATE_STARTED" \
    "$@" "${attachments[@]}"
  status=$?
  gate_say "evidence: $GATE_BUNDLE/$GATE_PATH_NAME.json"
  rm -rf -- "$GATE_TMP"
  return "$status"
}

# PATH as a brand-new user has it: no ~/.local/bin, and no directory that
# already holds an axocoatl.
gate_new_user_path() {
  local out='' dir old_ifs=$IFS
  IFS=:
  for dir in $PATH; do
    case "$dir" in
      '' | . | */.local/bin | */.local/bin/) continue ;;
    esac
    [ -e "$dir/axocoatl" ] && continue
    out="${out:+$out:}$dir"
  done
  IFS=$old_ifs
  printf '%s\n' "$out"
}

# gate_path_without PROGRAM MIRROR_DIR: PATH with every directory that
# holds PROGRAM replaced by a mirror of its other entries.
gate_path_without() {
  local program=$1 mirrors=$2 out='' dir entry n=0 old_ifs=$IFS
  mkdir -p "$mirrors"
  IFS=:
  for dir in $PATH; do
    [ -n "$dir" ] || continue
    if [ -e "$dir/$program" ]; then
      n=$((n + 1))
      mkdir -p "$mirrors/$n"
      for entry in "$dir"/* "$dir"/.[!.]*; do
        [ -e "$entry" ] || [ -L "$entry" ] || continue
        [ "${entry##*/}" = "$program" ] && continue
        ln -s "$entry" "$mirrors/$n/${entry##*/}" 2>/dev/null || true
      done
      out="${out:+$out:}$mirrors/$n"
    else
      out="${out:+$out:}$dir"
    fi
  done
  IFS=$old_ifs
  printf '%s\n' "$out"
}

# gate_guard_shims DIR LOG: sudo and package managers that refuse and log
# their argv, so a step that must not escalate or install is caught doing so.
gate_guard_shims() {
  local dir=$1 log=$2 tool
  mkdir -p "$dir"
  : >"$log"
  for tool in sudo apt-get apt dnf yum zypper pacman brew; do
    cat >"$dir/$tool" <<EOF
#!/bin/sh
printf '%s %s\n' "$tool" "\$*" >>"$log"
echo "gate guard: $tool is not allowed in this step" >&2
exit 1
EOF
    chmod 0755 "$dir/$tool"
  done
}

# gate_curl_shim DIR RELEASE_DIR SITE_DIR LOG: a curl that serves the
# installer's GitHub requests and axocoatl.ai/install.sh from local files
# through the real curl (file:// URLs), and refuses any other http(s) URL.
gate_curl_shim() {
  local dir=$1 release=$2 site=$3 log=$4 real
  real=$(command -v curl) || gate_die "curl is required"
  mkdir -p "$dir"
  : >"$log"
  cat >"$dir/curl" <<EOF
#!/bin/sh
for arg do
  shift
  case "\$arg" in
    https://api.github.com/repos/axocoatl/axocoatl/releases/latest)
      arg="file://$release/latest.json" ;;
    https://github.com/axocoatl/axocoatl/releases/download/*/*)
      arg="file://$release/\${arg##*/}" ;;
    https://axocoatl.ai/install.sh)
      arg="file://$site/install.sh" ;;
    http://* | https://*)
      printf 'REFUSED %s\n' "\$arg" >>"$log"
      echo "gate curl shim: refusing \$arg" >&2
      exit 22 ;;
  esac
  set -- "\$@" "\$arg"
done
printf '%s\n' "\$*" >>"$log"
exec "$real" "\$@"
EOF
  chmod 0755 "$dir/curl"
}

# gate_stage_release TARBALL DIR: lay out a GitHub-release-like directory
# (the tarball, its .sha256 and latest.json) for the curl shim; prints the tag.
gate_stage_release() {
  local tarball=$1 dir=$2 name tag
  name=${tarball##*/}
  case "$name" in
    axocoatl-v*-*.tar.gz) ;;
    *) gate_die "not a release tarball name: $name" ;;
  esac
  [ -f "$tarball.sha256" ] || gate_die "missing $tarball.sha256"
  tag=${name#axocoatl-}
  tag=${tag%%-*}
  mkdir -p "$dir"
  cp "$tarball" "$tarball.sha256" "$dir/"
  printf '{"tag_name":"%s"}\n' "$tag" >"$dir/latest.json"
  printf '%s\n' "$tag"
}

# gate_install_new_user STAGE_DIR SITE_DIR: run the published installer as
# `curl -fsSL https://axocoatl.ai/install.sh | sh` does, against the staged
# release, then check where the binary went and whether the user's next
# shell finds it. Sets GATE_INSTALLED (the binary) and puts its directory on
# PATH for later steps the way the installer told the user to.
gate_install_new_user() {
  local stage=$1 site=$2 shim="$GATE_TMP/curl-shim" out="$GATE_ATTACH/logs/install.log"
  # Never replace another install: where /usr/local/bin is writable and
  # already holds an axocoatl, the installer would overwrite it.
  if grep -q '/usr/local/bin' "$site/install.sh" && [ -w /usr/local/bin ] && [ -e /usr/local/bin/axocoatl ]; then
    gate_record "install with the published command" fail '' '' '' \
      "the installer would replace /usr/local/bin/axocoatl, another install on this machine; run the gate where there is none"
    return 1
  fi
  gate_curl_shim "$shim" "$stage" "$site" "$GATE_TMP/curl.log"
  local t0 b0 status seconds
  t0=$(gate_now_ms)
  b0=$(gate_rx_bytes)
  env PATH="$shim:$PATH" sh -c 'curl -fsSL https://axocoatl.ai/install.sh | sh' >"$out" 2>&1
  status=$?
  cat "$out" >&2
  cp "$GATE_TMP/curl.log" "$GATE_ATTACH/logs/install-curl-urls.log"
  seconds=$(node -e 'process.stdout.write(((Date.now() - Number(process.argv[1])) / 1000).toFixed(1))' "$t0")
  if [ "$status" != 0 ]; then
    gate_record "install with the published command" fail "$status" "$seconds" "$(($(gate_rx_bytes) - b0))" "$(tail -n 2 "$out" | tr '\n' ' ')"
    return 1
  fi
  gate_record "install with the published command" pass 0 "$seconds" "$(($(gate_rx_bytes) - b0))" ''
  if grep -q '^REFUSED' "$GATE_TMP/curl.log"; then
    gate_record "installer fetched only the release" fail '' '' '' "$(grep '^REFUSED' "$GATE_TMP/curl.log" | head -n 3 | tr '\n' ' ')"
  fi
  GATE_INSTALLED=$(sed -n 's/^axocoatl-install: installed axocoatl [^ ]* -> \(.*\)$/\1/p' "$out" | tail -n 1)
  if [ -z "$GATE_INSTALLED" ] || [ ! -x "$GATE_INSTALLED" ]; then
    gate_record "installed binary is where the installer said" fail '' '' '' "no 'installed axocoatl ... -> PATH' line, or no executable there"
    return 1
  fi
  gate_record "installed binary is where the installer said" pass '' '' '' "$GATE_INSTALLED"
  local dest=${GATE_INSTALLED%/*} shell_found='' login_shell=/bin/bash shell_flags=-lc
  if [ "$(uname -s)" = Darwin ]; then
    login_shell=/bin/zsh
    shell_flags=-ilc
  fi
  shell_found=$(env -i HOME="$HOME" USER="${USER:-}" TERM=dumb PATH="$GATE_BASE_PATH" "$login_shell" "$shell_flags" 'command -v axocoatl' 2>/dev/null </dev/null | tail -n 1)
  if [ "$shell_found" = "$GATE_INSTALLED" ]; then
    gate_record "a new login shell finds axocoatl" pass '' '' '' "$login_shell -l resolves $shell_found"
  elif grep -q "export PATH=\"$dest:" "$out"; then
    gate_record "a new login shell finds axocoatl" pass '' '' '' "not on a new shell's PATH; the installer printed the export line for $dest"
  else
    gate_record "a new login shell finds axocoatl" fail '' '' '' "a new $login_shell login shell does not find axocoatl, and the installer printed no PATH line for $dest"
  fi
  case ":$PATH:" in
    *":$dest:"*) ;;
    *) PATH="$dest:$PATH" ;;
  esac
  export PATH GATE_INSTALLED
  return 0
}

# gate_host_json FILE: what this host is, for the evidence.
gate_host_json() {
  local os_release='' kernel arch os
  os=$(uname -s)
  kernel=$(uname -r)
  arch=$(uname -m)
  if [ -r /etc/os-release ]; then
    # shellcheck disable=SC1091
    os_release=$(. /etc/os-release && printf '%s %s' "${ID:-}" "${VERSION_ID:-}")
  elif command -v sw_vers >/dev/null 2>&1; then
    os_release="macOS $(sw_vers -productVersion 2>/dev/null)"
  fi
  node -e '
    const [file, os, release, kernel, arch, runner, memory, cpus] = process.argv.slice(1);
    require("fs").writeFileSync(file, JSON.stringify({ os, release, kernel, arch, runner: runner || null,
      memory_bytes: Number(memory) || require("os").totalmem(), cpus: Number(cpus) || require("os").cpus().length,
      user: require("os").userInfo().username }, null, 2));
  ' "$1" "$os" "$os_release" "$kernel" "$arch" "${GATE_RUNNER:-}" "" ""
}

# gate_tools_json FILE: the versions of the tools the path ran with.
gate_tools_json() {
  local file=$1 podman_version='' git_version node_version
  podman_version=$(podman version --format '{{.Client.Version}}' 2>/dev/null || true)
  git_version=$(git --version 2>/dev/null | awk '{ print $3 }')
  node_version=$(node --version 2>/dev/null)
  node -e '
    const [file, podman, git, node, axocoatl, ollama] = process.argv.slice(1);
    require("fs").writeFileSync(file, JSON.stringify({ podman: podman || null, git, node,
      axocoatl: axocoatl || null, ollama: ollama || null }, null, 2));
  ' "$file" "$podman_version" "$git_version" "$node_version" "${GATE_AXOCOATL_VERSION:-}" "${GATE_OLLAMA_VERSION:-}"
}

# gate_fixture_repo FIXTURE DIR: a new user's repository: the fixture's
# files, git-initialised and committed.
gate_fixture_repo() {
  local fixture=$1 dir=$2
  [ -d "$gate_repo/tests/fixtures/newuser/$fixture" ] || return 1
  rm -rf -- "$dir"
  mkdir -p "$(dirname -- "$dir")"
  cp -R "$gate_repo/tests/fixtures/newuser/$fixture" "$dir"
  git -C "$dir" init -q -b main
  git -C "$dir" -c user.name='New User' -c user.email='new-user@example.invalid' add -A
  git -C "$dir" -c user.name='New User' -c user.email='new-user@example.invalid' commit -qm "Start $fixture"
}

# gate_fixture_expect FIXTURE KEY: a field of tests/fixtures/newuser/fixtures.json.
gate_fixture_expect() {
  node -e '
    const m = require(process.argv[1]);
    const value = m.fixtures[process.argv[2]]?.[process.argv[3]];
    process.stdout.write(value == null ? "" : String(value));
  ' "$gate_repo/tests/fixtures/newuser/fixtures.json" "$1" "$2"
}

gate_fixture_names() {
  node -e 'for (const name of Object.keys(require(process.argv[1]).fixtures)) console.log(name)' \
    "$gate_repo/tests/fixtures/newuser/fixtures.json"
}

# gate_with_outputs COMMAND: COMMAND plus --junit/--record/--receipt where
# the published command lacks them (a run's outputs, kept as evidence).
gate_with_outputs() {
  local command=$1
  case " $command " in *" --junit "*) ;; *) command="$command --junit axocoatl-junit.xml" ;; esac
  case " $command " in *" --record "*) ;; *) command="$command --record run.axorecord.jsonl" ;; esac
  case " $command " in *" --receipt "*) ;; *) command="$command --receipt receipt.json" ;; esac
  printf '%s\n' "$command"
}

# gate_flag_value COMMAND FLAG: the word after FLAG in COMMAND.
gate_flag_value() {
  printf '%s\n' "$1" | awk -v flag="$2" '{ for (i = 1; i < NF; i++) if ($i == flag) { print $(i + 1); exit } }'
}

# gate_run_fixture FIXTURE COMMAND REPO_DIR [WRITER_LABEL]: run COMMAND (as
# published, plus outputs) in a fresh copy of FIXTURE, then verify its
# receipt with the documented command and its record. Records each check.
gate_run_fixture() {
  local fixture=$1 published=$2 repo=$3 label=${4:-$1} command record receipt junit keep
  keep="$GATE_ATTACH/$label"
  if ! gate_fixture_repo "$fixture" "$repo"; then
    gate_record "fixture $fixture exists" fail '' '' '' "tests/fixtures/newuser/$fixture is missing"
    return 1
  fi
  command=$(gate_with_outputs "$published")
  record=$(gate_flag_value "$command" --record)
  receipt=$(gate_flag_value "$command" --receipt)
  junit=$(gate_flag_value "$command" --junit)
  mkdir -p "$keep"
  printf '%s\n' "$command" >"$keep/command.txt"
  local status=0
  gate_step "run fix on $label" -- /bin/bash -c "cd \"\$1\" && $command" gate-run "$repo" || status=1
  cp "$GATE_ATTACH/logs/$(gate_slug "run fix on $label").log" "$keep/run.log" 2>/dev/null || true
  for file in "$receipt" "$record" "$junit"; do
    [ -n "$file" ] && [ -f "$repo/$file" ] && cp "$repo/$file" "$keep/"
  done
  (cd "$repo" && git status --porcelain && git diff) >"$keep/changes.txt" 2>&1 || true
  # The run says which image and setup it chose (fixtures.json).
  local image setup line
  image=$(gate_fixture_expect "$fixture" image_contains)
  setup=$(gate_fixture_expect "$fixture" setup_contains)
  line=$(grep -Eo 'image: [^ ]+' "$keep/run.log" 2>/dev/null | head -n 1)
  if [ -n "$line" ] && printf '%s' "${line#image: }" | grep -qi -- "$image"; then
    gate_record "run on $label chose a $image image" pass '' '' '' "$line"
  else
    gate_record "run on $label chose a $image image" fail '' '' '' "expected an 'image: …' line naming $image; found '${line:-none}'"
  fi
  if [ -n "$setup" ]; then
    if grep -q -- "$setup" "$keep/run.log" 2>/dev/null || { [ -f "$keep/${record##*/}" ] && grep -q -- "$setup" "$keep/${record##*/}"; }; then
      gate_record "run on $label ran setup ($setup)" pass '' '' '' ''
    else
      gate_record "run on $label ran setup ($setup)" fail '' '' '' "neither the run's output nor its record names $setup"
    fi
  fi
  # The documented verify commands, run in the repository.
  gate_step "receipt verify on $label (documented command)" -- /bin/bash -c \
    "cd \"\$1\" && axocoatl receipt verify \"\$2\" --key \"\$(axocoatl receipt key)\" --repo ." \
    gate-verify "$repo" "$receipt" || status=1
  gate_step "record verify on $label" -- /bin/bash -c 'cd "$1" && axocoatl record verify "$2"' \
    gate-verify "$repo" "$record" || status=1
  gate_step "receipt matches the record on $label" -- /bin/bash -c \
    "cd \"\$1\" && axocoatl receipt verify \"\$2\" --key \"\$(axocoatl receipt key)\" --record \"\$3\"" \
    gate-verify "$repo" "$receipt" "$record" || status=1
  return "$status"
}
