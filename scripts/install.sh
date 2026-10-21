#!/bin/sh
# Axocoatl installer — downloads a prebuilt binary from GitHub Releases.
# Usage: curl -fsSL https://axocoatl.ai/install.sh | sh
#
# It installs to /usr/local/bin when that directory is writable, else to
# ~/.local/bin. When the directory is not on PATH it asks, on the terminal,
# before adding one line to the startup files your shell reads in a new
# terminal and a new login (~/.zprofile for zsh on macOS; ~/.zshrc and
# ~/.zprofile for zsh on Linux; for bash the login file that exists, else
# ~/.bash_profile on macOS and ~/.profile on Linux, and on Linux ~/.bashrc
# too; fish's config.fish; ~/.profile otherwise). The line adds the directory
# only when PATH does not have it. A file that has this installer's line, or
# the lines Debian's and Ubuntu's ~/.profile use to add ~/.local/bin, gets
# none; next to such a ~/.profile the ~/.bashrc line skips login shells.
# Without a terminal (or when the answer is end of input) it only prints that
# line. In a GitHub Actions step it adds the directory to $GITHUB_PATH
# instead, for the job's later steps.
# AXOCOATL_MODIFY_PATH=yes adds the line without asking;
# AXOCOATL_MODIFY_PATH=no never changes a profile or $GITHUB_PATH.
# AXOCOATL_VERSION=v1.5.0 installs that release instead of the latest one, and
# AXOCOATL_INSTALL_BASE_URL (https:// or file://, with AXOCOATL_VERSION) names
# the directory that holds its archive and .sha256 file instead of the GitHub
# release; the checksum is still required.
# AXOCOATL_INSTALL_SYSTEM_DIR and AXOCOATL_INSTALL_TTY replace /usr/local/bin
# and /dev/tty for scripts/test-install.sh.
set -eu

REPO="axocoatl/axocoatl"
BIN="axocoatl"

err() { echo "axocoatl-install: $*" >&2; exit 1; }
info() { echo "axocoatl-install: $*"; }

modify_path="${AXOCOATL_MODIFY_PATH:-ask}"
case "$modify_path" in
  ask|yes|no) ;;
  *) err "AXOCOATL_MODIFY_PATH must be ask, yes or no, not '$modify_path'" ;;
esac
# A release tag such as v1.5.0 (or 1.5.0), checked before any download.
requested_tag="${AXOCOATL_VERSION:-}"
case "$requested_tag" in
  '') ;;
  *[!A-Za-z0-9.-]*) err "AXOCOATL_VERSION must be a release tag such as v1.5.0, not '$requested_tag'" ;;
  v[0-9]*) ;;
  [0-9]*) requested_tag="v$requested_tag" ;;
  *) err "AXOCOATL_VERSION must be a release tag such as v1.5.0, not '$requested_tag'" ;;
esac
base_url="${AXOCOATL_INSTALL_BASE_URL:-}"
case "$base_url" in
  '') ;;
  https://*|file://*)
    [ -n "$requested_tag" ] \
      || err "AXOCOATL_INSTALL_BASE_URL needs AXOCOATL_VERSION, which names the release to install from it"
    base_url="${base_url%/}"
    ;;
  *) err "AXOCOATL_INSTALL_BASE_URL must start with https:// or file://, not '$base_url'" ;;
esac
system_dir="${AXOCOATL_INSTALL_SYSTEM_DIR:-/usr/local/bin}"
tty_device="${AXOCOATL_INSTALL_TTY:-/dev/tty}"

# A path as the user knows it: ~/... under the home directory.
display_path() {
  case "$1" in
    "$HOME"/*) printf '~/%s' "${1#"$HOME"/}" ;;
    *) printf '%s' "$1" ;;
  esac
}

# A path quoted for a shell command line when it needs quoting.
shell_word() {
  case "$1" in
    *[!A-Za-z0-9/._+:-]*) printf "'%s'" "$1" ;;
    *) printf '%s' "$1" ;;
  esac
}

# The lines that put directory expression $1 ($HOME/... or a full path) on
# PATH: first on PATH for this shell; the same only when PATH does not have it
# yet, for startup files; and fish's, which changes only PATH (without --path,
# fish_add_path also saves the directory in the universal fish_user_paths,
# which outlives the line).
# shellcheck disable=SC2016
export_line() { printf 'export PATH="%s:$PATH"' "$1"; }
# shellcheck disable=SC2016
guarded_line() { printf 'case ":$PATH:" in *":%s:"*) ;; *) export PATH="%s:$PATH" ;; esac' "$1" "$1"; }
fish_line() { printf 'fish_add_path --path "%s"' "$1"; }
# Before the ~/.bashrc line next to a ~/.profile that adds the directory.
nonlogin_prefix='shopt -q login_shell || '

# Every line this installer writes, or a 1.5.0 release candidate wrote, for
# directory expression $1, one per line.
own_lines() {
  export_line "$1"
  echo
  guarded_line "$1"
  echo
  printf '%s' "$nonlogin_prefix"
  guarded_line "$1"
  echo
  printf 'fish_add_path "%s"\n' "$1"
  fish_line "$1"
}

# Awk functions for the checks below, which read a startup file the way the
# shell runs it. track(text) keeps depth, the number of blocks a line is in
# (sh, bash and zsh: if, case, for, while, until, select and { }; fish: if,
# for, while, function, switch and begin, closed by end), so that a line
# counts only at the top level of the file, where it runs whenever the file
# is read, and not inside an OS check, a function or a loop. It sets ended
# after a return or an exit anywhere, branched after an else or elif in a
# top-level block, and lost after a here-document (<<) or a block that closes
# more than it opened. It splits a line into words at blanks, makes ;, & and |
# words of their own, stops at a word that starts with #, and counts a keyword
# only where a command starts (first, after ; & | and after then, do, else,
# a case pattern and the like), so that `zstyle ... for more` opens nothing.
# It is not a shell parser: a keyword in a quoted string can still miscount,
# but what the checks look for is exact, and counts only at depth 0.
# swap(s, from, to) replaces every from in s with to.
startup_awk='
BEGIN { depth = 0; ended = 0; branched = 0; lost = 0 }
function swap(s, from, to,    at, out) {
  if (from == "") return s
  out = ""
  while ((at = index(s, from)) > 0) {
    out = out substr(s, 1, at - 1) to
    s = substr(s, at + length(from))
  }
  return out s
}
function track(text,    count, k, w, words, command) {
  gsub(/[;&|]+/, " ; ", text)
  count = split(text, words, "[ \t]+")
  command = 1
  for (k = 1; k <= count; k++) {
    w = words[k]
    if (w == "") continue
    if (substr(w, 1, 1) == "#") break
    if (index(w, "<<") > 0) lost = 1
    if (w == "return" || w == "exit") ended = 1
    if (w == ";") { command = 1; continue }
    if (fish) {
      if (command && (w == "if" || w == "for" || w == "while" || w == "function" || w == "switch" || w == "begin")) depth++
      else if (command && w == "end") depth--
      command = command && (w == "and" || w == "or" || w == "not" || w == "begin")
      continue
    }
    if (command && (w == "if" || w == "case" || w == "for" || w == "while" || w == "until" || w == "select" || w == "{")) depth++
    else if (command && (w == "fi" || w == "esac" || w == "done" || w == "}")) depth--
    else if (w == "{" && k == count) depth++
    if (command && (w == "else" || w == "elif") && depth == 1) branched = 1
    command = (command && (w == "then" || w == "do" || w == "else" || w == "elif" || w == "if" ||
      w == "while" || w == "until" || w == "!" || w == "{" || w == "(" || w == "time")) || w ~ /\)$/
  }
  if (depth < 0) lost = 1
}
'

# Whether startup file $1 surely puts the install directory ($dest) on PATH:
# at its top level it has a line this installer writes (or a 1.5.0 release
# candidate wrote), or, with no return or exit before them, the lines
# Debian's and Ubuntu's ~/.profile use:
#   if [ -d "$HOME/.local/bin" ] ; then
#       PATH="$HOME/.local/bin:$PATH"
#   fi
# Anything else (a subdirectory, a line that takes the directory out of PATH
# or names it only in a comment, a line behind an OS check) does not count:
# the line this installer adds does nothing when PATH has the directory, so
# adding one too many costs nothing, while counting a file wrongly would leave
# new terminals without axocoatl.
adds_to_path() {
  [ -f "$1" ] || return 1
  case "$dest" in
    "$HOME"/*) dest_rel="${dest#"$HOME"/}" ;;
    *) dest_rel="" ;;
  esac
  case "$1" in
    *.fish) scan_syntax='fish' ;;
    *) scan_syntax='sh' ;;
  esac
  scan_dir="$dest" scan_rel="$dest_rel" scan_syntax="$scan_syntax" \
  scan_lines="$(own_lines @DIR@)" awk "$startup_awk"'
    BEGIN {
      stanza = 0
      found = 0
      fish = (ENVIRON["scan_syntax"] == "fish")
      n = split(ENVIRON["scan_lines"], list, "\n")
      for (i = 1; i <= n; i++) own[list[i]] = 1
    }
    # The line with each way of writing the directory as @DIR@.
    function dir(s) {
      s = swap(s, ENVIRON["scan_dir"], "@DIR@")
      if (ENVIRON["scan_rel"] != "") {
        s = swap(s, "$HOME/" ENVIRON["scan_rel"], "@DIR@")
        s = swap(s, "${HOME}/" ENVIRON["scan_rel"], "@DIR@")
      }
      return s
    }
    {
      raw = dir($0)
      line = raw
      sub(/^[ \t]+/, "", line)
      sub(/[ \t]+$/, "", line)
      if (depth == 0 && !lost && (line in own)) { found = 1; exit }
      if (stanza == 2 && raw ~ /^fi[ \t]*$/ && !ended && !lost) { found = 1; exit }
      if (stanza == 1 && line ~ /^(export[ \t]+)?PATH="?@DIR@:\$\{?PATH\}?"?$/) stanza = 2
      else stanza = 0
      if (stanza == 0 && depth == 0 && !ended && !lost &&
        raw ~ /^if[ \t]+\[[ \t]+-d[ \t]+"?@DIR@"?[ \t]+\][ \t]*;[ \t]*then[ \t]*$/) stanza = 1
      track(line)
    }
    END { exit !found }
  ' "$1" 2>/dev/null
}

# Whether bash login file $1 (~/.bash_profile or ~/.bash_login) reads
# ~/.profile (written ~/.profile, $HOME/.profile, ${HOME}/.profile, with
# double quotes around all or part of it, or its full path), at its top level
# with no return or exit before: `. FILE` or `source FILE`, alone, after
# `[ -f FILE ] &&` (or [[ ]], test, -r, -e, -s), or in the then branch of
# `if [ -f FILE ]; then`.
reads_dot_profile() {
  [ -f "$1" ] || return 1
  scan_profile="$HOME/.profile" awk "$startup_awk"'
    BEGIN {
      p = "(~|\\$HOME)/\\.profile"
      src = "(\\.|source)[ \t]+" p
      exists = "(\\[[ \t]+-[efrs][ \t]+" p "[ \t]+\\]|\\[\\[[ \t]+-[efrs][ \t]+" p "[ \t]+\\]\\]|test[ \t]+-[efrs][ \t]+" p ")"
      tail = "[ \t]*;?[ \t]*(#.*)?$"
      alone = "^" src tail
      guarded = "^" exists "[ \t]*&&[ \t]*" src tail
      one_if = "^if[ \t]+" exists "[ \t]*;[ \t]*then[ \t]+" src "[ \t]*;[ \t]*fi" tail
      open_if = "^if[ \t]+" exists "[ \t]*;[ \t]*then" tail
      abs = ENVIRON["scan_profile"]
      opened = 0
      found = 0
    }
    {
      line = swap($0, "'\''" abs "'\''", "$HOME/.profile")
      line = swap(line, abs, "$HOME/.profile")
      line = swap(line, "${HOME}", "$HOME")
      # A quoted ~ is not the home directory; other double quotes go.
      gsub(/["'\'']~/, "@QUOTED@", line)
      gsub(/"/, "", line)
      sub(/^[ \t]+/, "", line)
      sub(/[ \t]+$/, "", line)
      if (!ended && !lost) {
        if (depth == 0 && (line ~ alone || line ~ guarded || line ~ one_if)) { found = 1; exit }
        if (depth == 1 && opened && !branched && line ~ alone) { found = 1; exit }
      }
      was = depth
      track(line)
      if (was == 0 && depth == 1) { opened = (line ~ open_if); branched = 0 }
    }
    END { exit !found }
  ' "$1" 2>/dev/null
}

# The startup files a new shell of the user's login shell reads, one per
# line, and the line that puts $1 on PATH in them. Sets profiles,
# path_line (for those files), now_line (for this shell) and login_covers
# (a bash login file that puts $1 on PATH already, or empty).
# Terminal on macOS starts login shells. On Linux a new terminal window
# starts an interactive shell (.bashrc, .zshrc) and a new login (SSH, the
# console, the desktop session) a login shell, which reads only its login
# files (.bash_profile, .bash_login or .profile; .zprofile), so the line goes
# into both. The line adds the directory only when PATH does not have it
# yet: a login zsh reads .zprofile and then .zshrc, and a login bash's
# .profile or .bash_profile often reads .bashrc, and neither should list it
# twice.
choose_profiles() {
  case "$1" in
    "$HOME"/*) dir_expr="\$HOME/${1#"$HOME"/}" ;;
    *) dir_expr="$1" ;;
  esac
  user_shell="${SHELL:-}"
  user_shell="${user_shell##*/}"
  now_line="$(export_line "$dir_expr")"
  path_line="$(guarded_line "$dir_expr")"
  login_covers=""
  case "$user_shell" in
    zsh)
      if [ "$os" = "Darwin" ]; then
        profiles="${ZDOTDIR:-$HOME}/.zprofile"
      else
        profiles="${ZDOTDIR:-$HOME}/.zshrc
${ZDOTDIR:-$HOME}/.zprofile"
      fi
      ;;
    bash)
      # A login bash reads only the first of these that exists, so a new
      # .bash_profile would hide an existing .profile.
      login_file=""
      for candidate in "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.profile"; do
        if [ -f "$candidate" ]; then
          login_file="$candidate"
          break
        fi
      done
      if [ "$os" = "Darwin" ]; then
        profiles="${login_file:-$HOME/.bash_profile}"
      else
        login_file="${login_file:-$HOME/.profile}"
        if adds_to_path "$login_file"; then
          login_covers="$login_file"
        elif [ "$login_file" != "$HOME/.profile" ] && reads_dot_profile "$login_file" \
          && adds_to_path "$HOME/.profile"; then
          login_covers="$HOME/.profile"
        fi
        if [ -n "$login_covers" ]; then
          # Ubuntu's and Debian's ~/.profile read ~/.bashrc and then add
          # ~/.local/bin whether or not PATH has it, so a line in ~/.bashrc
          # that ran in a login shell would list it twice: there it runs
          # only in a shell that is not a login shell (a new terminal
          # window whose session started before ~/.local/bin existed).
          profiles="$HOME/.bashrc"
          path_line="${nonlogin_prefix}${path_line}"
        else
          profiles="$HOME/.bashrc
${login_file}"
        fi
      fi
      ;;
    fish)
      # fish reads config.fish in every shell, login or not, and
      # fish_add_path adds nothing PATH already has; with --path it changes
      # only this shell's PATH and saves nothing (fish 3.2 or later).
      profiles="${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish"
      path_line="$(fish_line "$dir_expr")"
      now_line=$path_line
      ;;
    *)
      profiles="$HOME/.profile"
      ;;
  esac
}

# Paths, one per line, as the user knows them: "~/.a" or "~/.a and ~/.b".
display_paths() {
  shown=""
  old_ifs=$IFS
  IFS='
'
  set -f
  for each in $1; do
    shown="${shown}${shown:+ and }$(display_path "$each")"
  done
  set +f
  IFS=$old_ifs
  printf '%s' "$shown"
}

# Whether a question can be asked: the terminal opens for reading and writing.
can_ask() {
  ( exec <"$tty_device" >>"$tty_device" ) 2>/dev/null
}

require_supported_glibc() {
  [ "$os" = "Linux" ] || return 0
  command -v getconf >/dev/null 2>&1 \
    || err "prebuilt Linux releases require GNU libc 2.35 or newer; this system cannot report its libc version. Alpine/musl needs a source build"

  libc_version="$(getconf GNU_LIBC_VERSION 2>/dev/null || true)"
  case "$libc_version" in
    glibc\ *) version="${libc_version#glibc }" ;;
    *) err "prebuilt Linux releases require GNU libc 2.35 or newer; this system reported '$libc_version'. Alpine/musl needs a source build" ;;
  esac

  major="${version%%.*}"
  remainder="${version#*.}"
  [ "$remainder" != "$version" ] || err "could not parse GNU libc version '$version'"
  minor="${remainder%%.*}"
  case "$major" in ''|*[!0-9]*) err "could not parse GNU libc version '$version'" ;; esac
  case "$minor" in ''|*[!0-9]*) err "could not parse GNU libc version '$version'" ;; esac

  if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 35 ]; }; then
    err "prebuilt Linux releases require GNU libc 2.35 or newer; detected $libc_version. Upgrade the distribution or build from source"
  fi
}

require_supported_macos() {
  [ "$os" = "Darwin" ] || return 0
  command -v sw_vers >/dev/null 2>&1 \
    || err "prebuilt macOS releases require macOS 11 or newer; this system cannot report its macOS version"

  macos_version="$(SYSTEM_VERSION_COMPAT=0 sw_vers -productVersion 2>/dev/null || true)"
  [ -n "$macos_version" ] \
    || err "prebuilt macOS releases require macOS 11 or newer; this system cannot report its macOS version"
  macos_major="${macos_version%%.*}"
  case "$macos_major" in
    ''|*[!0-9]*) err "could not parse macOS version '$macos_version'" ;;
  esac

  if [ "$macos_major" -lt 11 ]; then
    err "prebuilt macOS releases require macOS 11 or newer; detected macOS $macos_version. Upgrade macOS before installing this release"
  fi
}

# --- Detect OS / arch -> release target triple ---
os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Linux)  os_part="unknown-linux-gnu" ;;
  Darwin) os_part="apple-darwin" ;;
  MINGW*|MSYS*|CYGWIN*|Windows_NT)
    err "Axocoatl runs on Windows inside WSL2, not natively (its session sandbox
  is Podman and its service is systemd/launchd); WSL2 is not yet verified. Open a
  WSL2 distro (e.g. Ubuntu) and run this same command there:

      curl -fsSL https://axocoatl.ai/install.sh | sh

  No WSL2 yet? In an admin PowerShell:  wsl --install  (then reboot).
  Full guide: https://docs.axocoatl.ai/start/install/#windows-through-wsl2" ;;
  *) err "unsupported OS '$os' — use 'cargo install axocoatl-cli' or build from source" ;;
esac

case "$arch" in
  x86_64|amd64)  arch_part="x86_64" ;;
  arm64|aarch64) arch_part="aarch64" ;;
  *) err "unsupported architecture '$arch'" ;;
esac

require_supported_glibc
require_supported_macos
target="${arch_part}-${os_part}"

# --- Resolve the release tag ---
if [ -n "$requested_tag" ]; then
  tag="$requested_tag"
  info "installing the requested release ${tag}..."
else
  info "resolving latest release..."
  tag="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep '"tag_name"' | head -n1 | cut -d'"' -f4)"
  [ -n "$tag" ] || err "could not resolve latest release tag"
fi

tarball="${BIN}-${tag}-${target}.tar.gz"
url="${base_url:-https://github.com/${REPO}/releases/download/${tag}}/${tarball}"
sha_url="${url}.sha256"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

info "downloading ${tarball} (${tag})..."
curl -fsSL "$url" -o "${tmp}/${tarball}" || err "download failed: $url"

# --- Require and verify the release checksum ---
curl -fsSL "$sha_url" -o "${tmp}/sha" \
  || err "checksum download failed: $sha_url"

if ! awk -v expected_name="$tarball" '
  NR != 1 { exit 1 }
  NF != 2 { exit 1 }
  length($1) != 64 { exit 1 }
  $1 !~ /^[0-9A-Fa-f]+$/ { exit 1 }
  $2 != expected_name { exit 1 }
  END { if (NR != 1) exit 1 }
' "${tmp}/sha"; then
  err "malformed checksum file for $tarball"
fi

expected="$(awk '{print $1}' "${tmp}/sha" | tr 'A-F' 'a-f')"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "${tmp}/${tarball}" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "${tmp}/${tarball}" | awk '{print $1}')"
else
  err "neither sha256sum nor shasum is available"
fi
actual="$(printf '%s' "$actual" | tr 'A-F' 'a-f')"
[ "$expected" = "$actual" ] \
  || err "checksum mismatch (expected $expected, got $actual)"
info "checksum verified"

tar -xzf "${tmp}/${tarball}" -C "$tmp"

# --- Choose install dir ---
if [ -w "$system_dir" ]; then
  dest="$system_dir"
else
  dest="${HOME}/.local/bin"
  mkdir -p "$dest"
fi

install -m 0755 "${tmp}/${BIN}" "${dest}/${BIN}" 2>/dev/null \
  || { cp "${tmp}/${BIN}" "${dest}/${BIN}" && chmod 0755 "${dest}/${BIN}"; }

info "installed ${BIN} ${tag} -> ${dest}/${BIN}"

# --- Keep it on PATH for new terminals ---
# A stock macOS PATH (/etc/paths) has no ~/.local/bin, and an `export` typed
# in this shell is gone in the next Terminal window.
offer_profile_line() {
  choose_profiles "$dest"
  shown_dest="$(display_path "$dest")"
  # The startup files that do not put it on PATH yet, and those that do.
  missing=""
  present="$login_covers"
  old_ifs=$IFS
  IFS='
'
  set -f
  for profile in $profiles; do
    if adds_to_path "$profile"; then
      present="${present}${present:+
}${profile}"
    else
      missing="${missing}${missing:+
}${profile}"
    fi
  done
  set +f
  IFS=$old_ifs
  case "$present" in
    '') ;;
    *'
'*) present_note="$(display_paths "$present") already add ${shown_dest} to PATH" ;;
    *) present_note="$(display_paths "$present") already adds ${shown_dest} to PATH" ;;
  esac
  if [ -z "$missing" ]; then
    info "${present_note}; new terminals find ${BIN}"
    return 0
  fi
  if [ -n "$login_covers" ] && [ "$present" = "$login_covers" ]; then
    info "${present_note} in a new login"
  elif [ -n "$present" ]; then
    info "${present_note}"
  fi
  answer=no
  if [ "$modify_path" = "yes" ]; then
    answer=yes
  elif [ "$modify_path" = "ask" ] && can_ask; then
    printf 'axocoatl-install: %s is not on your PATH. Add it in %s? [Y/n] ' \
      "$shown_dest" "$(display_paths "$missing")" >>"$tty_device"
    reply=""
    if read -r reply <"$tty_device"; then
      case "$reply" in
        ''|y|Y|yes|Yes|YES) answer=yes ;;
        *) answer=no ;;
      esac
    else
      # End of input (Ctrl-D, or a terminal that went away) is not consent.
      printf '\n' 2>/dev/null >>"$tty_device" || true
    fi
  fi
  if [ "$answer" = "yes" ]; then
    written=""
    failed=""
    old_ifs=$IFS
    IFS='
'
    set -f
    for profile in $missing; do
      if mkdir -p "${profile%/*}" 2>/dev/null \
        && printf '\n# Added by the Axocoatl installer\n%s\n' "$path_line" 2>/dev/null >>"$profile"; then
        written="${written}${written:+
}${profile}"
      else
        failed="${failed}${failed:+
}${profile}"
      fi
    done
    set +f
    IFS=$old_ifs
    [ -z "$written" ] || info "added to $(display_paths "$written"):  ${path_line}"
    if [ -z "$failed" ]; then
      info "new terminals find ${BIN}; in this one, run:  ${now_line}"
      return 0
    fi
    info "could not write $(display_paths "$failed")"
    missing=$failed
  fi
  info "${shown_dest} is not on your PATH. For new terminals to find ${BIN}, add this line to $(display_paths "$missing"):"
  echo
  echo "    ${path_line}"
}

case ":${PATH}:" in
  *":${dest}:"*) ;;
  *)
    if [ -n "${GITHUB_PATH:-}" ] && [ "$modify_path" != "no" ]; then
      # A GitHub Actions step: the job's later steps read $GITHUB_PATH, and
      # there is no profile or new terminal to set up.
      if printf '%s\n' "$dest" 2>/dev/null >>"$GITHUB_PATH"; then
        info "added ${dest} to GITHUB_PATH; the job's later steps find ${BIN}"
      else
        info "could not write GITHUB_PATH; add ${dest} to PATH in later steps"
      fi
    else
      offer_profile_line
    fi
    ;;
esac

# The next commands must run as printed: by name only when this shell
# resolves the name to the binary just installed.
found="$(command -v "$BIN" 2>/dev/null || true)"
if [ "$found" = "${dest}/${BIN}" ]; then
  run="$BIN"
else
  run="$(shell_word "${dest}/${BIN}")"
  if [ -n "$found" ]; then
    info "note: ${found} comes first on your PATH, so '${BIN}' runs that copy; the commands below name the one just installed"
  fi
fi

# Every Session and run needs rootless Podman; onboarding sets it up.
if ! command -v podman >/dev/null 2>&1; then
  info "Podman, which every Session and run needs, is not installed yet; onboard (below) sets it up or says how, asking first"
fi

echo
echo "Next:  ${run} onboard      # set up Podman, a writer and a reviewer for this user, asking first"
echo "       ${run} doctor       # check the sandbox, the models and the daemon"
echo "       ${run} dev          # start the daemon; leave it running"
