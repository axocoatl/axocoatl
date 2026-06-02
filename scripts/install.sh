#!/bin/sh
# Axocoatl installer — downloads a prebuilt binary from GitHub Releases.
# Usage: curl -fsSL https://raw.githubusercontent.com/axocoatl/axocoatl/main/scripts/install.sh | sh
set -eu

REPO="axocoatl/axocoatl"
BIN="axocoatl"

err() { echo "axocoatl-install: $*" >&2; exit 1; }
info() { echo "axocoatl-install: $*"; }

# --- Detect OS / arch -> release target triple ---
os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Linux)  os_part="unknown-linux-gnu" ;;
  Darwin) os_part="apple-darwin" ;;
  *) err "unsupported OS '$os' — use 'cargo install axocoatl-cli' or build from source" ;;
esac

case "$arch" in
  x86_64|amd64)  arch_part="x86_64" ;;
  arm64|aarch64) arch_part="aarch64" ;;
  *) err "unsupported architecture '$arch'" ;;
esac

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

# --- Verify checksum if available ---
if curl -fsSL "$sha_url" -o "${tmp}/sha" 2>/dev/null; then
  expected="$(cut -d' ' -f1 < "${tmp}/sha")"
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "${tmp}/${tarball}" | cut -d' ' -f1)"
  else
    actual="$(shasum -a 256 "${tmp}/${tarball}" | cut -d' ' -f1)"
  fi
  [ "$expected" = "$actual" ] || err "checksum mismatch (expected $expected, got $actual)"
  info "checksum verified"
fi

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

echo
echo "Next:  ${BIN} onboard      # interactive setup"
echo "       ${BIN} doctor       # verify environment"
