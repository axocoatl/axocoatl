#!/bin/sh
# End-to-end installer regressions with a fully local release fixture.
set -eu

if [ "${TEST_INSTALL_TOOL_MODE:-}" = "1" ]; then
  case "${0##*/}" in
    uname)
      case "${1:-}" in
        -s) printf '%s\n' "${TEST_OS:-Linux}" ;;
        -m) printf '%s\n' "${TEST_ARCH:-x86_64}" ;;
        *) exit 2 ;;
      esac
      ;;
    getconf)
      [ "${1:-}" = "GNU_LIBC_VERSION" ] || exit 2
      [ "${TEST_GLIBC:-glibc 2.35}" != "unavailable" ] || exit 1
      printf '%s\n' "${TEST_GLIBC:-glibc 2.35}"
      ;;
    sw_vers)
      [ "${1:-}" = "-productVersion" ] || exit 2
      [ "${TEST_MACOS:-15.0}" != "unavailable" ] || exit 1
      if [ "${TEST_MACOS:-15.0}" = "compat" ]; then
        case "${SYSTEM_VERSION_COMPAT:-}" in
          0) printf '%s\n' 11.0 ;;
          *) printf '%s\n' 10.16 ;;
        esac
      else
        printf '%s\n' "${TEST_MACOS:-15.0}"
      fi
      ;;
    curl)
      output=""
      url=""
      while [ "$#" -gt 0 ]; do
        case "$1" in
          -o)
            [ "$#" -ge 2 ] || exit 2
            output=$2
            shift 2
            ;;
          -*) shift ;;
          *) url=$1; shift ;;
        esac
      done
      [ -n "$url" ] || exit 2
      printf '%s\n' "$url" >> "$TEST_CURL_LOG"
      case "$url" in
        */releases/latest)
          printf '{"tag_name":"%s"}\n' "$TEST_TAG"
          ;;
        *.tar.gz.sha256)
          [ -n "$output" ] || exit 2
          archive_name="${url##*/}"
          archive_name="${archive_name%.sha256}"
          case "${TEST_CHECKSUM_MODE:-valid}" in
            valid) printf '%s  %s\n' "$TEST_DIGEST" "$archive_name" > "$output" ;;
            missing) exit 22 ;;
            malformed) printf '%s\n' "not-a-sha256  $archive_name" > "$output" ;;
            mismatch) printf '%064d  %s\n' 0 "$archive_name" > "$output" ;;
            *) exit 2 ;;
          esac
          ;;
        *.tar.gz)
          [ -n "$output" ] || exit 2
          /bin/cp "$TEST_FIXTURE_ARCHIVE" "$output"
          ;;
        *) exit 22 ;;
      esac
      ;;
    sha256sum)
      printf '%s  %s\n' "$TEST_DIGEST" "$1"
      ;;
    shasum)
      [ "${1:-}" = "-a" ] || exit 2
      [ "${2:-}" = "256" ] || exit 2
      [ "$#" -eq 3 ] || exit 2
      printf '%s  %s\n' "$TEST_DIGEST" "$3"
      ;;
    install)
      printf '%s\n' "$*" >> "$TEST_INSTALL_LOG"
      [ "${1:-}" = "-m" ] || exit 2
      [ "${2:-}" = "0755" ] || exit 2
      [ "$#" -eq 4 ] || exit 2
      /bin/cp "$3" "$TEST_INSTALLED_BINARY"
      /bin/chmod 0755 "$TEST_INSTALLED_BINARY"
      # Every case points the installer at directories under its own work
      # directory, so the stub can install where the installer asked.
      case "$4" in
        "$TEST_WORK_DIR"/*) ;;
        *) echo "install stub: refusing to write outside the test directory: $4" >&2; exit 2 ;;
      esac
      /bin/cp "$3" "$4"
      /bin/chmod 0755 "$4"
      ;;
    *) exit 127 ;;
  esac
  exit 0
fi

script_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
repo_root="$(CDPATH= cd -- "$script_dir/.." && pwd)"
installer="$script_dir/install.sh"
test_driver="$script_dir/test-install.sh"
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-install-test.XXXXXX")"

cleanup() {
  rm -rf "$work_dir"
}
trap cleanup 0 HUP INT TERM

fail() {
  echo "test-install: $*" >&2
  exit 1
}

fixture_dir="$work_dir/fixture"
mkdir -p "$fixture_dir"
current_version="$(awk '
  $0 == "[package]" { package_section = 1; next }
  package_section && /^\[/ { exit }
  package_section && $1 == "version" {
    gsub(/"/, "", $3)
    print $3
    exit
  }
' "$repo_root/axocoatl-cli/Cargo.toml")"
[ -n "$current_version" ] || fail "could not determine the current CLI version"
current_tag="v$current_version"
printf '%s\n' '#!/bin/sh' \
  'if [ "${1:-}" = "--version" ]; then' \
  "  echo 'axocoatl $current_version'" \
  'else' \
  "  echo 'axocoatl $current_version fixture'" \
  'fi' \
  > "$fixture_dir/axocoatl"
chmod 0755 "$fixture_dir/axocoatl"
printf '%s\n' 'Axocoatl license fixture' > "$fixture_dir/LICENSE"
printf '%s\n' 'Axocoatl third-party notice fixture' > "$fixture_dir/THIRD_PARTY_LICENSES.txt"
fixture_archive="$work_dir/axocoatl-${current_tag}-fixture.tar.gz"
tar -czf "$fixture_archive" -C "$fixture_dir" \
  axocoatl LICENSE THIRD_PARTY_LICENSES.txt

fake_bin="$work_dir/bin"
no_hash_bin="$work_dir/bin-no-hash"
shasum_bin="$work_dir/bin-shasum"
mkdir -p "$fake_bin" "$no_hash_bin" "$shasum_bin"

link_tool() {
  target=$1
  name=$2
  ln -s "$target" "$fake_bin/$name"
  ln -s "$target" "$no_hash_bin/$name"
  ln -s "$target" "$shasum_bin/$name"
}

for tool in awk cut grep gzip head mkdir mktemp rm tar tr; do
  tool_path="$(command -v "$tool")"
  [ -n "$tool_path" ] || fail "required test host tool is missing: $tool"
  link_tool "$tool_path" "$tool"
done
for tool in curl getconf install sw_vers uname; do
  link_tool "$test_driver" "$tool"
done
ln -s "$test_driver" "$fake_bin/sha256sum"
ln -s "$test_driver" "$shasum_bin/shasum"

# The real curl and checksum tools, for an install from a file:// release
# directory exactly as a release gate runs it.
real_bin="$work_dir/bin-real-download"
mkdir -p "$real_bin"
for tool in awk cut grep gzip head mkdir mktemp rm tar tr curl; do
  tool_path="$(command -v "$tool")"
  [ -n "$tool_path" ] || fail "required test host tool is missing: $tool"
  ln -s "$tool_path" "$real_bin/$tool"
done
if command -v sha256sum >/dev/null 2>&1; then
  ln -s "$(command -v sha256sum)" "$real_bin/sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  ln -s "$(command -v shasum)" "$real_bin/shasum"
else
  fail "required test host tool is missing: sha256sum or shasum"
fi
for tool in getconf install sw_vers uname; do
  ln -s "$test_driver" "$real_bin/$tool"
done

digest="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
case_count=0

run_case() {
  name=$1
  expected=$2
  expected_text=$3
  os=$4
  arch=$5
  expected_target=$6
  glibc=$7
  macos=$8
  checksum=$9
  path=${10}

  case_count=$((case_count + 1))
  output="$work_dir/$name.out"
  curl_log="$work_dir/$name.curl"
  install_log="$work_dir/$name.install"
  installed_binary="$work_dir/$name.installed"
  : > "$curl_log"
  : > "$install_log"
  case_tmp="$work_dir/$name-tmp"
  case_home="$work_dir/$name-home"
  case_system="$work_dir/$name-system"
  mkdir -p "$case_tmp" "$case_home" "$case_system"

  set +e
  AXOCOATL_INSTALL_SYSTEM_DIR="$case_system" \
  AXOCOATL_INSTALL_TTY="$work_dir/no-terminal" \
  AXOCOATL_MODIFY_PATH=ask \
  AXOCOATL_VERSION= \
  AXOCOATL_INSTALL_BASE_URL= \
  GITHUB_PATH= \
  SHELL=/bin/sh \
  XDG_CONFIG_HOME= \
  ZDOTDIR= \
  TEST_WORK_DIR="$work_dir" \
  TEST_OS="$os" \
  TEST_INSTALL_TOOL_MODE=1 \
  TEST_ARCH="$arch" \
  TEST_TAG="$current_tag" \
  TEST_GLIBC="$glibc" \
  TEST_MACOS="$macos" \
  TEST_CHECKSUM_MODE="$checksum" \
  TEST_DIGEST="$digest" \
  TEST_FIXTURE_ARCHIVE="$fixture_archive" \
  TEST_CURL_LOG="$curl_log" \
  TEST_INSTALL_LOG="$install_log" \
  TEST_INSTALLED_BINARY="$installed_binary" \
  TMPDIR="$case_tmp" \
  HOME="$case_home" \
  PATH="$path" \
    /bin/sh "$installer" > "$output" 2>&1
  status=$?
  set -e

  case "$expected" in
    pass)
      [ "$status" -eq 0 ] || fail "$name unexpectedly failed: $(cat "$output")"
      [ -s "$install_log" ] || fail "$name did not reach the isolated install stub"
      [ -x "$installed_binary" ] || fail "$name did not install an executable fixture"
      [ "$("$installed_binary" --version)" = "axocoatl $current_version" ] \
        || fail "$name installed a fixture with the wrong product version"
      grep -F "axocoatl-${current_tag}-${expected_target}.tar.gz" "$curl_log" >/dev/null \
        || fail "$name did not request the exact $current_tag $expected_target archive"
      ;;
    fail)
      [ "$status" -ne 0 ] || fail "$name unexpectedly succeeded"
      ;;
    *) fail "invalid expectation for $name: $expected" ;;
  esac
  grep -F "$expected_text" "$output" >/dev/null \
    || fail "$name did not report '$expected_text': $(cat "$output")"
}

run_case linux-x86_64 pass "checksum verified" Linux x86_64 x86_64-unknown-linux-gnu "glibc 2.35" unavailable valid "$fake_bin"
run_case linux-aarch64 pass "checksum verified" Linux aarch64 aarch64-unknown-linux-gnu "glibc 2.35" unavailable valid "$fake_bin"
run_case darwin-x86_64 pass "checksum verified" Darwin x86_64 x86_64-apple-darwin unavailable 11.0 valid "$fake_bin"
run_case darwin-arm64 pass "checksum verified" Darwin arm64 aarch64-apple-darwin unavailable 11.0 valid "$fake_bin"
run_case darwin-shasum-only pass "checksum verified" Darwin arm64 aarch64-apple-darwin unavailable 11.0 valid "$shasum_bin"
run_case darwin-macos-compat pass "checksum verified" Darwin x86_64 x86_64-apple-darwin unavailable compat valid "$fake_bin"
run_case darwin-old-macos fail "require macOS 11 or newer; detected macOS 10.15.7" Darwin x86_64 none unavailable 10.15.7 valid "$fake_bin"
run_case darwin-version-unavailable fail "cannot report its macOS version" Darwin x86_64 none unavailable unavailable valid "$fake_bin"
run_case linux-old-glibc fail "require GNU libc 2.35 or newer" Linux x86_64 none "glibc 2.34" unavailable valid "$fake_bin"
run_case linux-musl fail "Alpine/musl needs a source build" Linux x86_64 none "musl libc 1.2.5" unavailable valid "$fake_bin"
run_case windows-native fail "runs on Windows inside WSL2, not natively" Windows_NT x86_64 none unavailable unavailable valid "$fake_bin"
run_case unsupported-arch fail "unsupported architecture 'riscv64'" Linux riscv64 none "glibc 2.35" unavailable valid "$fake_bin"
run_case checksum-missing fail "checksum download failed" Linux x86_64 none "glibc 2.35" unavailable missing "$fake_bin"
run_case checksum-malformed fail "malformed checksum file" Linux x86_64 none "glibc 2.35" unavailable malformed "$fake_bin"
run_case checksum-mismatch fail "checksum mismatch" Linux x86_64 none "glibc 2.35" unavailable mismatch "$fake_bin"
run_case checksum-tool-missing fail "neither sha256sum nor shasum is available" Linux x86_64 none "glibc 2.35" unavailable valid "$no_hash_bin"

[ ! -s "$work_dir/linux-old-glibc.curl" ] || fail "old glibc reached the network stub"
[ ! -s "$work_dir/linux-musl.curl" ] || fail "musl reached the network stub"
[ ! -s "$work_dir/darwin-old-macos.curl" ] || fail "old macOS reached the network stub"
[ ! -s "$work_dir/darwin-version-unavailable.curl" ] || fail "unknown macOS version reached the network stub"
[ ! -s "$work_dir/windows-native.curl" ] || fail "native Windows reached the network stub"
[ ! -s "$work_dir/unsupported-arch.curl" ] || fail "unsupported architecture reached the network stub"

# --- PATH handling ---------------------------------------------------------
# Each case installs where /usr/local/bin is not writable (or is, and is on
# PATH), with a given login shell, terminal answer and AXOCOATL_MODIFY_PATH.
# Environment for one case:
#   P_OS          Darwin or Linux
#   P_SHELL       the user's $SHELL
#   P_SYSTEM      writable: the system directory is writable and used;
#                 missing: it does not exist, so ~/.local/bin is used
#   P_ON_PATH     yes: the chosen directory is on PATH already
#   P_SHADOW      yes: another axocoatl comes first on PATH
#   P_TTY         none: no terminal; otherwise the answer typed (may be empty)
#   P_MODIFY      the AXOCOATL_MODIFY_PATH value
#   P_HOME_NAME   the home directory's last path segment
#   P_GITHUB_PATH yes: GITHUB_PATH names a file, as in a GitHub Actions step
#   P_PODMAN      yes: a podman command is on PATH (a stub that runs nothing)
#   P_VERSION     the AXOCOATL_VERSION value
#   P_BASE_URL    the AXOCOATL_INSTALL_BASE_URL value
#   P_BIN         the directory of tools on PATH (default: the stubs)
#   P_XDG_CONFIG_HOME, P_ZDOTDIR  those variables (default: unset)
run_path_case() {
  name=$1
  case_count=$((case_count + 1))
  output="$work_dir/$name.out"
  curl_log="$work_dir/$name.curl"
  install_log="$work_dir/$name.install"
  installed_binary="$work_dir/$name.installed"
  : > "$curl_log"
  : > "$install_log"
  case_tmp="$work_dir/$name-tmp"
  case_home="$work_dir/$name/${P_HOME_NAME:-home}"
  mkdir -p "$case_tmp" "$case_home"
  case "${P_SYSTEM:-missing}" in
    writable) case_system="$work_dir/$name-system"; mkdir -p "$case_system"; case_dest="$case_system" ;;
    missing) case_system="$work_dir/$name-no-system-dir"; case_dest="$case_home/.local/bin" ;;
    *) fail "invalid P_SYSTEM for $name" ;;
  esac
  case_path="${P_BIN:-$fake_bin}"
  if [ "${P_PODMAN:-no}" = yes ]; then
    podman_dir="$work_dir/$name-podman"
    mkdir -p "$podman_dir"
    printf '%s\n' '#!/bin/sh' 'exit 0' > "$podman_dir/podman"
    chmod 0755 "$podman_dir/podman"
    case_path="$case_path:$podman_dir"
  fi
  case_github_path=""
  if [ "${P_GITHUB_PATH:-no}" = yes ]; then
    case_github_path="$work_dir/$name.github-path"
    : > "$case_github_path"
  fi
  if [ "${P_SHADOW:-no}" = yes ]; then
    shadow_dir="$work_dir/$name-shadow"
    mkdir -p "$shadow_dir"
    printf '%s\n' '#!/bin/sh' 'echo "axocoatl 0.0.1 shadow"' > "$shadow_dir/axocoatl"
    chmod 0755 "$shadow_dir/axocoatl"
    case_path="$shadow_dir:$case_path"
  fi
  [ "${P_ON_PATH:-no}" = yes ] && case_path="$case_path:$case_dest"
  case_tty="$work_dir/$name.tty"
  if [ "${P_TTY-none}" = none ]; then
    case_tty="$work_dir/$name.no-terminal"
  elif [ "$P_TTY" = eof ]; then
    # A terminal that answers with end of input, as Ctrl-D does.
    case_tty=/dev/null
  else
    printf '%s\n' "$P_TTY" > "$case_tty"
  fi

  set +e
  AXOCOATL_INSTALL_SYSTEM_DIR="$case_system" \
  AXOCOATL_INSTALL_TTY="$case_tty" \
  AXOCOATL_MODIFY_PATH="${P_MODIFY:-ask}" \
  AXOCOATL_VERSION="${P_VERSION:-}" \
  AXOCOATL_INSTALL_BASE_URL="${P_BASE_URL:-}" \
  GITHUB_PATH="$case_github_path" \
  SHELL="${P_SHELL:-/bin/zsh}" \
  XDG_CONFIG_HOME="${P_XDG_CONFIG_HOME:-}" \
  ZDOTDIR="${P_ZDOTDIR:-}" \
  TEST_WORK_DIR="$work_dir" \
  TEST_OS="${P_OS:-Darwin}" \
  TEST_INSTALL_TOOL_MODE=1 \
  TEST_ARCH=arm64 \
  TEST_TAG="$current_tag" \
  TEST_GLIBC="glibc 2.35" \
  TEST_MACOS=15.0 \
  TEST_CHECKSUM_MODE=valid \
  TEST_DIGEST="$digest" \
  TEST_FIXTURE_ARCHIVE="$fixture_archive" \
  TEST_CURL_LOG="$curl_log" \
  TEST_INSTALL_LOG="$install_log" \
  TEST_INSTALLED_BINARY="$installed_binary" \
  TMPDIR="$case_tmp" \
  HOME="$case_home" \
  PATH="$case_path" \
    /bin/sh "$installer" > "$output" 2>&1
  status=$?
  set -e
}

path_expect_status() {
  [ "$status" -eq "$1" ] || fail "$name exited $status, not $1: $(cat "$output")"
}

path_expect_text() {
  grep -F -- "$1" "$output" >/dev/null || fail "$name did not report '$1': $(cat "$output")"
}

path_refuse_text() {
  if grep -F -- "$1" "$output" >/dev/null; then
    fail "$name reported '$1': $(cat "$output")"
  fi
}

path_expect_installed_to() {
  [ -x "$1/axocoatl" ] || fail "$name did not install to $1: $(cat "$install_log")"
}

# The printed next command runs as printed, from a shell with the same PATH.
path_expect_next_runs() {
  next_line="$(grep '^Next:  ' "$output" | head -n 1)"
  [ -n "$next_line" ] || fail "$name printed no next command: $(cat "$output")"
  next_command="${next_line#Next:  }"
  next_command="${next_command%%#*}"
  next_result="$(HOME="$case_home" PATH="$case_path" /bin/sh -c "$next_command" 2>&1)" \
    || fail "$name: the printed next command failed: $next_command: $next_result"
  [ "$next_result" = "axocoatl $current_version fixture" ] \
    || fail "$name: the printed next command ran something else: $next_command: $next_result"
}

# path_expect_new_shells_find SHELL FILE...: the installer wrote its line
# ($P_LINE, else path_line_for SHELL) into each FILE, and every new SHELL a
# user gets next finds the installed binary, with its directory on PATH
# exactly once. The shells start as a new terminal or login starts them
# (macOS Terminal: an interactive login shell; Linux: a login shell for SSH,
# the console or the desktop session, an interactive one for a new terminal
# window, and an interactive login one for SSH), from an empty environment
# with a base PATH, so they read only their own startup files. A shell not
# installed here is recorded, and each FILE is read with /bin/sh instead
# (fish: its line is looked for), which checks the line but not which files
# that shell reads.
real_shells=""
missing_shells=""
path_expect_new_shells_find() {
  shell_name=$1
  shift
  expected_line="${P_LINE:-$(path_line_for "$shell_name")}"
  for profile_file in "$@"; do
    [ -f "$profile_file" ] || fail "$name did not write $profile_file: $(cat "$output")"
    grep -Fx -- "$expected_line" "$profile_file" >/dev/null \
      || fail "$name: $profile_file does not have the PATH line: $(cat "$profile_file")"
  done
  case "$shell_name" in
    sh) shell_path=/bin/sh ;;
    *) shell_path="$(command -v "$shell_name" 2>/dev/null || true)" ;;
  esac
  if [ -z "$shell_path" ]; then
    case " $missing_shells " in *" $shell_name "*) ;; *) missing_shells="$missing_shells${missing_shells:+ }$shell_name" ;; esac
    [ "$shell_name" = fish ] && return 0
    for profile_file in "$@"; do
      shell_out="$(env -i HOME="$case_home" PATH=/usr/bin:/bin /bin/sh -c '. "$1" >/dev/null 2>&1; echo "PATH=$PATH"; command -v axocoatl' sh "$profile_file")" || true
      path_expect_once "$profile_file read with sh" "$shell_out" "$@"
    done
    return 0
  fi
  case " $real_shells " in *" $shell_name "*) ;; *) real_shells="$real_shells${real_shells:+ }$shell_name" ;; esac
  case "${P_OS:-Darwin}:$shell_name" in
    *:sh) flag_sets='-lc' ;;
    Darwin:*) flag_sets='-ilc' ;;
    *) flag_sets='-lc -ic -ilc' ;;
  esac
  for flags in $flag_sets; do
    shell_out="$(env -i HOME="$case_home" PATH=/usr/bin:/bin TERM=dumb \
      ${P_XDG_CONFIG_HOME:+"XDG_CONFIG_HOME=$P_XDG_CONFIG_HOME"} \
      "$shell_path" "$flags" 'echo "PATH=$PATH"; command -v axocoatl' </dev/null 2>/dev/null)" || true
    path_expect_once "a new '$shell_name $flags'" "$shell_out" "$@"
  done
  # fish_add_path without --path saves the directory in the universal
  # fish_user_paths (fish_variables), where it outlives the line.
  if [ "$shell_name" = fish ]; then
    fish_variables="${P_XDG_CONFIG_HOME:-$case_home/.config}/fish/fish_variables"
    if [ -f "$fish_variables" ] && grep -F 'fish_user_paths' "$fish_variables" >/dev/null; then
      fail "$name: fish saved a universal fish_user_paths: $(cat "$fish_variables")"
    fi
  fi
}

# path_expect_once WHAT OUTPUT FILE...: OUTPUT (a PATH=... line, then what
# `command -v axocoatl` printed) names the installed binary, and its PATH
# lists the install directory once.
path_expect_once() {
  what=$1
  shell_out=$2
  shift 2
  found_binary="$(printf '%s\n' "$shell_out" | tail -n 1)"
  found_path="$(printf '%s\n' "$shell_out" | sed -n 's/^PATH=//p' | tail -n 1)"
  startup_files="$(for f in "$@"; do printf '%s: ' "$f"; tr '\n' ' ' <"$f"; done)"
  [ "$found_binary" = "$case_dest/axocoatl" ] \
    || fail "$name: $what finds '$found_binary', not $case_dest/axocoatl; startup files: $startup_files"
  path_count="$(printf '%s\n' "$found_path" | tr ':' '\n' | grep -cxF -- "$case_dest" || true)"
  [ "$path_count" = 1 ] \
    || fail "$name: $what lists $case_dest $path_count times on PATH ($found_path); startup files: $startup_files"
}

# The line the installer adds for SHELL, for ~/.local/bin: one that adds it
# only when PATH does not have it (bash-nonlogin: and only in a shell that is
# not a login shell, for ~/.bashrc next to a ~/.profile that adds it).
path_line_for() {
  case "$1" in
    fish) printf '%s' 'fish_add_path --path "$HOME/.local/bin"' ;;
    bash-nonlogin) printf '%s' 'shopt -q login_shell || case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) export PATH="$HOME/.local/bin:$PATH" ;; esac' ;;
    *) printf '%s' 'case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) export PATH="$HOME/.local/bin:$PATH" ;; esac' ;;
  esac
}

# seed_home NAME LAYOUT: the startup files a new account starts with, in the
# home directory of path case NAME, written before the installer runs.
#   ubuntu        Ubuntu's and Debian's /etc/skel: a .profile that reads
#                 .bashrc in bash and then adds ~/.local/bin when it exists,
#                 whether or not PATH has it, and a .bashrc that does nothing
#                 more in a non-interactive shell
#   reads-bashrc  ubuntu's files without the ~/.local/bin lines
#   runner        ubuntu's files and a .bash_profile that reads neither
#                 (GitHub's Ubuntu runner images add one to /etc/skel that
#                 loads nvm)
#   bash-profile-reads-profile  ubuntu's files and a .bash_profile that
#                 reads .profile
#   bash-profile-reads-profile-quoted  the same, with `. "$HOME"/.profile`
#   bash-profile-reads-profile-full-path  the same, with
#                 `source '<home>/.profile'`
#   bash-profile-reads-profile-elsewhere  the same, with a .bash_profile that
#                 reads .profile only on another OS
#   ubuntu-elsewhere  ubuntu's files with the ~/.local/bin lines in .profile
#                 only for another OS
#   arch          a .bash_profile that reads .bashrc, and a .bashrc that
#                 does nothing more in a non-interactive shell (Arch's skel)
#   fedora        a .bash_profile that reads .bashrc, and a .bashrc that adds
#                 ~/.local/bin when PATH does not have it (Fedora's skel)
seed_home() {
  seed_dir="$work_dir/$1/${P_HOME_NAME:-home}"
  mkdir -p "$seed_dir"
  case "$2" in
    ubuntu | reads-bashrc | runner | bash-profile-reads-profile*)
      printf '%s\n' '# like Ubuntu: bash reads .bashrc' \
        'if [ -n "$BASH_VERSION" ]; then' \
        '    if [ -f "$HOME/.bashrc" ]; then' \
        '	. "$HOME/.bashrc"' \
        '    fi' \
        'fi' >"$seed_dir/.profile"
      if [ "$2" != reads-bashrc ]; then
        printf '%s\n' '' \
          '# set PATH so it includes user'"'"'s private bin if it exists' \
          'if [ -d "$HOME/.local/bin" ] ; then' \
          '    PATH="$HOME/.local/bin:$PATH"' \
          'fi' >>"$seed_dir/.profile"
      fi
      printf '%s\n' '# like Ubuntu: nothing more for a non-interactive shell' \
        'case $- in' '    *i*) ;;' '      *) return;;' 'esac' >"$seed_dir/.bashrc"
      ;;
    ubuntu-elsewhere)
      printf '%s\n' 'if [ -n "$BASH_VERSION" ]; then' \
        '    if [ -f "$HOME/.bashrc" ]; then' \
        '	. "$HOME/.bashrc"' \
        '    fi' \
        'fi' \
        'if [ "$(uname)" = SunOS ]; then' \
        'if [ -d "$HOME/.local/bin" ] ; then' \
        '    PATH="$HOME/.local/bin:$PATH"' \
        'fi' \
        'fi' >"$seed_dir/.profile"
      printf '%s\n' 'case $- in' '    *i*) ;;' '      *) return;;' 'esac' >"$seed_dir/.bashrc"
      ;;
    arch)
      printf '%s\n' '[[ -f ~/.bashrc ]] && . ~/.bashrc' >"$seed_dir/.bash_profile"
      printf '%s\n' '[[ $- != *i* ]] && return' >"$seed_dir/.bashrc"
      ;;
    fedora)
      printf '%s\n' 'if [ -f ~/.bashrc ]; then' '	. ~/.bashrc' 'fi' >"$seed_dir/.bash_profile"
      printf '%s\n' 'if ! [[ "$PATH" =~ "$HOME/.local/bin:$HOME/bin:" ]]' 'then' \
        '    PATH="$HOME/.local/bin:$HOME/bin:$PATH"' 'fi' 'export PATH' >"$seed_dir/.bashrc"
      ;;
    *) fail "invalid home layout $2" ;;
  esac
  case "$2" in
    runner)
      printf '%s\n' '[ -s "$NVM_DIR/nvm.sh" ] && \. "$NVM_DIR/nvm.sh"  # This loads nvm' >"$seed_dir/.bash_profile"
      ;;
    bash-profile-reads-profile)
      printf '%s\n' '[ -f "$HOME/.profile" ] && . "$HOME/.profile"' >"$seed_dir/.bash_profile"
      ;;
    bash-profile-reads-profile-quoted)
      printf '%s\n' '. "$HOME"/.profile' >"$seed_dir/.bash_profile"
      ;;
    bash-profile-reads-profile-full-path)
      printf "source '%s/.profile'\n" "$seed_dir" >"$seed_dir/.bash_profile"
      ;;
    bash-profile-reads-profile-elsewhere)
      printf '%s\n' '[ "$(uname)" = SunOS ] && . ~/.profile' >"$seed_dir/.bash_profile"
      ;;
  esac
}

# The installer's own lines in startup file $1.
installer_lines() {
  grep -c -F '# Added by the Axocoatl installer' "$1" 2>/dev/null || true
}

path_expect_no_prompt() {
  if [ -f "$case_tty" ] && grep -F 'is not on your PATH. Add it in' "$case_tty" >/dev/null; then
    fail "$name asked on the terminal: $(cat "$case_tty")"
  fi
}

# path_reinstall_twice COUNT FILE...: install twice more over path case
# $name's home directory, as it is, from a shell without the install
# directory on PATH and with the same terminal answer (y in every caller).
# Neither install asks or adds a line, and each FILE (relative to the home
# directory) keeps COUNT installer lines.
path_reinstall_twice() {
  again_count=$1
  shift
  for again in 2 3; do
    run_path_case "$name"
    path_expect_status 0
    path_expect_text 'to PATH; new terminals find axocoatl'
    path_expect_no_prompt
    path_refuse_text 'added to'
    for again_file in "$@"; do
      again_lines="$(installer_lines "$case_home/$again_file")"
      [ "$again_lines" = "$again_count" ] \
        || fail "$name, install $again: ~/$again_file has $again_lines installer lines, not $again_count: $(cat "$case_home/$again_file")"
    done
  done
}

reset_path_case() {
  P_OS=Darwin P_SHELL=/bin/zsh P_SYSTEM=missing P_ON_PATH=no P_SHADOW=no P_TTY=none P_MODIFY=ask P_HOME_NAME=home
  P_GITHUB_PATH=no P_PODMAN=no P_VERSION='' P_BASE_URL='' P_BIN='' P_XDG_CONFIG_HOME='' P_ZDOTDIR='' P_LINE=''
}

# Reading startup files: the installer's adds_to_path and reads_dot_profile
# on startup files a user may have. A file counts as adding ~/.local/bin when
# it has the installer's marked line or its guarded line anywhere (so that no
# construct makes a reinstall add the line again), or the unmarked export
# line, fish's line or Debian's and Ubuntu's lines where the installer can
# tell nothing before them keeps them from running: at the top level, not
# after a line that ends in &&, ||, | or \, not in a string, a function, a
# subshell or a block for another OS, and not after a return, an exec or
# anything the reader cannot follow. What it cannot see (set -e and a failing
# command, a sourced file that exits, a syntax error it does not notice) is
# not tested here: such a file can count without running the line. Counting
# a file that only seems to add it would leave new terminals without
# axocoatl; an extra line is harmless. A bash login file counts as reading
# ~/.profile by the same rule.
install_helpers="$work_dir/install-helpers.sh"
sed -n '/^# A path as the user knows it/,/^# The startup files a new shell/p' "$installer" >"$install_helpers"
grep '^adds_to_path()' "$install_helpers" >/dev/null && grep '^reads_dot_profile()' "$install_helpers" >/dev/null \
  || fail "could not take adds_to_path and reads_dot_profile from install.sh"
startup_home="$work_dir/startup home"
mkdir -p "$startup_home"
startup_count=0
# startup_expect FUNCTION yes|no WHAT CONTENT [FILE]: FUNCTION on a FILE
# (default .profile) in the home directory that holds CONTENT.
startup_expect() {
  startup_count=$((startup_count + 1))
  startup_file="$startup_home/${5:-.profile}"
  printf '%s\n' "$4" >"$startup_file"
  if (HOME="$startup_home" dest="$startup_home/.local/bin" && . "$install_helpers" && "$1" "$startup_file"); then
    startup_got=yes
  else
    startup_got=no
  fi
  [ "$startup_got" = "$2" ] || fail "$1 says $startup_got, not $2, for $3: $4"
}
stanza='if [ -d "$HOME/.local/bin" ] ; then
    PATH="$HOME/.local/bin:$PATH"
fi'
guarded="$(path_line_for zsh)"
nonlogin="$(path_line_for bash-nonlogin)"
unguarded='export PATH="$HOME/.local/bin:$PATH"'
os_block() { printf 'if [ "$(uname)" = SunOS ]; then\n%s\nfi' "$1"; }
# Constructs that a line reader can lose track in, each valid zsh; the
# second, third, fifth, sixth (bash 4 or later) and last are valid bash too.
zsh_function='mkcd() { mkdir -p "$1" && cd "$1" }'
here_string='read -r word <<< "a b"'
arithmetic='shifted=$((1 << 2))'
zsh_short_for='for f (a b) print -r -- $f'
here_document="$(printf 'cat >/dev/null <<EOF\nfi\nesac\nEOF')"
unbalanced='one=$(case a in a) echo 1;; esac)'
zsh_short_if='if [[ -n x ]] { : }'
c_for='for ((i = 0; i < 1; i++)); do :; done'

# The installer's own lines count wherever they are when its marker comment
# is right before them, and the guarded line always; an own line without the
# marker (as 1.4.0 told users to add it) counts only where the installer can
# tell nothing before it keeps it from running.
startup_expect adds_to_path yes "the installer's line" "$(printf '# Added by the Axocoatl installer\n%s' "$guarded")"
startup_expect adds_to_path yes "the installer's ~/.bashrc line" "$nonlogin"
startup_expect adds_to_path yes "the unguarded line (1.4.0 and earlier printed it; unreleased 1.5.0 builds wrote it)" "$unguarded"
startup_expect adds_to_path yes "the unguarded line with the full path, as 1.4.0 printed it" \
  "export PATH=\"$startup_home/.local/bin:\$PATH\""
startup_expect adds_to_path yes "the installer's line with the full path" \
  "case \":\$PATH:\" in *\":$startup_home/.local/bin:\"*) ;; *) export PATH=\"$startup_home/.local/bin:\$PATH\" ;; esac"
startup_expect adds_to_path yes "the installer's line after Ubuntu's ~/.bashrc stop" "$(printf 'case $- in\n  *i*) ;;\n  *) return;;\nesac\n%s' "$nonlogin")" .bashrc
startup_expect adds_to_path yes "the installer's line after a function and a conda block" \
  "$(printf 'f() {\n  if true; then :; fi\n}\n# >>> conda initialize >>>\nif [ $? -eq 0 ]; then\n  :\nelse\n  export PATH="/opt/conda/bin:$PATH"\nfi\n# <<< conda initialize <<<\n%s' "$guarded")"
for construct in "$zsh_function" "$here_string" "$arithmetic" "$zsh_short_for" "$here_document" "$unbalanced" "$zsh_short_if" \
  "$c_for" 'fi' '}' 'if true; then'; do
  for own in "$guarded" "$nonlogin" "$unguarded"; do
    startup_expect adds_to_path yes "the installer's line after $construct" "$(printf '%s\n\n# Added by the Axocoatl installer\n%s' "$construct" "$own")" .zshrc
  done
done
startup_expect adds_to_path yes "the installer's line in a block for another OS (taken at the user's word)" "$(os_block "  $guarded")"
startup_expect adds_to_path yes "the installer's line in a loop" "$(printf 'for d in a; do\n%s\ndone' "$guarded")"
startup_expect adds_to_path no "the unmarked line after a line that ends in &&" "$(printf '[ "$(uname)" = SunOS ] &&\n  %s' "$unguarded")"
startup_expect adds_to_path no "the unmarked line after a line that ends in \\" "$(printf '[ "$(uname)" = SunOS ] && \\\n  %s' "$unguarded")"
startup_expect adds_to_path no "the unmarked line in a block for another OS" "$(os_block "  $unguarded")"
startup_expect adds_to_path no "the unmarked line in a function" "$(printf 'use_local_bin() {\n  %s\n}' "$unguarded")"
startup_expect adds_to_path no "the unmarked line after an early return" "$(printf '[ -n "$NO_LOCAL" ] && return\n%s' "$unguarded")"
startup_expect adds_to_path yes "the marked line in a block for another OS (taken at the user's word)" \
  "$(os_block "$(printf '  # Added by the Axocoatl installer\n  %s' "$unguarded")")"
startup_expect adds_to_path yes "fish's line" "$(path_line_for fish)" config.fish
startup_expect adds_to_path yes "the fish line unreleased 1.5.0 builds wrote" 'fish_add_path "$HOME/.local/bin"' config.fish
startup_expect adds_to_path yes "the unguarded line in fish" "$unguarded" config.fish
startup_expect adds_to_path yes "fish's line after blocks" \
  "$(printf 'function mkcd; mkdir -p $argv; and cd $argv; end\nif status is-interactive\nend\nif test -n x\nelse if test -n y\nend\n%s' "$(path_line_for fish)")" config.fish
startup_expect adds_to_path no "fish's unmarked line in a block" "$(printf 'if test (uname) = SunOS\n  %s\nend' "$(path_line_for fish)")" config.fish
startup_expect adds_to_path no "fish's unmarked line in a function" "$(printf 'function local_bin\n  %s\nend' "$(path_line_for fish)")" config.fish
startup_expect adds_to_path no "fish's unmarked line after a return" "$(printf 'status is-interactive; or return\n%s' "$(path_line_for fish)")" config.fish
startup_expect adds_to_path no "fish's unmarked line after an exec" "$(printf 'exec zsh\n%s' "$(path_line_for fish)")" config.fish
startup_expect adds_to_path yes "fish's unmarked line after a block word in quotes" "$(printf 'echo "begin"\n%s' "$(path_line_for fish)")" config.fish
startup_expect adds_to_path no "fish's unmarked line in a block with a quoted end" \
  "$(printf 'if test -n x\n  echo "x; end"\n  %s\nend' "$(path_line_for fish)")" config.fish
startup_expect adds_to_path no "fish's unmarked line after a quoted test that opens a block" \
  "$(printf 'if test "$x" = y\n  %s\nend' "$(path_line_for fish)")" config.fish
startup_expect adds_to_path yes "fish's marked line in a block (taken at the user's word)" \
  "$(printf 'if test (uname) = Linux\n  # Added by the Axocoatl installer\n  %s\nend' "$(path_line_for fish)")" config.fish
startup_expect adds_to_path no "the installer's sh line in fish" "$guarded" config.fish
startup_expect adds_to_path no "fish's line in zsh" "$(path_line_for fish)" .zprofile
startup_expect adds_to_path no "a subdirectory" 'export PATH="$HOME/.local/bin/scripts:$PATH"'
startup_expect adds_to_path no "a line that takes it out of PATH" 'export PATH="${PATH#$HOME/.local/bin:}"'
startup_expect adds_to_path no "PATH in a comment" 'if [ -d "$HOME/.local/bin" ]; then echo has-local-bin; fi  # PATH'
startup_expect adds_to_path no "the installer's line in a comment" "# $guarded"
startup_expect adds_to_path no "the installer's line with more on its line" "$guarded # mine"
startup_expect adds_to_path no "a line for another OS" '[ "$(uname)" = SunOS ] && export PATH="$HOME/.local/bin:$PATH"'

# Debian's and Ubuntu's lines count only where the installer can tell
# nothing before them keeps them from running.
startup_expect adds_to_path yes "Ubuntu's lines" "$stanza"
startup_expect adds_to_path yes "Ubuntu's lines after a keyword that starts no command" \
  "$(printf '%s\n' "zstyle ':completion:*' list-prompt %SAt %p: Hit TAB for more, or the character to insert%s" \
    'case $x in a) if true; then :; fi;; esac' "$stanza")" .zshrc
for construct in "$here_string" "$arithmetic" 'function f { :; }' 'g(){ :; }' "$(printf 'plugins=(\n  git\n)')" \
  "$(printf "note='a\nb'")" 'eval "$(/opt/homebrew/bin/brew shellenv)"'; do
  startup_expect adds_to_path yes "Ubuntu's lines after $construct" "$(printf '%s\n%s' "$construct" "$stanza")"
done
startup_expect adds_to_path no "Ubuntu's lines in a block for another OS" "$(os_block "$stanza")"
startup_expect adds_to_path no "Ubuntu's lines in a function" "$(printf 'f() {\n%s\n}' "$stanza")"
startup_expect adds_to_path no "Ubuntu's lines in f(){" "$(printf 'f(){\n%s\n}' "$stanza")"
startup_expect adds_to_path no "Ubuntu's lines in a block for another OS after f(){ :; }" "$(os_block "$(printf '  f(){ :; }\n%s' "$stanza")")"
startup_expect adds_to_path no "Ubuntu's lines in a block for another OS after function f { :; }" \
  "$(os_block "$(printf '  function f { :; }\n%s' "$stanza")")"
startup_expect adds_to_path no "Ubuntu's lines in a block for another OS after f() { :; }" \
  "$(os_block "$(printf '  f() { :; }\n%s' "$stanza")")"
startup_expect adds_to_path no "Ubuntu's lines in a loop" "$(printf 'for d in a; do\n%s\ndone' "$stanza")"
startup_expect adds_to_path no "Ubuntu's lines in a subshell" "$(printf '(\n%s\n)' "$stanza")"
startup_expect adds_to_path no "Ubuntu's lines in a group for another OS" "$(printf '[ "$(uname)" = SunOS ] && {\n%s\n}' "$stanza")"
startup_expect adds_to_path no "Ubuntu's lines in a case" "$(printf 'case $(uname) in\nSunOS)\n%s\n;;\nesac' "$stanza")"
startup_expect adds_to_path no "Ubuntu's lines after a line that ends in &&" "$(printf '[ "$(uname)" = SunOS ] &&\n%s' "$stanza")"
startup_expect adds_to_path no "Ubuntu's lines after a line that ends in ||" "$(printf '[ "$(uname)" = Linux ] ||\n%s' "$stanza")"
startup_expect adds_to_path no "Ubuntu's lines after a line that ends in |" "$(printf 'true |\n%s' "$stanza")"
startup_expect adds_to_path no "Ubuntu's lines after a line that ends in \\" "$(printf '[ "$(uname)" = SunOS ] && \\\n%s' "$stanza")"
startup_expect adds_to_path no "Ubuntu's lines after a line that ends in an open quote" "$(printf "note='\n%s\n'" "$stanza")"
startup_expect adds_to_path no "Ubuntu's lines after a return" "$(printf '[ -n "$PS1" ] || return\n%s' "$stanza")"
startup_expect adds_to_path no "Ubuntu's lines after an exec" "$(printf 'exec zsh\n%s' "$stanza")"
startup_expect adds_to_path no "Ubuntu's lines after a here-document" "$(printf '%s\n%s' "$here_document" "$stanza")"
startup_expect adds_to_path no "Ubuntu's lines after a block the reader cannot balance" "$(printf '%s\n%s' "$zsh_function" "$stanza")" .zshrc
startup_expect adds_to_path no "Ubuntu's lines after a construct the reader cannot follow" "$(printf '%s\n%s' "$unbalanced" "$stanza")"
startup_expect adds_to_path no "Ubuntu's lines after a C-style for" "$(printf '%s\n%s' "$c_for" "$stanza")"
startup_expect adds_to_path no "Ubuntu's lines after a syntax error (a line that starts with |)" "$(printf '| true\n%s' "$stanza")"
startup_expect adds_to_path no "Ubuntu's lines for a subdirectory" 'if [ -d "$HOME/.local/bin/x" ] ; then
    PATH="$HOME/.local/bin/x:$PATH"
fi'
startup_expect adds_to_path no "Fedora's ~/.bashrc lines" 'if ! [[ "$PATH" =~ "$HOME/.local/bin:$HOME/bin:" ]]
then
    PATH="$HOME/.local/bin:$HOME/bin:$PATH"
fi'
startup_expect adds_to_path no "Ubuntu's lines in fish" "$stanza" config.fish
# Constructs the reader once misread, so that a line in or after them
# counted although no shell runs it: a case inside a $( ) over several lines
# (its pattern's ) is not the end of the $( )), an apostrophe in a comment
# inside an array (not a quote), return or exit written with a backslash,
# and zsh's bye.
startup_expect adds_to_path no "Ubuntu's lines in a case inside a \$( ) over several lines" \
  "$(printf 'x=$(\ncase a in\nb)\n%s\n;;\nesac\n)' "$stanza")"
startup_expect adds_to_path no "Ubuntu's lines in a quote after an array with an apostrophe in a comment" \
  "$(printf "plugins=(\n  git  # don't remove\n)\ny='a)\n%s\n'" "$stanza")" .zshrc
startup_expect adds_to_path yes "Ubuntu's lines after an array with an apostrophe in a comment" \
  "$(printf "plugins=(\n  git  # don't remove\n)\n%s" "$stanza")" .zshrc
startup_expect adds_to_path yes "Ubuntu's lines after a zsh glob flag in an array" \
  "$(printf 'files=(*(#qN))\n%s' "$stanza")" .zshrc
startup_expect adds_to_path no "Ubuntu's lines after return written with a backslash" "$(printf '[ -n "$PS1" ] || \\return\n%s' "$stanza")"
startup_expect adds_to_path no "Ubuntu's lines after zsh's bye" "$(printf '[[ -o interactive ]] || bye\n%s' "$stanza")" .zshrc
startup_expect adds_to_path no "the unmarked line after exit written with a backslash" "$(printf '[ -n "$PS1" ] || \\exit\n%s' "$unguarded")" .zshrc

for reads in '. ~/.profile' 'source ~/.profile' '. "$HOME/.profile"' '. "$HOME"/.profile' '. "${HOME}"/.profile' \
  ". '$startup_home/.profile'" "source \"$startup_home/.profile\"" '[ -f ~/.profile ] && . ~/.profile' \
  '[[ -r "$HOME/.profile" ]] && source "$HOME/.profile"' 'test -f ~/.profile && . ~/.profile' \
  'if [ -f ~/.profile ]; then . ~/.profile; fi' "$(printf 'if [ -f "$HOME/.profile" ]; then\n    . "$HOME/.profile"\nfi')" \
  "$(printf 'if [ -f ~/.profile ]; then\n  f(){ :; }\n  . ~/.profile\nfi')" "$(printf '%s\n. ~/.profile' "$here_string")" \
  "$(printf '%s\n. ~/.profile' "$arithmetic")" "$(printf '[[ -f ~/.bashrc ]] && . ~/.bashrc\n. ~/.profile')"; do
  startup_expect reads_dot_profile yes "$reads" "$reads" .bash_profile
done
for reads in '# . ~/.profile' '. ~/.profile.local' '. "~/.profile"' ". '\$HOME/.profile'" '. ~/x/.profile' \
  '[ "$(uname)" = SunOS ] && . ~/.profile' "emulate sh -c '. ~/.profile'" "$(os_block '  . ~/.profile')" \
  "$(printf 'if [ -f ~/.profile ]; then\n  :\nelse\n  . ~/.profile\nfi')" "$(printf '[ -n "$PS1" ] || return\n. ~/.profile')" \
  "$(printf '[ "$(uname)" = SunOS ] &&\n  . ~/.profile')" "$(printf '[ "$(uname)" = Linux ] ||\n  . ~/.profile')" \
  "$(printf 'true |\n  . ~/.profile')" "$(printf ': \\\n. ~/.profile')" "$(printf '[ "$(uname)" = SunOS ] && \\\n  . ~/.profile')" \
  "$(printf "note='\n. ~/.profile\n'")" "$(printf 'load(){\n  . ~/.profile\n}')" "$(printf 'function load {\n  . ~/.profile\n}')" \
  "$(os_block "$(printf '  f(){ :; }\n  . ~/.profile')")" "$(os_block "$(printf '  function f { :; }\n  . ~/.profile')")" \
  "$(os_block "$(printf '  f() { :; }\n  . ~/.profile')")" \
  "$(printf 'if [ -f ~/.profile ]; then\n  f(){ :; }\nelse\n  . ~/.profile\nfi')" \
  "$(printf 'if [ -f ~/.profile ]; then\n  false &&\n  . ~/.profile\nfi')" "$(printf '(\n. ~/.profile\n)')" \
  "$(printf 'exec zsh\n. ~/.profile')" "$(printf '%s\n. ~/.profile' "$here_document")"; do
  startup_expect reads_dot_profile no "$reads" "$reads" .bash_profile
done
# The then branch of an if that tests ~/.profile reads it only while that
# same if is open, and only when that if ends with fi alone, in this shell:
# not in an if that a later `fi; if`, `fi && if`, `done; if`, `}; if`,
# `esac; if` or `); if` opens on the line that closes the first block, and
# not when the if ends in `fi | cat` or `fi &`. A ~ with a quote right
# after it is not the home directory either.
for reads in \
  "$(printf 'if [ -f ~/.profile ]; then\n  :\nfi; if [ "$(uname)" = SunOS ]; then\n  . ~/.profile\nfi')" \
  "$(printf 'if [ -f ~/.profile ]; then\n  :\nfi && if [ "$(uname)" = SunOS ]; then\n  . ~/.profile\nfi')" \
  "$(printf 'if [ -f ~/.profile ]; then\n  :\nfi;if [ "$(uname)" = SunOS ]; then\n  . ~/.profile\nfi')" \
  "$(printf 'if [ -f ~/.profile ]; then\n  for d in a; do\n    :\n  done; fi; if [ "$(uname)" = SunOS ]; then\n  . ~/.profile\nfi')" \
  "$(printf 'if [ -f ~/.profile ]; then\n  :\nfi; while false; do\n  :\ndone; if [ "$(uname)" = SunOS ]; then\n  . ~/.profile\nfi')" \
  "$(printf 'if [ -f ~/.profile ]; then\n  :\nfi; {\n  :\n}; if [ "$(uname)" = SunOS ]; then\n  . ~/.profile\nfi')" \
  "$(printf 'if [ -f ~/.profile ]; then\n  :\nfi; case $- in\n  *) :;;\nesac; if [ "$(uname)" = SunOS ]; then\n  . ~/.profile\nfi')" \
  "$(printf 'if [ -f ~/.profile ]; then\n  :\nfi; (\n  :\n); if [ "$(uname)" = SunOS ]; then\n  . ~/.profile\nfi')" \
  "$(printf 'if [ -f ~/.profile ]; then\n  . ~/.profile\nfi | cat')" \
  "$(printf 'if [ -f ~/.profile ]; then\n  . ~/.profile\nfi &')" \
  '. ~"/.profile"' ". ~'/.profile'"; do
  startup_expect reads_dot_profile no "$reads" "$reads" .bash_profile
done
for reads in \
  "$(printf 'if [ -f ~/.profile ]; then\n  . ~/.profile\nelse\n  :\nfi')" \
  "$(printf 'if [ -f ~/.profile ]; then\n  . ~/.profile\nfi  # read it')" \
  "$(printf 'if [ -f ~/.profile ]; then\n  . ~/.profile\nfi\nif [ "$(uname)" = SunOS ]; then\n  :\nfi')" \
  "$(printf 'if [ -f ~/.profile ]; then\n  :\nfi\nif [ -f ~/.profile ]; then\n  . ~/.profile\nfi')" \
  "$(printf 'for d in a; do\n  :\ndone\nif [ -f ~/.profile ]; then\n  . ~/.profile\nfi')" \
  '. ~/".profile"'; do
  startup_expect reads_dot_profile yes "$reads" "$reads" .bash_profile
done

# A stock Mac in a pipe with no terminal: ~/.local/bin, the persistent line
# printed for ~/.zprofile, nothing written, and the next command a full path.
reset_path_case
run_path_case path-mac-no-terminal
path_expect_status 0
path_expect_installed_to "$case_home/.local/bin"
path_expect_text "~/.local/bin is not on your PATH. For new terminals to find axocoatl, add this line to ~/.zprofile:"
path_expect_text "    $(path_line_for zsh)"
path_expect_text "Next:  $case_home/.local/bin/axocoatl onboard"
path_expect_text "Podman, which every Session and run needs, is not installed yet; onboard (below) sets it up or says how, asking first"
[ ! -e "$case_home/.zprofile" ] || fail "$name wrote ~/.zprofile without being asked"
path_expect_next_runs

# A terminal, Enter accepts the default: the line goes into ~/.zprofile once,
# and a new login shell finds axocoatl.
reset_path_case
P_TTY=''
run_path_case path-mac-terminal-yes
path_expect_status 0
grep -F '~/.local/bin is not on your PATH. Add it in ~/.zprofile? [Y/n]' "$case_tty" >/dev/null \
  || fail "$name did not ask on the terminal: $(cat "$case_tty")"
path_expect_text "added to ~/.zprofile:  $(path_line_for zsh)"
path_expect_text 'new terminals find axocoatl; in this one, run:  export PATH="$HOME/.local/bin:$PATH"'
path_expect_new_shells_find zsh "$case_home/.zprofile"
path_expect_next_runs
[ "$(grep -c -F 'export PATH="$HOME/.local/bin:$PATH"' "$case_home/.zprofile")" -eq 1 ] \
  || fail "$name wrote the PATH line more than once"

# Install again over that profile: nothing is asked or added.
first_profile="$case_home/.zprofile"
reset_path_case
P_TTY='y'
name=path-mac-terminal-again
mkdir -p "$work_dir/$name/home"
cp "$first_profile" "$work_dir/$name/home/.zprofile"
run_path_case "$name"
path_expect_status 0
path_expect_text '~/.zprofile already adds ~/.local/bin to PATH; new terminals find axocoatl'
path_expect_no_prompt
[ "$(grep -c -F 'export PATH="$HOME/.local/bin:$PATH"' "$case_home/.zprofile")" -eq 1 ] \
  || fail "$name added the PATH line again"
path_expect_next_runs
path_reinstall_twice 1 .zprofile

# A ~/.zprofile that only seems to add ~/.local/bin does not count: a
# subdirectory, a line that takes it out of PATH, PATH only in a comment, a
# line for another OS, and Debian's lines where something before them can
# keep them from running (for another OS, after a line that ends in && or \,
# in a function, or after a function inside a block for another OS). Each
# gets the line, and a new login shell finds axocoatl, with ~/.local/bin on
# PATH once.
fake_number=0
while IFS= read -r fake_line <&3; do
  [ -n "$fake_line" ] || continue
  fake_number=$((fake_number + 1))
  reset_path_case
  P_TTY=''
  name="path-mac-zprofile-seems-$fake_number"
  mkdir -p "$work_dir/$name/home"
  printf '%s\n' "$fake_line" | sed 's/  *NEWLINE  */\
/g' >"$work_dir/$name/home/.zprofile"
  run_path_case "$name"
  path_expect_status 0
  path_refuse_text 'already adds'
  grep -F '~/.local/bin is not on your PATH. Add it in ~/.zprofile? [Y/n]' "$case_tty" >/dev/null \
    || fail "$name ($fake_line) did not ask about ~/.zprofile: $(cat "$case_tty")"
  path_expect_text "added to ~/.zprofile:  $(path_line_for zsh)"
  path_expect_new_shells_find zsh "$case_home/.zprofile"
done 3<<'SEEMS'
export PATH="$HOME/.local/bin/scripts:$PATH"
export PATH="${PATH#$HOME/.local/bin:}"
if [ -d "$HOME/.local/bin" ]; then echo has-local-bin >/dev/null; fi  # PATH
[ "$(uname)" = SunOS ] && export PATH="$HOME/.local/bin:$PATH"
if [ "$(uname)" = SunOS ]; then  NEWLINE  if [ -d "$HOME/.local/bin" ] ; then  NEWLINE  PATH="$HOME/.local/bin:$PATH"  NEWLINE  fi  NEWLINE  fi
[ "$(uname)" = SunOS ] &&  NEWLINE  if [ -d "$HOME/.local/bin" ] ; then  NEWLINE  PATH="$HOME/.local/bin:$PATH"  NEWLINE  fi
[ "$(uname)" = SunOS ] && \  NEWLINE  if [ -d "$HOME/.local/bin" ] ; then  NEWLINE  PATH="$HOME/.local/bin:$PATH"  NEWLINE  fi
f(){  NEWLINE  if [ -d "$HOME/.local/bin" ] ; then  NEWLINE  PATH="$HOME/.local/bin:$PATH"  NEWLINE  fi  NEWLINE  }
if [ "$(uname)" = SunOS ]; then  NEWLINE  f(){ :; }  NEWLINE  if [ -d "$HOME/.local/bin" ] ; then  NEWLINE  PATH="$HOME/.local/bin:$PATH"  NEWLINE  fi  NEWLINE  fi
if [ "$(uname)" = SunOS ]; then  NEWLINE  function f { :; }  NEWLINE  if [ -d "$HOME/.local/bin" ] ; then  NEWLINE  PATH="$HOME/.local/bin:$PATH"  NEWLINE  fi  NEWLINE  fi
SEEMS
[ "$fake_number" = 10 ] || fail "ran $fake_number of the 10 look-alike ~/.zprofile cases"

# Installing again never adds the line again, whatever comes before it in
# the file: here constructs a line reader can lose track in, each valid zsh
# (a function whose } follows a command, a here-string, an arithmetic shift,
# zsh's short for and if, a here-document, a case in $( )). The first install
# adds the line, two more add nothing, and a new login shell finds axocoatl
# with ~/.local/bin on PATH once.
again_number=0
while IFS= read -r again_text <&3; do
  [ -n "$again_text" ] || continue
  again_number=$((again_number + 1))
  reset_path_case
  P_TTY='y'
  name="path-mac-zprofile-reinstall-$again_number"
  mkdir -p "$work_dir/$name/home"
  printf '%s\n' "$again_text" | sed 's/  *NEWLINE  */\
/g' >"$work_dir/$name/home/.zprofile"
  run_path_case "$name"
  path_expect_status 0
  path_expect_text "added to ~/.zprofile:  $(path_line_for zsh)"
  path_reinstall_twice 1 .zprofile
  path_expect_new_shells_find zsh "$case_home/.zprofile"
done 3<<'CONSTRUCTS'
mkcd() { mkdir -p "$1" && cd "$1" }
read -r word <<< "a b"
shifted=$((1 << 2))
for f (a b) print -r -- $f >/dev/null
if [[ -n x ]] { : }
cat >/dev/null <<EOF  NEWLINE  fi  NEWLINE  esac  NEWLINE  EOF
one=$(case a in a) echo 1;; esac)
CONSTRUCTS
[ "$again_number" = 7 ] || fail "ran $again_number of the 7 reinstall ~/.zprofile cases"

# The same on Linux for a zsh user whose ~/.zshrc starts with such a function:
# ~/.zshrc and ~/.zprofile each get the line once.
reset_path_case
P_OS=Linux P_SHELL=/usr/bin/zsh P_TTY='y'
name=path-linux-zshrc-reinstall
mkdir -p "$work_dir/$name/home"
printf '%s\n' 'mkcd() { mkdir -p "$1" && cd "$1" }' 'read -r word <<< "a b"' >"$work_dir/$name/home/.zshrc"
run_path_case "$name"
path_expect_status 0
path_expect_text "added to ~/.zshrc and ~/.zprofile:  $(path_line_for zsh)"
path_reinstall_twice 1 .zshrc .zprofile
path_expect_new_shells_find zsh "$case_home/.zshrc" "$case_home/.zprofile"

# bash on Linux next to Ubuntu's files, with such constructs in ~/.bashrc
# (valid bash, 3.2 included, so a C-style for instead of a case in $( )):
# ~/.bashrc gets its line once, and ~/.profile none.
again_number=0
for again_text in 'read -r word <<< "a b"' 'shifted=$((1 << 2))' "$(printf 'cat >/dev/null <<EOF\nfi\nEOF')" \
  'for ((i = 0; i < 1; i++)); do :; done'; do
  again_number=$((again_number + 1))
  reset_path_case
  P_OS=Linux P_SHELL=/bin/bash P_TTY='y'
  name="path-linux-bashrc-reinstall-$again_number"
  seed_home "$name" ubuntu
  printf '%s\n' "$again_text" >>"$work_dir/$name/home/.bashrc"
  run_path_case "$name"
  path_expect_status 0
  path_expect_text "added to ~/.bashrc:  $(path_line_for bash-nonlogin)"
  path_reinstall_twice 1 .bashrc
  [ "$(installer_lines "$case_home/.profile")" = 0 ] || fail "$name wrote ~/.profile"
  P_LINE="$(path_line_for bash-nonlogin)"
  path_expect_new_shells_find bash "$case_home/.bashrc"
done

# The line installers up to 1.4.0 printed for the user to add, with the full
# path, counts: nothing is asked or added, on three installs.
reset_path_case
P_TTY='y'
name=path-mac-1-4-line
mkdir -p "$work_dir/$name/home"
printf 'export PATH="%s:$PATH"\n' "$work_dir/$name/home/.local/bin" >"$work_dir/$name/home/.zprofile"
run_path_case "$name"
path_expect_status 0
path_expect_text '~/.zprofile already adds ~/.local/bin to PATH; new terminals find axocoatl'
path_expect_no_prompt
path_reinstall_twice 0 .zprofile
[ "$(wc -l <"$case_home/.zprofile" | tr -d ' ')" = 1 ] || fail "$name changed ~/.zprofile: $(cat "$case_home/.zprofile")"
P_LINE="export PATH=\"$case_home/.local/bin:\$PATH\""
path_expect_new_shells_find zsh "$case_home/.zprofile"

# Answering no changes nothing and prints the line instead.
reset_path_case
P_TTY='n'
run_path_case path-mac-terminal-no
path_expect_status 0
path_expect_text 'add this line to ~/.zprofile:'
[ ! -e "$case_home/.zprofile" ] || fail "$name wrote ~/.zprofile after the answer no"
path_expect_next_runs

# End of input at the question (Ctrl-D) is not consent: nothing is written,
# and the line is printed instead.
reset_path_case
P_TTY=eof
run_path_case path-mac-terminal-eof
path_expect_status 0
path_expect_text 'add this line to ~/.zprofile:'
path_refuse_text 'added to ~/.zprofile'
[ ! -e "$case_home/.zprofile" ] || fail "$name wrote ~/.zprofile on end of input"
path_expect_next_runs

# AXOCOATL_MODIFY_PATH=no never asks, even with a terminal.
reset_path_case
P_TTY='y'
P_MODIFY=no
run_path_case path-modify-no
path_expect_status 0
path_expect_no_prompt
[ ! -e "$case_home/.zprofile" ] || fail "$name wrote ~/.zprofile with AXOCOATL_MODIFY_PATH=no"
path_expect_text 'add this line to ~/.zprofile:'

# AXOCOATL_MODIFY_PATH=yes adds the line without a terminal.
reset_path_case
P_MODIFY=yes
run_path_case path-modify-yes
path_expect_status 0
path_expect_new_shells_find zsh "$case_home/.zprofile"

# An unknown AXOCOATL_MODIFY_PATH stops before any download.
reset_path_case
P_MODIFY=maybe
run_path_case path-modify-invalid
path_expect_status 1
path_expect_text "AXOCOATL_MODIFY_PATH must be ask, yes or no, not 'maybe'"
[ ! -s "$curl_log" ] || fail "$name reached the network stub"

# Linux bash: a new terminal reads ~/.bashrc, and a new login reads the first
# of ~/.bash_profile, ~/.bash_login and ~/.profile, so the line goes into
# ~/.bashrc and that login file (~/.profile when there is none).
reset_path_case
P_OS=Linux P_SHELL=/bin/bash P_TTY='yes'
run_path_case path-linux-bash
path_expect_status 0
grep -F '~/.local/bin is not on your PATH. Add it in ~/.bashrc and ~/.profile? [Y/n]' "$case_tty" >/dev/null \
  || fail "$name did not name ~/.bashrc and ~/.profile in its question: $(cat "$case_tty")"
path_expect_text "added to ~/.bashrc and ~/.profile:  $(path_line_for bash)"
path_expect_text 'new terminals find axocoatl; in this one, run:  export PATH="$HOME/.local/bin:$PATH"'
path_expect_new_shells_find bash "$case_home/.bashrc" "$case_home/.profile"
[ ! -e "$case_home/.bash_profile" ] || fail "$name wrote ~/.bash_profile on Linux"
path_expect_next_runs

# Ubuntu's and Debian's startup files: ~/.profile reads ~/.bashrc and then
# adds ~/.local/bin, now that it exists, without looking at PATH. It gets no
# line; ~/.bashrc gets one that skips login shells, so a new login lists
# ~/.local/bin once and a new terminal of a session that started before the
# install finds axocoatl too.
reset_path_case
P_OS=Linux P_SHELL=/bin/bash P_TTY=''
seed_home path-linux-bash-ubuntu ubuntu
cp "$work_dir/path-linux-bash-ubuntu/home/.profile" "$work_dir/ubuntu-profile-before"
run_path_case path-linux-bash-ubuntu
path_expect_status 0
path_expect_text '~/.profile already adds ~/.local/bin to PATH in a new login'
grep -F '~/.local/bin is not on your PATH. Add it in ~/.bashrc? [Y/n]' "$case_tty" >/dev/null \
  || fail "$name did not ask about ~/.bashrc alone: $(cat "$case_tty")"
path_expect_text "added to ~/.bashrc:  $(path_line_for bash-nonlogin)"
cmp -s "$work_dir/ubuntu-profile-before" "$case_home/.profile" || fail "$name changed ~/.profile: $(cat "$case_home/.profile")"
P_LINE="$(path_line_for bash-nonlogin)"
path_expect_new_shells_find bash "$case_home/.bashrc"
[ ! -e "$case_home/.bash_profile" ] || fail "$name created ~/.bash_profile next to ~/.profile"
linux_bash_home=$case_home

# Install again over that home: nothing is asked or added.
reset_path_case
P_OS=Linux P_SHELL=/bin/bash P_TTY='y'
name=path-linux-bash-again
mkdir -p "$work_dir/$name/home"
cp "$linux_bash_home/.profile" "$linux_bash_home/.bashrc" "$work_dir/$name/home/"
run_path_case "$name"
path_expect_status 0
path_expect_text '~/.profile and ~/.bashrc already add ~/.local/bin to PATH; new terminals find axocoatl'
path_expect_no_prompt
cmp -s "$linux_bash_home/.bashrc" "$case_home/.bashrc" || fail "$name changed ~/.bashrc again"
cmp -s "$linux_bash_home/.profile" "$case_home/.profile" || fail "$name changed ~/.profile"
path_reinstall_twice 1 .bashrc
[ "$(installer_lines "$case_home/.profile")" = 0 ] || fail "$name wrote ~/.profile"

# No terminal on Ubuntu: it says ~/.profile has it and prints the ~/.bashrc
# line; added as told, every new shell lists ~/.local/bin once.
reset_path_case
P_OS=Linux P_SHELL=/bin/bash
seed_home path-linux-bash-ubuntu-no-terminal ubuntu
run_path_case path-linux-bash-ubuntu-no-terminal
path_expect_status 0
path_expect_text '~/.profile already adds ~/.local/bin to PATH in a new login'
path_expect_text '~/.local/bin is not on your PATH. For new terminals to find axocoatl, add this line to ~/.bashrc:'
path_expect_text "    $(path_line_for bash-nonlogin)"
[ "$(installer_lines "$case_home/.bashrc")" = 0 ] || fail "$name wrote ~/.bashrc without being asked"
printf '\n%s\n' "$(path_line_for bash-nonlogin)" >>"$case_home/.bashrc"
P_LINE="$(path_line_for bash-nonlogin)"
path_expect_new_shells_find bash "$case_home/.bashrc"

# A login shell that reads ~/.profile through ~/.bash_profile: the same.
reset_path_case
P_OS=Linux P_SHELL=/bin/bash P_TTY='y'
seed_home path-linux-bash-profile-via-bash-profile bash-profile-reads-profile
run_path_case path-linux-bash-profile-via-bash-profile
path_expect_status 0
path_expect_text '~/.profile already adds ~/.local/bin to PATH in a new login'
grep -F 'Add it in ~/.bashrc? [Y/n]' "$case_tty" >/dev/null \
  || fail "$name did not ask about ~/.bashrc alone: $(cat "$case_tty")"
[ "$(installer_lines "$case_home/.bash_profile")" = 0 ] || fail "$name wrote ~/.bash_profile, which reads ~/.profile"
[ "$(installer_lines "$case_home/.profile")" = 0 ] || fail "$name wrote ~/.profile, which adds ~/.local/bin already"
P_LINE="$(path_line_for bash-nonlogin)"
path_expect_new_shells_find bash "$case_home/.bashrc"

# The same with `. "$HOME"/.profile` and with ~/.profile's full path in
# ~/.bash_profile: an interactive login shell (bash -ilc, as SSH starts)
# reads ~/.bash_profile, ~/.profile, ~/.bashrc and ~/.profile's own lines, and
# lists ~/.local/bin once.
for layout in bash-profile-reads-profile-quoted bash-profile-reads-profile-full-path; do
  reset_path_case
  P_OS=Linux P_SHELL=/bin/bash P_TTY='y'
  seed_home "path-linux-$layout" "$layout"
  run_path_case "path-linux-$layout"
  path_expect_status 0
  path_expect_text '~/.profile already adds ~/.local/bin to PATH in a new login'
  grep -F 'Add it in ~/.bashrc? [Y/n]' "$case_tty" >/dev/null \
    || fail "$name did not ask about ~/.bashrc alone: $(cat "$case_tty")"
  [ "$(installer_lines "$case_home/.bash_profile")" = 0 ] || fail "$name wrote ~/.bash_profile, which reads ~/.profile"
  P_LINE="$(path_line_for bash-nonlogin)"
  path_expect_new_shells_find bash "$case_home/.bashrc"
done

# A ~/.bash_profile that reads ~/.profile only on another OS: a login shell
# here reads ~/.bash_profile alone, so it gets the line too.
reset_path_case
P_OS=Linux P_SHELL=/bin/bash P_TTY='y'
seed_home path-linux-bash-profile-elsewhere bash-profile-reads-profile-elsewhere
run_path_case path-linux-bash-profile-elsewhere
path_expect_status 0
path_refuse_text 'already adds'
grep -F 'Add it in ~/.bashrc and ~/.bash_profile? [Y/n]' "$case_tty" >/dev/null \
  || fail "$name did not name ~/.bashrc and ~/.bash_profile in its question: $(cat "$case_tty")"
path_expect_new_shells_find bash "$case_home/.bashrc" "$case_home/.bash_profile"

# A ~/.profile whose ~/.local/bin lines are only for another OS: it does not
# count, and gets the line with ~/.bashrc.
reset_path_case
P_OS=Linux P_SHELL=/bin/bash P_TTY='y'
seed_home path-linux-bash-ubuntu-elsewhere ubuntu-elsewhere
run_path_case path-linux-bash-ubuntu-elsewhere
path_expect_status 0
path_refuse_text 'already adds'
grep -F 'Add it in ~/.bashrc and ~/.profile? [Y/n]' "$case_tty" >/dev/null \
  || fail "$name did not name ~/.bashrc and ~/.profile in its question: $(cat "$case_tty")"
path_expect_new_shells_find bash "$case_home/.bashrc" "$case_home/.profile"

# A ~/.profile that reads ~/.bashrc and does not add ~/.local/bin: the line
# goes into both, and a login shell, which reads both, lists it once.
reset_path_case
P_OS=Linux P_SHELL=/bin/bash P_TTY='y'
seed_home path-linux-bash-reads-bashrc reads-bashrc
run_path_case path-linux-bash-reads-bashrc
path_expect_status 0
grep -F 'Add it in ~/.bashrc and ~/.profile? [Y/n]' "$case_tty" >/dev/null \
  || fail "$name did not name ~/.bashrc and ~/.profile in its question: $(cat "$case_tty")"
path_expect_new_shells_find bash "$case_home/.bashrc" "$case_home/.profile"
grep -F '# like Ubuntu: bash reads .bashrc' "$case_home/.profile" >/dev/null || fail "$name lost the existing ~/.profile"

# A home where only ~/.bashrc has a line (as an earlier installer left it)
# and ~/.profile does not add ~/.local/bin: only ~/.profile is asked about
# and written.
reset_path_case
P_OS=Linux P_SHELL=/bin/bash P_TTY='y'
seed_home path-linux-bash-bashrc-only reads-bashrc
printf '\n# Added by the Axocoatl installer\n%s\n' 'export PATH="$HOME/.local/bin:$PATH"' \
  >>"$work_dir/path-linux-bash-bashrc-only/home/.bashrc"
run_path_case path-linux-bash-bashrc-only
path_expect_status 0
path_expect_text '~/.bashrc already adds ~/.local/bin to PATH'
grep -F 'Add it in ~/.profile? [Y/n]' "$case_tty" >/dev/null \
  || fail "$name did not ask about ~/.profile alone: $(cat "$case_tty")"
path_expect_text "added to ~/.profile:  $(path_line_for bash)"
[ "$(installer_lines "$case_home/.bashrc")" = 1 ] || fail "$name added a line to ~/.bashrc again"
path_expect_new_shells_find bash "$case_home/.profile"

# Ubuntu's files with the unguarded line that unreleased 1.5.0 builds wrote
# to ~/.bashrc: it counts, and nothing is added, then or on two more installs.
reset_path_case
P_OS=Linux P_SHELL=/bin/bash P_TTY='y'
seed_home path-linux-bash-ubuntu-old-line ubuntu
printf '\n# Added by the Axocoatl installer\n%s\n' 'export PATH="$HOME/.local/bin:$PATH"' \
  >>"$work_dir/path-linux-bash-ubuntu-old-line/home/.bashrc"
run_path_case path-linux-bash-ubuntu-old-line
path_expect_status 0
path_expect_text '~/.profile and ~/.bashrc already add ~/.local/bin to PATH; new terminals find axocoatl'
path_expect_no_prompt
[ "$(installer_lines "$case_home/.bashrc")" = 1 ] || fail "$name added a line to ~/.bashrc again"
[ "$(installer_lines "$case_home/.profile")" = 0 ] || fail "$name wrote ~/.profile"
path_reinstall_twice 1 .bashrc

# GitHub's Ubuntu runner images: a ~/.bash_profile that reads neither
# ~/.profile nor ~/.bashrc, so a login shell reads only it.
reset_path_case
P_OS=Linux P_SHELL=/bin/bash P_TTY='y'
seed_home path-linux-bash-runner runner
run_path_case path-linux-bash-runner
path_expect_status 0
grep -F 'Add it in ~/.bashrc and ~/.bash_profile? [Y/n]' "$case_tty" >/dev/null \
  || fail "$name did not name ~/.bashrc and ~/.bash_profile in its question: $(cat "$case_tty")"
path_expect_new_shells_find bash "$case_home/.bashrc" "$case_home/.bash_profile"
[ "$(installer_lines "$case_home/.profile")" = 0 ] \
  || fail "$name wrote ~/.profile, which a login bash does not read next to ~/.bash_profile"

# No terminal: the printed line names both files, and adding it to them as
# told puts axocoatl on every new shell's PATH, once.
reset_path_case
P_OS=Linux P_SHELL=/bin/bash
seed_home path-linux-bash-no-terminal runner
run_path_case path-linux-bash-no-terminal
path_expect_status 0
path_expect_text '~/.local/bin is not on your PATH. For new terminals to find axocoatl, add this line to ~/.bashrc and ~/.bash_profile:'
path_expect_text "    $(path_line_for bash)"
[ "$(installer_lines "$case_home/.bashrc")$(installer_lines "$case_home/.bash_profile")" = 00 ] \
  || fail "$name wrote a startup file without being asked"
for profile_file in .bashrc .bash_profile; do
  printf '\n%s\n' "$(path_line_for bash)" >>"$case_home/$profile_file"
done
path_expect_new_shells_find bash "$case_home/.bashrc" "$case_home/.bash_profile"

# Arch's files: ~/.bash_profile reads ~/.bashrc, so an interactive login
# shell runs both lines, and lists ~/.local/bin once.
reset_path_case
P_OS=Linux P_SHELL=/bin/bash P_TTY='y'
seed_home path-linux-bash-arch arch
run_path_case path-linux-bash-arch
path_expect_status 0
grep -F 'Add it in ~/.bashrc and ~/.bash_profile? [Y/n]' "$case_tty" >/dev/null \
  || fail "$name did not name ~/.bashrc and ~/.bash_profile in its question: $(cat "$case_tty")"
path_expect_new_shells_find bash "$case_home/.bashrc" "$case_home/.bash_profile"

# Fedora's files: ~/.bashrc adds ~/.local/bin itself, in its own way, which
# the installer does not take as adding it; both files get the line, which
# does nothing after Fedora's, and every new shell lists ~/.local/bin once.
reset_path_case
P_OS=Linux P_SHELL=/bin/bash P_TTY='y'
seed_home path-linux-bash-fedora fedora
run_path_case path-linux-bash-fedora
path_expect_status 0
path_refuse_text 'already adds'
grep -F 'Add it in ~/.bashrc and ~/.bash_profile? [Y/n]' "$case_tty" >/dev/null \
  || fail "$name did not name ~/.bashrc and ~/.bash_profile in its question: $(cat "$case_tty")"
grep -F 'PATH="$HOME/.local/bin:$HOME/bin:$PATH"' "$case_home/.bashrc" >/dev/null || fail "$name lost Fedora's ~/.bashrc"
path_expect_new_shells_find bash "$case_home/.bashrc" "$case_home/.bash_profile"

# Any other shell next to Ubuntu's ~/.profile, which adds ~/.local/bin: it
# says so, and adds nothing.
reset_path_case
P_SHELL=/bin/ksh P_OS=Linux P_TTY='y'
seed_home path-other-shell-ubuntu ubuntu
cp "$work_dir/path-other-shell-ubuntu/home/.profile" "$work_dir/other-shell-profile-before"
run_path_case path-other-shell-ubuntu
path_expect_status 0
path_expect_text '~/.profile already adds ~/.local/bin to PATH; new terminals find axocoatl'
path_expect_no_prompt
cmp -s "$work_dir/other-shell-profile-before" "$case_home/.profile" || fail "$name changed ~/.profile"
found_binary="$(env -i HOME="$case_home" PATH=/usr/bin:/bin /bin/sh -c '. "$HOME/.profile" >/dev/null 2>&1; echo "PATH=$PATH"; command -v axocoatl')"
path_expect_once "~/.profile read with sh" "$found_binary" "$case_home/.profile"

# Linux zsh: a new terminal reads ~/.zshrc and a new login ~/.zprofile.
reset_path_case
P_OS=Linux P_SHELL=/usr/bin/zsh P_TTY='Y'
run_path_case path-linux-zsh
path_expect_status 0
grep -F 'Add it in ~/.zshrc and ~/.zprofile? [Y/n]' "$case_tty" >/dev/null \
  || fail "$name did not name ~/.zshrc and ~/.zprofile in its question: $(cat "$case_tty")"
path_expect_new_shells_find zsh "$case_home/.zshrc" "$case_home/.zprofile"

# zsh with ZDOTDIR (set in ~/.zshenv): the files in that directory.
reset_path_case
P_SHELL=/bin/zsh P_TTY='y'
name=path-mac-zsh-zdotdir
mkdir -p "$work_dir/$name/home/.config/zsh"
printf '%s\n' 'export ZDOTDIR="$HOME/.config/zsh"' >"$work_dir/$name/home/.zshenv"
P_ZDOTDIR="$work_dir/$name/home/.config/zsh"
run_path_case "$name"
path_expect_status 0
grep -F 'Add it in ~/.config/zsh/.zprofile? [Y/n]' "$case_tty" >/dev/null \
  || fail "$name did not name ~/.config/zsh/.zprofile in its question: $(cat "$case_tty")"
[ ! -e "$case_home/.zprofile" ] || fail "$name wrote ~/.zprofile, which zsh does not read with ZDOTDIR"
path_expect_new_shells_find zsh "$case_home/.config/zsh/.zprofile"

# macOS bash with no startup file: ~/.bash_profile, which Terminal's login
# shell reads.
reset_path_case
P_SHELL=/bin/bash P_TTY='y'
run_path_case path-mac-bash
path_expect_status 0
path_expect_new_shells_find bash "$case_home/.bash_profile"
[ ! -e "$case_home/.bashrc" ] || fail "$name wrote ~/.bashrc on macOS"

# macOS bash with only ~/.profile: a new ~/.bash_profile would hide it, so the
# line goes into ~/.profile.
reset_path_case
P_SHELL=/bin/bash P_TTY=''
name=path-mac-bash-profile
mkdir -p "$work_dir/$name/home"
printf '%s\n' '# existing profile' > "$work_dir/$name/home/.profile"
run_path_case "$name"
path_expect_status 0
path_expect_new_shells_find bash "$case_home/.profile"
[ ! -e "$case_home/.bash_profile" ] || fail "$name created ~/.bash_profile next to ~/.profile"
grep -F '# existing profile' "$case_home/.profile" >/dev/null || fail "$name lost the existing ~/.profile"

# fish gets fish_add_path in its config, on macOS and on Linux.
for fish_os in Darwin Linux; do
  reset_path_case
  P_OS=$fish_os P_SHELL=/opt/homebrew/bin/fish P_TTY='y'
  [ "$fish_os" = Linux ] && P_SHELL=/usr/bin/fish
  run_path_case "path-fish-$fish_os"
  path_expect_status 0
  grep -F 'Add it in ~/.config/fish/config.fish? [Y/n]' "$case_tty" >/dev/null \
    || fail "$name did not name config.fish in its question: $(cat "$case_tty")"
  path_expect_new_shells_find fish "$case_home/.config/fish/config.fish"
  path_expect_next_runs
done

# fish with XDG_CONFIG_HOME: the config.fish under it, which fish reads.
reset_path_case
P_OS=Linux P_SHELL=/usr/bin/fish P_TTY='y'
P_XDG_CONFIG_HOME="$work_dir/path-fish-xdg/xdg config"
run_path_case path-fish-xdg
path_expect_status 0
grep -F "Add it in $P_XDG_CONFIG_HOME/fish/config.fish? [Y/n]" "$case_tty" >/dev/null \
  || fail "$name did not name \$XDG_CONFIG_HOME/fish/config.fish in its question: $(cat "$case_tty")"
[ ! -e "$case_home/.config/fish/config.fish" ] || fail "$name wrote ~/.config/fish/config.fish next to XDG_CONFIG_HOME"
path_expect_new_shells_find fish "$P_XDG_CONFIG_HOME/fish/config.fish"

# fish with functions and blocks before the line: the first install adds it,
# and two more add nothing.
reset_path_case
P_OS=Linux P_SHELL=/usr/bin/fish P_TTY='y'
name=path-fish-reinstall
mkdir -p "$work_dir/$name/home/.config/fish"
printf '%s\n' 'function mkcd' '    mkdir -p $argv; and cd $argv' 'end' 'if status is-interactive' \
  '    switch (uname)' '        case SunOS' '            set -g on_sunos 1' '    end' 'end' \
  >"$work_dir/$name/home/.config/fish/config.fish"
run_path_case "$name"
path_expect_status 0
path_expect_text "added to ~/.config/fish/config.fish:  $(path_line_for fish)"
path_reinstall_twice 1 .config/fish/config.fish
path_expect_new_shells_find fish "$case_home/.config/fish/config.fish"

# The fish line unreleased 1.5.0 builds wrote (without --path) counts:
# nothing is asked or added, then or on two more installs.
reset_path_case
P_OS=Linux P_SHELL=/usr/bin/fish P_TTY='y'
name=path-fish-unreleased-line
mkdir -p "$work_dir/$name/home/.config/fish"
printf '\n# Added by the Axocoatl installer\n%s\n' 'fish_add_path "$HOME/.local/bin"' \
  >"$work_dir/$name/home/.config/fish/config.fish"
run_path_case "$name"
path_expect_status 0
path_expect_text '~/.config/fish/config.fish already adds ~/.local/bin to PATH; new terminals find axocoatl'
path_expect_no_prompt
[ "$(installer_lines "$case_home/.config/fish/config.fish")" = 1 ] || fail "$name added a line to config.fish again"
path_reinstall_twice 1 .config/fish/config.fish

# Any other shell reads ~/.profile.
reset_path_case
P_SHELL=/bin/ksh P_TTY='y'
run_path_case path-other-shell
path_expect_status 0
path_expect_new_shells_find sh "$case_home/.profile"

# A home directory with a space: the printed next command is quoted.
reset_path_case
P_HOME_NAME='home dir'
run_path_case path-home-with-space
path_expect_status 0
path_expect_text "Next:  '$case_home/.local/bin/axocoatl' onboard"
path_expect_next_runs

# Already on PATH: no PATH message, and the next command is the bare name.
reset_path_case
P_SYSTEM=writable P_ON_PATH=yes P_TTY='y'
run_path_case path-already-on-path
path_expect_status 0
path_expect_installed_to "$case_dest"
path_refuse_text 'is not on your PATH'
path_expect_no_prompt
path_expect_text 'Next:  axocoatl onboard'
path_expect_next_runs

# Another axocoatl first on PATH: the next command names the new one.
reset_path_case
P_SYSTEM=writable P_ON_PATH=yes P_SHADOW=yes
run_path_case path-shadowed
path_expect_status 0
path_expect_text "comes first on your PATH, so 'axocoatl' runs that copy"
path_expect_text "Next:  $case_system/axocoatl onboard"
path_expect_next_runs

# A GitHub Actions step: the directory goes into $GITHUB_PATH for the job's
# later steps, and no profile is asked about or written.
reset_path_case
P_GITHUB_PATH=yes P_TTY='y'
run_path_case path-github-actions
path_expect_status 0
[ "$(cat "$case_github_path")" = "$case_home/.local/bin" ] \
  || fail "$name did not add the install directory to GITHUB_PATH: $(cat "$case_github_path")"
path_expect_text "added $case_home/.local/bin to GITHUB_PATH"
path_expect_no_prompt
path_refuse_text 'add this line to'
[ ! -e "$case_home/.zprofile" ] || fail "$name wrote ~/.zprofile in a GitHub Actions step"
found_in_job="$(PATH="$(cat "$case_github_path"):$fake_bin" /bin/sh -c 'command -v axocoatl')"
[ "$found_in_job" = "$case_home/.local/bin/axocoatl" ] \
  || fail "$name: a later step would find '$found_in_job'"
path_expect_next_runs

# AXOCOATL_MODIFY_PATH=no leaves GITHUB_PATH alone too.
reset_path_case
P_GITHUB_PATH=yes P_MODIFY=no
run_path_case path-github-actions-modify-no
path_expect_status 0
[ ! -s "$case_github_path" ] || fail "$name wrote GITHUB_PATH with AXOCOATL_MODIFY_PATH=no"
path_expect_text 'add this line to ~/.zprofile:'

# With Podman on PATH there is no Podman note.
reset_path_case
P_PODMAN=yes
run_path_case path-podman-present
path_expect_status 0
path_refuse_text 'Podman, which every Session and run needs'

# --- Choosing the release ------------------------------------------------
# AXOCOATL_VERSION installs that release without asking GitHub for the latest
# one; a bare version gets its v.
reset_path_case
P_VERSION="$current_version"
run_path_case version-pinned
path_expect_status 0
path_expect_text "installing the requested release $current_tag"
grep -F '/releases/latest' "$curl_log" >/dev/null && fail "$name asked GitHub for the latest release"
grep -Fx "https://github.com/axocoatl/axocoatl/releases/download/$current_tag/axocoatl-$current_tag-aarch64-apple-darwin.tar.gz" "$curl_log" >/dev/null \
  || fail "$name did not download the pinned release: $(cat "$curl_log")"
path_expect_next_runs

# A malformed version, a base URL without a version, and a plain-http base URL
# stop before any download.
for invalid in 'v1.5.0;id' 'latest' '../v1.5.0'; do
  reset_path_case
  P_VERSION="$invalid"
  run_path_case version-invalid
  path_expect_status 1
  path_expect_text "AXOCOATL_VERSION must be a release tag such as v1.5.0, not '$invalid'"
  [ ! -s "$curl_log" ] || fail "$name ($invalid) reached the network stub"
done

reset_path_case
P_BASE_URL='https://mirror.example/axocoatl'
run_path_case base-url-without-version
path_expect_status 1
path_expect_text 'AXOCOATL_INSTALL_BASE_URL needs AXOCOATL_VERSION'
[ ! -s "$curl_log" ] || fail "$name reached the network stub"

reset_path_case
P_VERSION="$current_tag" P_BASE_URL='http://mirror.example/axocoatl'
run_path_case base-url-plain-http
path_expect_status 1
path_expect_text "AXOCOATL_INSTALL_BASE_URL must start with https:// or file://, not 'http://mirror.example/axocoatl'"
[ ! -s "$curl_log" ] || fail "$name reached the network stub"

# A file:// release directory, with the real curl and checksum tools: the
# archive and its .sha256 come from there, and the checksum is still required.
release_dir="$work_dir/local release"
mkdir -p "$release_dir"
local_archive="axocoatl-${current_tag}-aarch64-apple-darwin.tar.gz"
cp "$fixture_archive" "$release_dir/$local_archive"
if command -v sha256sum >/dev/null 2>&1; then
  local_digest="$(sha256sum "$release_dir/$local_archive" | awk '{print $1}')"
else
  local_digest="$(shasum -a 256 "$release_dir/$local_archive" | awk '{print $1}')"
fi
release_url="file://$(printf '%s' "$release_dir" | sed 's/ /%20/g')"
printf '%s  %s\n' "$local_digest" "$local_archive" > "$release_dir/$local_archive.sha256"
reset_path_case
P_VERSION="$current_tag" P_BASE_URL="$release_url/" P_BIN="$real_bin"
run_path_case base-url-file
path_expect_status 0
path_expect_text 'checksum verified'
path_expect_installed_to "$case_home/.local/bin"
cmp -s "$case_home/.local/bin/axocoatl" "$fixture_dir/axocoatl" \
  || fail "$name installed something other than the release directory's binary"
path_expect_next_runs

# The same directory with a wrong checksum is refused.
printf '%064d  %s\n' 0 "$local_archive" > "$release_dir/$local_archive.sha256"
reset_path_case
P_VERSION="$current_tag" P_BASE_URL="$release_url" P_BIN="$real_bin"
run_path_case base-url-file-mismatch
path_expect_status 1
path_expect_text 'checksum mismatch'
[ ! -e "$case_home/.local/bin/axocoatl" ] || fail "$name installed a binary whose checksum did not match"

shells_note="new shells started: ${real_shells:-none}"
[ -z "$missing_shells" ] || shells_note="$shells_note; not installed here, startup files read with sh instead: $missing_shells"
echo "Installer regression contract: PASS ($case_count local simulations, $startup_count startup files read; $shells_note)"
