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

# gate_curl ARGS...: curl for the gate's own requests to a daemon, a model
# server or a stub: at most 2 s to connect and GATE_CURL_MAX_TIME seconds
# (default 5) in all, so one that accepts the connection and never answers
# fails the step instead of hanging the gate.
gate_curl() { curl --connect-timeout 2 --max-time "${GATE_CURL_MAX_TIME:-5}" "$@"; }

# gate_daemon_up PORT: something answers /health/live on PORT.
gate_daemon_up() { gate_curl -fsS "http://127.0.0.1:$1/health/live" >/dev/null 2>&1; }

# gate_daemon_state PORT CONFIG: what answers /health on PORT, decided as
# onboarding decides it (axocoatl-cli/src/onboard/smoke.rs, started_at):
#   current  an Axocoatl daemon (its /health names its version) that started
#            after CONFIG last changed
#   stale    an Axocoatl daemon that started before CONFIG last changed, or
#            that does not say when it started (before 1.5.0)
#   none     nothing listens on PORT
#   other    something answers that is not an Axocoatl /health
#   unknown  the request failed otherwise (a timeout, an HTTP error, an
#            empty reply): the gate cannot tell, and never takes that for a
#            running daemon
gate_daemon_state() {
  local body status
  body=$(gate_curl -fsS "http://127.0.0.1:$1/health" 2>/dev/null)
  status=$?
  case "$status" in
    0) ;;
    7) echo none; return 0 ;;
    *) echo unknown; return 0 ;;
  esac
  printf '%s' "$body" | node -e '
    let text = "";
    process.stdin.on("data", (d) => (text += d)).on("end", () => {
      let health;
      try { health = JSON.parse(text); } catch { health = null; }
      if (!health || typeof health !== "object" || typeof health.version !== "string") return console.log("other");
      const started = health.started_at_ms;
      if (typeof started !== "number") return console.log("stale");
      let changed = 0;
      try { changed = Math.floor(require("fs").statSync(process.argv[1]).mtimeMs); } catch {}
      console.log(changed > started ? "stale" : "current");
    });' "$2"
}

# gate_daemon_state_says STATE PORT: STATE in words, for a step's detail.
gate_daemon_state_says() {
  case "$1" in
    current) echo "an Axocoatl daemon on port $2 runs the current configuration" ;;
    stale) echo "the Axocoatl daemon on port $2 started before the configuration last changed (or does not say when it started)" ;;
    none) echo "nothing answers on port $2" ;;
    other) echo "something answers on port $2 that is not an Axocoatl daemon (its /health names no version)" ;;
    *) echo "the request to port $2/health failed or timed out, so the gate cannot tell what runs there" ;;
  esac
}

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

# gate_valid_name NAME: a gate path name (A-x86, C1-local, D1): letters,
# digits and dashes, so it can never climb out of its bundle.
gate_valid_name() { [[ "$1" =~ ^[A-Za-z0-9][A-Za-z0-9-]{0,63}$ ]]; }
gate_valid_version() { [[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; }
gate_valid_commit() { [[ "$1" =~ ^[0-9a-f]{40}$ ]]; }

# gate_begin BUNDLE_ROOT VERSION COMMIT PATH_NAME TIER
# Starts a fresh record of PATH_NAME in BUNDLE_ROOT/VERSION/COMMIT/. The
# names are checked first: the previous record of the path is removed.
gate_begin() {
  gate_valid_version "$2" || gate_die "invalid version '$2' (X.Y.Z)"
  gate_valid_commit "$3" || gate_die "invalid commit '$3' (40 lowercase hex characters)"
  gate_valid_name "$4" || gate_die "invalid path name '$4' (letters, digits and dashes)"
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

# gate_ask QUESTION: the operator's answer on the terminal, y or n (and for
# n, what they saw instead), in ANSWER and ANSWER_NOTE. The question and the
# answer are printed to standard output (the script(1) transcript) and
# appended to the path's answers.txt, with the time and who answered. A
# simulated run (GATE_SIMULATION=1, never evidence) takes
# GATE_SIMULATED_ANSWER instead of a terminal.
gate_ask() {
  local answer='' note=''
  if [ "${GATE_SIMULATION:-0}" = 1 ] && [ -n "${GATE_SIMULATED_ANSWER:-}" ]; then
    answer=$GATE_SIMULATED_ANSWER
    [ "$answer" = y ] || note='simulated no'
    printf '\n>>> %s [y/n] (simulated)\n' "$1"
  else
    printf '\n>>> %s [y/n] ' "$1" >/dev/tty
    read -r answer </dev/tty
  fi
  case "$answer" in y | Y | yes) answer=y ;; *) answer=n ;; esac
  if [ "$answer" = n ] && [ -z "$note" ]; then
    printf '>>> What did you see instead? ' >/dev/tty
    read -r note </dev/tty
  fi
  printf '>>> answered %s%s\n' "$answer" "${note:+: $note}"
  printf '%s\t%s\t%s\t%s\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$(id -un)" "$1" "$answer" "$note" >>"$GATE_ATTACH/answers.txt"
  ANSWER=$answer
  ANSWER_NOTE=$note
}

# gate_confirm NAME QUESTION: record NAME as the operator answers QUESTION.
gate_confirm() {
  gate_ask "$2"
  if [ "$ANSWER" = y ]; then
    gate_record "$1" pass '' '' '' "the operator ($(id -un)) answered yes at the terminal: $2"
  else
    gate_record "$1" fail '' '' '' "the operator ($(id -un)) answered no: ${ANSWER_NOTE:-no note}"
  fi
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

# The default published install command, for scripts given no other.
GATE_DEFAULT_INSTALL='curl -fsSL https://axocoatl.ai/install.sh | sh'

# gate_new_shells: the shells a new user's next terminal starts, one
# "SHELL FLAGS" per line, for the user's login shell ($SHELL: bash, zsh or
# fish; any other counts as the platform's default).
# macOS Terminal starts an interactive login shell. On Linux a new login
# (SSH, a new desktop session) starts a login shell and a new terminal
# window an interactive one, so both must find axocoatl.
gate_new_shells() {
  local shell=${SHELL:-}
  case "${shell##*/}" in
    bash | zsh | fish) [ -x "$shell" ] || shell='' ;;
    *) shell='' ;;
  esac
  if [ "$(uname -s)" = Darwin ]; then
    printf '%s -ilc\n' "${shell:-/bin/zsh}"
  else
    shell=${shell:-/bin/bash}
    printf '%s -lc\n%s -ic\n' "$shell" "$shell"
  fi
}

# gate_new_shells_find BINARY: whether every new shell (gate_new_shells),
# started with a clean environment and the new user's base PATH, resolves
# axocoatl to BINARY. Prints what each one found.
gate_new_shells_find() {
  local binary=$1 shell flags found status=0 report=''
  while read -r shell flags; do
    found=$(env -i HOME="$HOME" USER="${USER:-}" LOGNAME="${USER:-}" SHELL="$shell" TERM=dumb \
      PATH="$GATE_BASE_PATH" "$shell" "$flags" 'command -v axocoatl' 2>/dev/null </dev/null | tail -n 1)
    report="$report${report:+; }${shell##*/} $flags: ${found:-not found}"
    [ "$found" = "$binary" ] || status=1
  done < <(gate_new_shells)
  printf '%s\n' "$report"
  return "$status"
}

# gate_printed_profile_line LOG: the shell startup files and the line the
# installer asked the user to add to them ("add this line to FILE:" or
# "add this line to FILE and FILE:", then the line), one FILE<TAB>LINE per
# file with a leading ~/ expanded; nothing when it asked for none. A bare
# `export PATH=...` for the current shell is not one.
gate_printed_profile_line() {
  awk -v home="$HOME" '
    found && NF {
      sub(/^[ \t]+/, "")
      n = split(files, list, " and ")
      for (i = 1; i <= n; i++) {
        file = list[i]
        if (substr(file, 1, 2) == "~/") file = home substr(file, 2)
        print file "\t" $0
      }
      exit
    }
    /add this line to .*:[ \t]*$/ {
      files = $0
      sub(/.*add this line to /, "", files)
      sub(/:[ \t]*$/, "", files)
      found = 1
    }' "$1"
}

# gate_install_new_user STAGE_DIR SITE_DIR [COMMAND]: run the published
# install COMMAND (default GATE_DEFAULT_INSTALL) exactly as written, against
# the staged release, then check where the binary went and that the user's
# next shell finds it. When the installer could not change a profile itself
# (no terminal to ask on) and printed the line to add, the gate adds that
# line as the user is told to and checks again; a transient `export` hint
# alone fails. Sets GATE_INSTALLED (the binary) and puts its directory on
# PATH for later steps the way the installer told the user to.
gate_install_new_user() {
  local stage=$1 site=$2 command=${3:-$GATE_DEFAULT_INSTALL}
  local shim="$GATE_TMP/curl-shim" out="$GATE_ATTACH/logs/install.log"
  # Never replace another install: where /usr/local/bin is writable and
  # already holds an axocoatl, the installer would overwrite it.
  if grep -q '/usr/local/bin' "$site/install.sh" && [ -w /usr/local/bin ] && [ -e /usr/local/bin/axocoatl ]; then
    gate_record "install with the published command" fail '' '' '' \
      "the installer would replace /usr/local/bin/axocoatl, another install on this machine; run the gate where there is none"
    return 1
  fi
  gate_curl_shim "$shim" "$stage" "$site" "$GATE_TMP/curl.log"
  printf '%s\n' "$command" >"$GATE_ATTACH/install-command.txt"
  local t0 b0 status seconds
  t0=$(gate_now_ms)
  b0=$(gate_rx_bytes)
  # A new user's shell: no CI PATH file to write, no installer overrides.
  env -u GITHUB_PATH -u AXOCOATL_MODIFY_PATH -u AXOCOATL_VERSION -u AXOCOATL_INSTALL_BASE_URL \
    -u AXOCOATL_INSTALL_SYSTEM_DIR PATH="$shim:$PATH" sh -c "$command" >"$out" 2>&1
  status=$?
  cat "$out" >&2
  cp "$GATE_TMP/curl.log" "$GATE_ATTACH/logs/install-curl-urls.log"
  seconds=$(node -e 'process.stdout.write(((Date.now() - Number(process.argv[1])) / 1000).toFixed(1))' "$t0")
  if [ "$status" != 0 ]; then
    gate_record "install with the published command" fail "$status" "$seconds" "$(($(gate_rx_bytes) - b0))" "$command: $(tail -n 2 "$out" | tr '\n' ' ')"
    return 1
  fi
  gate_record "install with the published command" pass 0 "$seconds" "$(($(gate_rx_bytes) - b0))" "$command"
  if grep -q '^REFUSED' "$GATE_TMP/curl.log"; then
    gate_record "installer fetched only the release" fail '' '' '' "$(grep '^REFUSED' "$GATE_TMP/curl.log" | head -n 3 | tr '\n' ' ')"
  fi
  GATE_INSTALLED=$(sed -n 's/^axocoatl-install: installed axocoatl [^ ]* -> \(.*\)$/\1/p' "$out" | tail -n 1)
  if [ -z "$GATE_INSTALLED" ] || [ ! -x "$GATE_INSTALLED" ]; then
    gate_record "installed binary is where the installer said" fail '' '' '' "no 'installed axocoatl ... -> PATH' line, or no executable there"
    return 1
  fi
  gate_record "installed binary is where the installer said" pass '' '' '' "$GATE_INSTALLED"
  local dest=${GATE_INSTALLED%/*} report printed profile line added='' profiles=''
  if report=$(gate_new_shells_find "$GATE_INSTALLED"); then
    gate_record "a new shell finds axocoatl" pass '' '' '' "$report"
  else
    printed=$(gate_printed_profile_line "$out")
    if [ -n "$printed" ]; then
      # The read that ends the loop empties line, so the line is kept apart.
      while IFS=$'\t' read -r profile line; do
        mkdir -p "$(dirname -- "$profile")" && printf '\n%s\n' "$line" >>"$profile"
        profiles="$profiles${profiles:+ and }$profile"
        added=$line
      done <<<"$printed"
      if report=$(gate_new_shells_find "$GATE_INSTALLED"); then
        gate_record "a new shell finds axocoatl" pass '' '' '' "after adding the line the installer printed to $profiles ($added): $report"
      else
        gate_record "a new shell finds axocoatl" fail '' '' '' "the line the installer printed for $profiles ($added) does not put axocoatl on a new shell's PATH: $report"
      fi
    elif grep -q 'export PATH=' "$out"; then
      gate_record "a new shell finds axocoatl" fail '' '' '' "$report; the installer printed only an export for the current shell, which a new terminal does not keep"
    else
      gate_record "a new shell finds axocoatl" fail '' '' '' "$report; the installer printed no line that puts $dest on PATH"
    fi
  fi
  case ":$PATH:" in
    *":$dest:"*) ;;
    *) PATH="$dest:$PATH" ;;
  esac
  export PATH GATE_INSTALLED
  return 0
}

# gate_config_path: where Axocoatl keeps this user's config.yaml (as
# axocoatl-cli/src/user_paths.rs says).
gate_config_path() {
  if [ "$(uname -s)" = Darwin ]; then
    printf '%s\n' "$HOME/Library/Application Support/Axocoatl/config.yaml"
  else
    printf '%s\n' "${XDG_CONFIG_HOME:-$HOME/.config}/axocoatl/config.yaml"
  fi
}

# gate_data_dir: where Axocoatl keeps this user's data (as
# axocoatl-cli/src/user_paths.rs says): its stored secrets, its local API
# token and its Sessions.
gate_data_dir() {
  if [ "$(uname -s)" = Darwin ]; then
    printf '%s\n' "$HOME/Library/Application Support/Axocoatl/data"
  else
    printf '%s\n' "${XDG_DATA_HOME:-$HOME/.local/share}/axocoatl"
  fi
}

# gate_log_dir: where onboarding writes its log, onboard-<time>.log
# (axocoatl-cli/src/onboard/host.rs, log_directory).
gate_log_dir() {
  if [ "$(uname -s)" = Darwin ]; then
    printf '%s\n' "$HOME/Library/Logs/Axocoatl"
  else
    printf '%s\n' "${XDG_STATE_HOME:-$HOME/.local/state}/axocoatl"
  fi
}

# gate_keep_onboarding_logs MARK: copy each onboarding log written since
# the file MARK was touched into <path>/logs/onboarding/, so a failed step
# (a browser sign-in that never called back, a sudo that failed) can be
# diagnosed from the evidence. The log holds every command onboarding ran
# and never a key.
gate_keep_onboarding_logs() {
  local dir log
  dir=$(gate_log_dir)
  [ -d "$dir" ] && [ -e "$1" ] || return 0
  while IFS= read -r log; do
    mkdir -p "$GATE_ATTACH/logs/onboarding"
    cp "$log" "$GATE_ATTACH/logs/onboarding/" 2>/dev/null
  done < <(find "$dir" -maxdepth 1 -name 'onboard-*.log' -newer "$1" 2>/dev/null | LC_ALL=C sort)
}

# gate_api_get PATH OUT: GET PATH from the daemon on GATE_DAEMON_PORT with
# the local API token (<data root>/local-api-token, as scripts send it),
# into OUT. The token goes to curl on standard input, so it is on no
# command line and in no log. Returns curl's status (0 on success).
gate_api_get() {
  local token
  token="$(gate_data_dir)/local-api-token"
  [ -r "$token" ] && [ -n "${GATE_DAEMON_PORT:-}" ] || { printf 'no local API token at %s, or no daemon port\n' "$token" >"$2"; return 2; }
  printf 'header = "Authorization: Bearer %s"\n' "$(cat "$token")" \
    | GATE_CURL_MAX_TIME=30 gate_curl -fsS --config - "http://127.0.0.1:$GATE_DAEMON_PORT$1" >"$2" 2>&1
}

# gate_secret_stored NAME: whether Axocoatl's secret store holds NAME, by
# `axocoatl secret list` (the names only, one per line): 0 it does, 1 it
# does not, 2 the list cannot be read (no axocoatl, an unreadable data
# directory or configuration), with why in GATE_SECRET_ERROR. A list that
# cannot be read is never taken for one without NAME.
gate_secret_stored() {
  local list status
  GATE_SECRET_ERROR=''
  list=$(axocoatl secret list 2>"$GATE_TMP/secret-list.err")
  status=$?
  if [ "$status" != 0 ]; then
    # shellcheck disable=SC2034 # read by the scripts that source this file
    GATE_SECRET_ERROR="axocoatl secret list exited $status: $(head -c 200 "$GATE_TMP/secret-list.err" | tr '\n' ' ')"
    return 2
  fi
  printf '%s\n' "$list" | grep -qxF -- "$1"
}

# gate_onboarding_log_lines MARK TEXT: the first five lines holding TEXT in
# the onboarding logs written since the file MARK was touched, on one line
# (for a failed sign-in: each connection its callback received).
gate_onboarding_log_lines() {
  local dir
  dir=$(gate_log_dir)
  [ -d "$dir" ] && [ -e "$1" ] || return 0
  find "$dir" -maxdepth 1 -name 'onboard-*.log' -newer "$1" -print0 2>/dev/null \
    | xargs -0 grep -h -F -- "$2" /dev/null 2>/dev/null | head -n 5 | cut -c1-200 | tr '\n' ' '
}

# gate_server_port CONFIG: server.port in CONFIG, or nothing.
gate_server_port() {
  awk '/^server:/ { inside = 1; next } inside && /^[^[:space:]#]/ { inside = 0 }
       inside && /^[[:space:]]+port:/ { gsub(/"/, "", $2); print $2; exit }' "$1" 2>/dev/null
}

# gate_run_model CONFIG ROLE: run.models.ROLE in CONFIG, or nothing.
gate_run_model() {
  awk -v role="$2" '/^run:/ { inside = 1; next } inside && /^[^[:space:]#]/ { inside = 0 }
       inside && $1 == role ":" { gsub(/"/, "", $2); print $2; exit }' "$1" 2>/dev/null
}

# gate_journal_reader: how this user can read the system journal: "plain"
# (journalctl as this user), "sudo" (only on a CI runner with passwordless
# sudo, whose gate evidence relies on the journal), or nothing.
gate_journal_reader() {
  command -v journalctl >/dev/null 2>&1 || return 0
  # Root's entries show only when the system journal is readable.
  if [ -n "$(journalctl -q -n 1 _UID=0 -o cat 2>/dev/null)" ]; then
    echo plain
  elif [ "${GITHUB_ACTIONS:-}" = true ] && [ -n "$(sudo -n journalctl -q -n 1 _UID=0 -o cat 2>/dev/null)" ]; then
    echo sudo
  fi
}

# gate_sudo_mark: "READER CURSOR", a place in the system journal, so that
# gate_sudo_check can find every sudo call made after it, by any path (not
# only through the guard shims on PATH). Empty when this user cannot read
# the system journal.
gate_sudo_mark() {
  local reader cursor
  reader=$(gate_journal_reader)
  case "$reader" in
    plain) cursor=$(journalctl -q -n 1 --show-cursor -o cat 2>/dev/null | sed -n 's/^-- cursor: //p' | tail -n 1) ;;
    sudo) cursor=$(sudo -n journalctl -q -n 1 --show-cursor -o cat 2>/dev/null | sed -n 's/^-- cursor: //p' | tail -n 1) ;;
    *) return 0 ;;
  esac
  [ -n "$cursor" ] && printf '%s %s\n' "$reader" "$cursor"
}

# gate_sudo_check NAME MARK: record NAME as pass when the journal shows no
# sudo command after MARK (the gate's own sudo journalctl reads aside).
# Without a readable journal it is a failure on a CI runner (whose evidence
# relies on it) and an optional skip elsewhere.
gate_sudo_check() {
  local name=$1 mark=$2 calls reader cursor
  if [ -z "$mark" ]; then
    if [ "${GITHUB_ACTIONS:-}" = true ]; then
      gate_record "$name" fail '' '' '' "the system journal is not readable, so a sudo by absolute path would go unseen"
    else
      gate_skip "$name" "the system journal is not readable here" optional
    fi
    return 0
  fi
  reader=${mark%% *}
  cursor=${mark#* }
  if [ "$reader" = sudo ]; then
    calls=$(sudo -n journalctl -q --after-cursor "$cursor" -t sudo -o cat 2>/dev/null)
  else
    calls=$(journalctl -q --after-cursor "$cursor" -t sudo -o cat 2>/dev/null)
  fi
  calls=$(printf '%s\n' "$calls" | grep 'COMMAND=' | grep -v 'COMMAND=[^ ]*journalctl ' || true)
  if [ -z "$calls" ]; then
    gate_record "$name" pass '' '' '' 'the journal shows no sudo command'
  else
    gate_record "$name" fail '' '' '' "$(printf '%s\n' "$calls" | head -n 3 | tr '\n' ' ')"
  fi
}

# gate_doctor_text_check LOG [LOADOUT]: plain doctor's verdict as a new user
# reads it: "Ready for: ... axocoatl run LOADOUT" (default fix; a writer's own
# loadout such as fix-codex may carry its parameters), a last "Ready: yes",
# no [FAIL] line.
gate_doctor_text_check() {
  local log=$1 loadout=${2:-fix} problems=''
  grep -Eq "^Ready for: (.*, )?axocoatl run $loadout( [^,]*)?(,|\$)" "$log" || problems="no 'Ready for: axocoatl run $loadout'"
  [ "$(grep -v '^[[:space:]]*$' "$log" | tail -n 1)" = 'Ready: yes' ] || problems="${problems:+$problems; }the last line is not 'Ready: yes'"
  ! grep -q '\[FAIL\]' "$log" || problems="${problems:+$problems; }$(grep -m 1 '\[FAIL\]' "$log")"
  if [ -z "$problems" ]; then
    gate_record "doctor says Ready for: axocoatl run $loadout, Ready: yes, no [FAIL]" pass '' '' '' ''
  else
    gate_record "doctor says Ready for: axocoatl run $loadout, Ready: yes, no [FAIL]" fail '' '' '' "$problems"
  fi
}

# gate_service_unit: the Always-On Service's unit file
# (crates/axocoatl-service: launchd.rs, systemd.rs).
gate_service_unit() {
  if [ "$(uname -s)" = Darwin ]; then
    printf '%s\n' "$HOME/Library/LaunchAgents/ai.axocoatl.daemon.plist"
  else
    printf '%s\n' "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/axocoatl.service"
  fi
}

# gate_service_status NAME INSTALLED RUNNING: run `axocoatl service status`
# as its own step, and record NAME as pass when it says `installed:
# INSTALLED` and `running: RUNNING` (yes or no) and the unit file is there
# exactly when INSTALLED is yes.
gate_service_status() {
  local name=$1 installed=$2 running=$3 log problems='' unit
  gate_step "axocoatl service status ($name)" --optional -- axocoatl service status
  log="$GATE_ATTACH/logs/$(gate_slug "axocoatl service status ($name)").log"
  grep -Eq "^[[:space:]]*installed:[[:space:]]+$installed\$" "$log" || problems="it does not say installed: $installed"
  grep -Eq "^[[:space:]]*running:[[:space:]]+$running\$" "$log" || problems="${problems:+$problems; }it does not say running: $running"
  unit=$(gate_service_unit)
  if [ "$installed" = yes ] && [ ! -f "$unit" ]; then problems="${problems:+$problems; }no $unit"; fi
  if [ "$installed" = no ] && [ -e "$unit" ]; then problems="${problems:+$problems; }$unit is still there"; fi
  if [ -z "$problems" ]; then
    gate_record "$name" pass '' '' '' "installed: $installed, running: $running; $(sed -n 's/^[[:space:]]*detail:[[:space:]]*//p' "$log" | head -n 1 | cut -c1-200)"
  else
    gate_record "$name" fail '' '' '' "$problems: $(grep -v '^[[:space:]]*$' "$log" | head -n 4 | tr '\n' ' ' | cut -c1-300)"
  fi
}

# gate_wait_daemon PORT CONFIG SECONDS: wait up to SECONDS until the daemon
# on PORT runs CONFIG as it is now (gate_daemon_state says current); prints
# the last state.
gate_wait_daemon() {
  local state='' deadline=$((SECONDS + $3))
  while :; do
    state=$(gate_daemon_state "$1" "$2")
    [ "$state" = current ] && break
    [ "$SECONDS" -ge "$deadline" ] && break
    sleep 2
  done
  printf '%s\n' "$state"
}

# gate_service_restart_steps CONFIG PORT OUTPUT: after `onboard --update`
# changed CONFIG while the Always-On Service ran, record that onboarding's
# Step 7 found the service ran the old configuration and restarted it (its
# question is in OUTPUT, the step's log or the transcript since the step
# began) and that the service on PORT now runs CONFIG as it is.
gate_service_restart_steps() {
  local state
  state=$(gate_wait_daemon "$2" "$1" 300)
  if grep -q 'The Always-On Service started before' "$3" 2>/dev/null && [ "$state" = current ]; then
    gate_record "onboarding restarted the service after the configuration changed (Step 7)" pass '' '' '' \
      "onboarding asked to restart it, and $(gate_daemon_state_says "$state" "$2")"
  else
    gate_record "onboarding restarted the service after the configuration changed (Step 7)" fail '' '' '' \
      "$(grep -q 'The Always-On Service started before' "$3" 2>/dev/null && echo 'onboarding asked to restart it' || echo 'onboarding did not ask to restart the service'); $(gate_daemon_state_says "$state" "$2")"
  fi
}

# gate_service_end PORT: stop the Always-On Service and uninstall it, each
# as a person runs it, and record that each did what it says.
gate_service_end() {
  local state
  gate_step "axocoatl service stop" -- axocoatl service stop
  state=none
  for _ in $(seq 1 60); do
    state=$(gate_daemon_state "$1" "$(gate_config_path)")
    [ "$state" = none ] && break
    sleep 2
  done
  gate_service_status "axocoatl service stop: the service is not running" yes no
  gate_check "nothing answers on port $1 after the service stopped" "$(gate_daemon_state_says "$state" "$1")" -- test "$state" = none
  gate_step "axocoatl service uninstall" -- axocoatl service uninstall
  gate_service_status "axocoatl service uninstall: not installed, its unit removed" no no
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

# gate_fixture_command FIXTURE PUBLISHED: the command the gate runs on
# FIXTURE. That is PUBLISHED (the README's command), except for a fixture
# whose fixtures.json entry says `"command": "fixture-readme"`: onboarding's
# first-run sample, whose own README gives the command onboarding runs on
# it, because the README's task describes another bug. Prints nothing when
# that README has no `axocoatl run fix` command.
gate_fixture_command() {
  case "$(gate_fixture_expect "$1" command)" in
    fixture-readme)
      node "$gate_commands_js" readme-run "$gate_repo/tests/fixtures/newuser/$1/README.md" 2>/dev/null || true
      ;;
    *) printf '%s\n' "$2" ;;
  esac
}

# gate_without_models COMMAND: COMMAND without its --model ROLE=MODEL
# options, so a run uses the models onboarding configured (run.models).
gate_without_models() {
  printf '%s\n' "$1" | sed -E 's/[[:space:]]+--model[[:space:]]+[^[:space:]]+//g'
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

# gate_chosen_image LOG: the image a run says it chose, from its plan line
# `image X · toolchain Y (why) · check Z · setup W` (or `image X · no
# toolchain (why) · …`), which PlanSummary::summary in
# crates/axocoatl-daemon/src/loadout/toolchain.rs formats and the CLI prints
# as `· note: image …` when the run starts and as `  image …` under Notes:.
# testdata/run-plan-lines.txt holds the lines CI's runs printed; the fake
# axocoatl prints them, and test-release-gate.sh fails when toolchain.rs
# stops formatting the line this way. Prints nothing when LOG has no plan
# line (`· building the run's image X (…)` is not one).
gate_chosen_image() {
  LC_ALL=C sed -nE 's/^(.*[[:space:]])?image ([^[:space:]]+) · (no )?toolchain .*/\2/p' "$1" 2>/dev/null | head -n 1
}

# gate_image_is IMAGE NAME: NAME (node, python, rust, go) is a whole part of
# IMAGE's repository, its reference without tag or digest:
# localhost/axocoatl-recipe-go:6063b3e9fdc6 is a go image;
# docker.io/library/mongo:7 and localhost/axocoatl-recipe-node:go are not.
gate_image_is() {
  local repository=${1%%@*}
  case "${repository##*/}" in *:*) repository=${repository%:*} ;; esac
  case "$2" in '' | *[!a-z0-9]*) return 1 ;; esac
  printf '%s\n' "$repository" | LC_ALL=C grep -qiE -- "(^|[/._-])$2([/._-]|\$)"
}

# gate_check_image LABEL NAME LOG: record whether the run whose output is
# LOG chose a NAME image (fixtures.json image_contains), by its plan line.
gate_check_image() {
  local chosen
  chosen=$(gate_chosen_image "$3")
  if [ -n "$chosen" ] && gate_image_is "$chosen" "$2"; then
    gate_record "run on $1 chose a $2 image" pass '' '' '' "$chosen"
  else
    gate_record "run on $1 chose a $2 image" fail '' '' '' "expected the run's plan line (image … · toolchain …) to name a $2 image; found '${chosen:-none}'"
  fi
}

# gate_copy_paste_lint REPO NPM_CACHE: scripts/check-copy-paste.mjs on REPO,
# as the docs gate runs it. It parses YAML with js-yaml, a devDependency of
# sites/docs; when REPO has not installed it, `npm ci` in sites/docs (as
# verify-product-browser-gate.sh runs it) installs it first, as its own
# step, with npm's cache in NPM_CACHE. A failed install fails that step and
# the lint then fails every YAML block: nothing is skipped.
gate_copy_paste_lint() {
  if ! node -e '
    const { createRequire } = require("module");
    for (const base of process.argv.slice(1)) {
      try { createRequire(base).resolve("js-yaml"); process.exit(0); } catch {}
    }
    process.exit(1);
  ' "$1/sites/docs/package.json" "$1/scripts/package.json" >/dev/null 2>&1; then
    gate_step "install the lint's YAML parser (npm ci in sites/docs)" -- /bin/bash -c \
      'cd "$1/sites/docs" && npm_config_cache="$2" npm ci --ignore-scripts --no-audit --no-fund' \
      gate-npm "$1" "$2"
  fi
  gate_step "copy-paste lint (scripts/check-copy-paste.mjs)" -- node "$1/scripts/check-copy-paste.mjs"
}

# gate_markup_check LABEL REPO RECORD [OUTPUT...]: no accepted output of
# the run whose record is RECORD, in REPO, holds tool-call markup a model
# wrote as text (`<function=` or `</tool_call>`; a turn that ended on such
# text ran nothing it meant to run). It reads, without a daemon:
#   - every answer the run accepted, from the history its record carries
#     (record-answers.mjs, which reads only a whole record and needs at
#     least one answer from a run that passed or whose checks failed);
#   - every line the run added to a file since the commit the record says
#     it started from (repo_head), in commits and in the working tree, and
#     every new file, except RECORD and each OUTPUT (its receipt and JUnit
#     report), which the gate asked for and which quote what the run read.
# Records the step; fails when either cannot be read.
gate_markup_check() {
  local label=$1 repo=$2 record=$3 name answers status start file found
  shift 3
  name="no tool-call markup in the accepted output of the run on $label"
  answers=$(node "$gate_dir/record-answers.mjs" "$record" 2>&1)
  status=$?
  if [ "$status" != 0 ]; then
    gate_record "$name" fail '' '' '' "$answers"
    return 1
  fi
  if ! start=$(node "$gate_dir/record-answers.mjs" --repo-head "$record" 2>&1) \
    || ! git -C "$repo" rev-parse --quiet --verify "$start^{commit}" >/dev/null 2>&1; then
    gate_record "$name" fail '' '' '' "$answers; cannot tell what the run changed in $repo: $start"
    return 1
  fi
  local outside=() top dir
  top=$(cd -P -- "$repo" && pwd)
  for file in "$record" "$@"; do
    # Relative to the repository, as the run wrote it, or absolute; one
    # outside the repository is not in what the run changed anyway.
    case "$file" in
      '') continue ;;
      /*)
        dir=$(cd -P -- "$(dirname -- "$file")" 2>/dev/null && pwd) || continue
        case "$dir/" in "$top/"*) ;; *) continue ;; esac
        file="${dir#"$top"}/$(basename -- "$file")"
        file=${file#/}
        ;;
    esac
    outside+=(":(exclude,literal)$file")
  done
  if ! git -C "$repo" diff --no-color --no-ext-diff --no-textconv -U0 "$start" -- . "${outside[@]}" \
    >"$GATE_TMP/markup.diff" 2>"$GATE_TMP/markup.err" \
    || ! git -C "$repo" ls-files --others --exclude-standard -z -- . "${outside[@]}" \
      >"$GATE_TMP/markup.new" 2>>"$GATE_TMP/markup.err" \
    || ! (cd "$repo" && xargs -0 sh -c 'grep -a -H -n -F -e "<function=" -e "</tool_call>" -- /dev/null "$@"; [ $? -le 1 ]' gate-grep \
      <"$GATE_TMP/markup.new" >"$GATE_TMP/markup.found" 2>>"$GATE_TMP/markup.err"); then
    gate_record "$name" fail '' '' '' "$answers; cannot read what the run changed in $repo: $(head -c 200 "$GATE_TMP/markup.err" | tr '\n' ' ')"
    return 1
  fi
  # Added lines only: a line the run left alone may quote markup (a parser's
  # own tests, a changelog).
  found=$( {
    awk '/^diff --git / { hunk = 0; file = $NF; sub(/^b\//, "", file); next }
         /^@@/ { hunk = 1; next }
         hunk && /^\+/ { print file ": " substr($0, 2) }' "$GATE_TMP/markup.diff"
    cat "$GATE_TMP/markup.found"
  } | grep -a -F -m 3 -e '<function=' -e '</tool_call>' | cut -c1-200 | tr '\n' ' ')
  if [ -n "$found" ]; then
    gate_record "$name" fail '' '' '' "$answers; a line the run wrote holds tool-call markup: $found"
    return 1
  fi
  gate_record "$name" pass '' '' '' "$answers; no line the run added since ${start:0:12} holds any"
  return 0
}

# gate_run_fixture FIXTURE COMMAND REPO_DIR [WRITER_LABEL]: run COMMAND (as
# published, plus outputs) in a fresh copy of FIXTURE, then verify its
# receipt with the documented command and its record, and check that no
# accepted output holds tool-call markup (gate_markup_check). Records each
# check; returns 0 only when the run and all of them passed.
gate_run_fixture() {
  local fixture=$1 published=$2 repo=$3 label=${4:-$1} command record receipt junit keep
  keep="$GATE_ATTACH/$label"
  if ! gate_fixture_repo "$fixture" "$repo"; then
    gate_record "fixture $fixture exists" fail '' '' '' "tests/fixtures/newuser/$fixture is missing"
    return 1
  fi
  if [ -z "$published" ]; then
    gate_record "run fix on $label" fail '' '' '' "no command to run: tests/fixtures/newuser/$fixture/README.md shows no axocoatl run fix command"
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
  local image setup
  image=$(gate_fixture_expect "$fixture" image_contains)
  setup=$(gate_fixture_expect "$fixture" setup_contains)
  if [ -n "$image" ]; then
    gate_check_image "$label" "$image" "$keep/run.log"
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
  gate_markup_check "$label" "$repo" "$repo/$record" "$receipt" "$junit" || status=1
  return "$status"
}
