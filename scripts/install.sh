#!/bin/sh
# Axocoatl installer — downloads a prebuilt binary from GitHub Releases.
# Usage: curl -fsSL https://axocoatl.ai/install.sh | sh
set -eu

REPO="axocoatl/axocoatl"
BIN="axocoatl"

err() { echo "axocoatl-install: $*" >&2; exit 1; }
info() { echo "axocoatl-install: $*"; }

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
if [ -w "/usr/local/bin" ]; then
  dest="/usr/local/bin"
else
  dest="${HOME}/.local/bin"
  mkdir -p "$dest"
fi

install -m 0755 "${tmp}/${BIN}" "${dest}/${BIN}" 2>/dev/null \
  || { cp "${tmp}/${BIN}" "${dest}/${BIN}" && chmod 0755 "${dest}/${BIN}"; }

info "installed ${BIN} ${tag} -> ${dest}/${BIN}"

case ":${PATH}:" in
  *":${dest}:"*) ;;
  *) info "add ${dest} to your PATH:  export PATH=\"${dest}:\$PATH\"" ;;
esac

# In WSL2 a fresh distro has no Podman, which sandboxed directory sessions need.
if grep -qiE 'microsoft|wsl' /proc/version 2>/dev/null && ! command -v podman >/dev/null 2>&1; then
  info "WSL detected — install Podman for sandboxed sessions:  sudo apt-get install -y podman"
fi

echo
echo "Next:  ${BIN} onboard      # configure Axocoatl for this user"
echo "       ${BIN} doctor       # verify environment"
echo "       ${BIN} dev          # open the workbench, then choose a Workspace"
