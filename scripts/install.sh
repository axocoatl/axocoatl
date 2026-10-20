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
# only when PATH does not have it, and a file that already adds it (such as
# Ubuntu's ~/.profile) gets none; then the ~/.bashrc line skips login shells.
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

# Whether startup file $1 already puts the install directory ($dest) on PATH:
# a line that is not a comment names the directory ($HOME/..., ${HOME}/...,
# ~/... or its full path) and sets PATH (or zsh's path, or fish_add_path).
# Ubuntu's and Debian's ~/.profile have one for ~/.local/bin; so does a line
# an earlier installer wrote.
adds_to_path() {
  [ -f "$1" ] || return 1
  case "$dest" in
    "$HOME"/*) dest_rel="${dest#"$HOME"/}" ;;
    *) dest_rel="" ;;
  esac
  awk -v abs="$dest" -v rel="$dest_rel" '
    # Whether text holds name, followed by an end of the directory name.
    function names(text, name,    at, next_char) {
      while ((at = index(text, name)) > 0) {
        next_char = substr(text, at + length(name), 1)
        if (next_char == "" || next_char !~ /[A-Za-z0-9_.-]/) return 1
        text = substr(text, at + 1)
      }
      return 0
    }
    {
      line = $0
      sub(/^[ \t]+/, "", line)
      if (substr(line, 1, 1) == "#") next
      if (line !~ /PATH|path\+?=|fish_add_path|fish_user_paths/) next
      if (names(line, abs)) { found = 1; exit }
      if (rel != "" && (names(line, "$HOME/" rel) || names(line, "${HOME}/" rel) || names(line, "~/" rel))) { found = 1; exit }
    }
    END { exit !found }
  ' "$1" 2>/dev/null
}

# Whether bash login file $1 (~/.bash_profile or ~/.bash_login) reads
# ~/.profile with `.` or `source`.
reads_dot_profile() {
  [ -f "$1" ] || return 1
  awk '
    {
      line = $0
      sub(/^[ \t]+/, "", line)
      if (substr(line, 1, 1) == "#") next
      if (line ~ /(^|[^A-Za-z0-9_])(\.|source)[ \t]+["'\'']?(~|\$HOME|\$\{HOME\})\/\.profile(["'\'' \t;]|$)/) { found = 1; exit }
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
  now_line="export PATH=\"${dir_expr}:\$PATH\""
  path_line="case \":\$PATH:\" in *\":${dir_expr}:\"*) ;; *) ${now_line} ;; esac"
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
          path_line="shopt -q login_shell || ${path_line}"
        else
          profiles="$HOME/.bashrc
${login_file}"
        fi
      fi
      ;;
    fish)
      # fish reads config.fish in every shell, login or not, and
      # fish_add_path adds nothing PATH already has.
      profiles="${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish"
      path_line="fish_add_path \"${dir_expr}\""
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
