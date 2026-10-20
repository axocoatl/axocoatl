#!/bin/sh
# Axocoatl installer — downloads a prebuilt binary from GitHub Releases.
# Usage: curl -fsSL https://axocoatl.ai/install.sh | sh
#
# It installs to /usr/local/bin when that directory is writable, else to
# ~/.local/bin. When the directory is not on PATH it asks, on the terminal,
# before adding one line to your shell's profile; without a terminal it only
# prints that line. AXOCOATL_MODIFY_PATH=yes adds the line without asking;
# AXOCOATL_MODIFY_PATH=no never changes a profile.
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

# The profile file the user's login shell reads, and the line that puts
# $1 on PATH in it. Sets profile and path_line.
choose_profile() {
  case "$1" in
    "$HOME"/*) dir_expr="\$HOME/${1#"$HOME"/}" ;;
    *) dir_expr="$1" ;;
  esac
  user_shell="${SHELL:-}"
  user_shell="${user_shell##*/}"
  path_line="export PATH=\"${dir_expr}:\$PATH\""
  case "$user_shell" in
    zsh)
      # Terminal on macOS starts login shells (.zprofile); Linux terminals
      # start interactive ones (.zshrc).
      if [ "$os" = "Darwin" ]; then
        profile="${ZDOTDIR:-$HOME}/.zprofile"
      else
        profile="${ZDOTDIR:-$HOME}/.zshrc"
      fi
      ;;
    bash)
      if [ "$os" = "Darwin" ]; then
        # A login bash reads only the first of these that exists, so a new
        # .bash_profile would hide an existing .profile.
        profile="$HOME/.bash_profile"
        for candidate in "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.profile"; do
          if [ -f "$candidate" ]; then
            profile="$candidate"
            break
          fi
        done
      else
        profile="$HOME/.bashrc"
      fi
      ;;
    fish)
      profile="${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish"
      path_line="fish_add_path \"${dir_expr}\""
      ;;
    *)
      profile="$HOME/.profile"
      ;;
  esac
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
    err "Axocoatl runs on Windows through WSL2, not natively (its session sandbox
  is Podman and its service is systemd/launchd). Open a WSL2 distro (e.g. Ubuntu)
  and run this same command there:

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

# --- Resolve latest release tag ---
info "resolving latest release..."
tag="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
  | grep '"tag_name"' | head -n1 | cut -d'"' -f4)"
[ -n "$tag" ] || err "could not resolve latest release tag"

tarball="${BIN}-${tag}-${target}.tar.gz"
url="https://github.com/${REPO}/releases/download/${tag}/${tarball}"
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
case ":${PATH}:" in
  *":${dest}:"*) ;;
  *)
    choose_profile "$dest"
    shown_dest="$(display_path "$dest")"
    shown_profile="$(display_path "$profile")"
    if [ -f "$profile" ] && grep -Fxq -- "$path_line" "$profile" 2>/dev/null; then
      info "${shown_profile} already adds ${shown_dest} to PATH; new terminals find ${BIN}"
    else
      answer=no
      if [ "$modify_path" = "yes" ]; then
        answer=yes
      elif [ "$modify_path" = "ask" ] && can_ask; then
        printf 'axocoatl-install: %s is not on your PATH. Add it in %s? [Y/n] ' \
          "$shown_dest" "$shown_profile" >>"$tty_device"
        reply=""
        read -r reply <"$tty_device" || true
        case "$reply" in
          ''|y|Y|yes|Yes|YES) answer=yes ;;
          *) answer=no ;;
        esac
      fi
      if [ "$answer" = "yes" ] \
        && mkdir -p "${profile%/*}" 2>/dev/null \
        && printf '\n# Added by the Axocoatl installer\n%s\n' "$path_line" 2>/dev/null >>"$profile"; then
        info "added to ${shown_profile}:  ${path_line}"
        info "new terminals find ${BIN}; in this one, run:  ${path_line}"
      else
        [ "$answer" = "no" ] || info "could not write ${shown_profile}"
        info "${shown_dest} is not on your PATH. For new terminals to find ${BIN}, add this line to ${shown_profile}:"
        echo
        echo "    ${path_line}"
      fi
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

# In WSL2 a fresh distro has no Podman, which sandboxed directory sessions need.
if grep -qiE 'microsoft|wsl' /proc/version 2>/dev/null && ! command -v podman >/dev/null 2>&1; then
  info "WSL detected — install Podman for sandboxed sessions:  sudo apt-get install -y podman"
fi

echo
echo "Next:  ${run} onboard      # set up Podman, a writer and a reviewer for this user, asking first"
echo "       ${run} doctor       # check the sandbox, the models and the daemon"
echo "       ${run} dev          # start the daemon; leave it running"
