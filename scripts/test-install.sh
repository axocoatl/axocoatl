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

# A new login shell that reads the profile finds the installed binary.
path_expect_profile_finds() {
  profile_file=$1
  [ -f "$profile_file" ] || fail "$name did not write $profile_file: $(cat "$output")"
  found_binary="$(HOME="$case_home" PATH="$fake_bin" /bin/sh -c '. "$1" && command -v axocoatl' sh "$profile_file")" \
    || fail "$name: $profile_file does not put axocoatl on PATH: $(cat "$profile_file")"
  [ "$found_binary" = "$case_dest/axocoatl" ] \
    || fail "$name: $profile_file finds '$found_binary', not $case_dest/axocoatl"
}

path_expect_no_prompt() {
  if [ -f "$case_tty" ] && grep -F 'is not on your PATH. Add it in' "$case_tty" >/dev/null; then
    fail "$name asked on the terminal: $(cat "$case_tty")"
  fi
}

reset_path_case() {
  P_OS=Darwin P_SHELL=/bin/zsh P_SYSTEM=missing P_ON_PATH=no P_SHADOW=no P_TTY=none P_MODIFY=ask P_HOME_NAME=home
  P_GITHUB_PATH=no P_PODMAN=no P_VERSION='' P_BASE_URL='' P_BIN=''
}

# A stock Mac in a pipe with no terminal: ~/.local/bin, the persistent line
# printed for ~/.zprofile, nothing written, and the next command a full path.
reset_path_case
run_path_case path-mac-no-terminal
path_expect_status 0
path_expect_installed_to "$case_home/.local/bin"
path_expect_text "~/.local/bin is not on your PATH. For new terminals to find axocoatl, add this line to ~/.zprofile:"
path_expect_text '    export PATH="$HOME/.local/bin:$PATH"'
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
path_expect_text 'added to ~/.zprofile:  export PATH="$HOME/.local/bin:$PATH"'
path_expect_profile_finds "$case_home/.zprofile"
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
path_expect_profile_finds "$case_home/.zprofile"

# An unknown AXOCOATL_MODIFY_PATH stops before any download.
reset_path_case
P_MODIFY=maybe
run_path_case path-modify-invalid
path_expect_status 1
path_expect_text "AXOCOATL_MODIFY_PATH must be ask, yes or no, not 'maybe'"
[ ! -s "$curl_log" ] || fail "$name reached the network stub"

# Linux bash reads ~/.bashrc in a new terminal.
reset_path_case
P_OS=Linux P_SHELL=/bin/bash P_TTY='yes'
run_path_case path-linux-bash
path_expect_status 0
path_expect_profile_finds "$case_home/.bashrc"
[ ! -e "$case_home/.bash_profile" ] || fail "$name wrote ~/.bash_profile on Linux"

# Linux zsh reads ~/.zshrc.
reset_path_case
P_OS=Linux P_SHELL=/usr/bin/zsh P_TTY='Y'
run_path_case path-linux-zsh
path_expect_status 0
path_expect_profile_finds "$case_home/.zshrc"

# macOS bash with only ~/.profile: a new ~/.bash_profile would hide it, so the
# line goes into ~/.profile.
reset_path_case
P_SHELL=/bin/bash P_TTY=''
name=path-mac-bash-profile
mkdir -p "$work_dir/$name/home"
printf '%s\n' '# existing profile' > "$work_dir/$name/home/.profile"
run_path_case "$name"
path_expect_status 0
path_expect_profile_finds "$case_home/.profile"
[ ! -e "$case_home/.bash_profile" ] || fail "$name created ~/.bash_profile next to ~/.profile"
grep -F '# existing profile' "$case_home/.profile" >/dev/null || fail "$name lost the existing ~/.profile"

# fish gets fish_add_path in its config.
reset_path_case
P_SHELL=/opt/homebrew/bin/fish P_TTY='y'
run_path_case path-fish
path_expect_status 0
grep -F 'Add it in ~/.config/fish/config.fish? [Y/n]' "$case_tty" >/dev/null \
  || fail "$name did not name config.fish in its question: $(cat "$case_tty")"
grep -F 'fish_add_path "$HOME/.local/bin"' "$case_home/.config/fish/config.fish" >/dev/null \
  || fail "$name did not add fish_add_path to config.fish: $(cat "$output")"
path_expect_next_runs

# Any other shell reads ~/.profile.
reset_path_case
P_SHELL=/bin/ksh P_TTY='y'
run_path_case path-other-shell
path_expect_status 0
path_expect_profile_finds "$case_home/.profile"

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

echo "Installer regression contract: PASS ($case_count local simulations)"
