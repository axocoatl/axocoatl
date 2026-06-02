#!/usr/bin/env bash
# Axocoatl Development Environment Setup
#
# Sets up all dependencies needed to build Axocoatl from source.
# Run once after cloning the repository.
#
# Usage: ./scripts/setup-dev.sh
#
# What this installs:
#   - protoc 26.x (protobuf compiler, needed by LanceDB/semantic memory)
#   - wasm-pack (for UI WASM bridge compilation)
#   - Verifies Rust toolchain >= 1.75
#
# Isolation Tiers (compiled via Cargo features):
#   Tier 0 (None):        In-process. Trusted built-in tools only.
#   Tier 1 (Wasmtime):    Universal. <1ms startup. Always compiled (default).
#   Tier 2 (youki OCI):   Linux only. ~198ms startup. Feature: oci-isolation
#   Tier 3 (Firecracker): Linux + KVM. <125ms cold / <5ms warm. Feature: firecracker-isolation

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

info()  { echo -e "${GREEN}[ok]${NC} $1"; }
warn()  { echo -e "${YELLOW}[!!]${NC} $1"; }
fail()  { echo -e "${RED}[err]${NC} $1"; exit 1; }

echo "=== Axocoatl Development Environment Setup ==="
echo ""

# ── 1. Check Rust toolchain ──────────────────────────────────────────────────

if ! command -v rustc &>/dev/null; then
    fail "Rust not found. Install via: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
fi

RUST_VERSION=$(rustc --version | grep -oP '\d+\.\d+' | head -1)
RUST_MAJOR=$(echo "$RUST_VERSION" | cut -d. -f1)
RUST_MINOR=$(echo "$RUST_VERSION" | cut -d. -f2)

if [ "$RUST_MAJOR" -lt 1 ] || { [ "$RUST_MAJOR" -eq 1 ] && [ "$RUST_MINOR" -lt 75 ]; }; then
    fail "Rust >= 1.75 required (found $RUST_VERSION). Run: rustup update stable"
fi
info "Rust $RUST_VERSION"

# ── 2. Install protoc (protobuf compiler) ────────────────────────────────────

PROTOC_VERSION="26.1"
INSTALL_DIR="$HOME/.local/bin"
mkdir -p "$INSTALL_DIR"

install_protoc() {
    local ARCH
    ARCH=$(uname -m)
    local OS
    OS=$(uname -s | tr '[:upper:]' '[:lower:]')

    local PROTOC_ZIP
    case "$OS-$ARCH" in
        linux-x86_64)   PROTOC_ZIP="protoc-${PROTOC_VERSION}-linux-x86_64.zip" ;;
        linux-aarch64)  PROTOC_ZIP="protoc-${PROTOC_VERSION}-linux-aarch_64.zip" ;;
        darwin-x86_64)  PROTOC_ZIP="protoc-${PROTOC_VERSION}-osx-x86_64.zip" ;;
        darwin-arm64)   PROTOC_ZIP="protoc-${PROTOC_VERSION}-osx-aarch_64.zip" ;;
        *) fail "Unsupported platform: $OS-$ARCH" ;;
    esac

    local URL="https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VERSION}/${PROTOC_ZIP}"
    local TMP_DIR
    TMP_DIR=$(mktemp -d)

    echo "  Downloading protoc ${PROTOC_VERSION}..."
    curl -fsSL -o "$TMP_DIR/protoc.zip" "$URL"
    unzip -q -o "$TMP_DIR/protoc.zip" -d "$TMP_DIR/protoc"
    cp "$TMP_DIR/protoc/bin/protoc" "$INSTALL_DIR/protoc"
    chmod +x "$INSTALL_DIR/protoc"

    # Install well-known proto includes
    mkdir -p "$HOME/.local/include"
    cp -r "$TMP_DIR/protoc/include/"* "$HOME/.local/include/"

    rm -rf "$TMP_DIR"
}

NEED_PROTOC=true
if command -v protoc &>/dev/null; then
    CURRENT_PROTOC=$(protoc --version | grep -oP '\d+\.\d+' | head -1)
    PROTOC_MAJOR=$(echo "$CURRENT_PROTOC" | cut -d. -f1)
    if [ "$PROTOC_MAJOR" -ge 15 ]; then
        NEED_PROTOC=false
        info "protoc $CURRENT_PROTOC (>= 15, OK)"
    else
        warn "protoc $CURRENT_PROTOC too old (need >= 15). Upgrading..."
    fi
fi

if $NEED_PROTOC; then
    install_protoc
    info "protoc ${PROTOC_VERSION} installed to $INSTALL_DIR/protoc"
fi

# Ensure ~/.local/bin is in PATH
if ! echo "$PATH" | grep -q "$INSTALL_DIR"; then
    warn "$INSTALL_DIR is not in PATH. Add to your shell profile:"
    echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
fi

# ── 3. Install wasm-pack ─────────────────────────────────────────────────────

if ! command -v wasm-pack &>/dev/null; then
    echo "  Installing wasm-pack..."
    curl https://rustwasm.github.io/wasm-pack/installer/init.sh -sSf | sh
    info "wasm-pack installed"
else
    info "wasm-pack $(wasm-pack --version 2>/dev/null | head -1)"
fi

# ── 4. Check platform capabilities ───────────────────────────────────────────

echo ""
echo "=== Isolation Tier Availability ==="
echo ""

info "Tier 1 (Wasmtime):    AVAILABLE — universal, always compiled"

if [ "$(uname -s)" = "Linux" ]; then
    info "Tier 2 (youki OCI):   AVAILABLE — compile with: cargo build --features oci-isolation"
else
    warn "Tier 2 (youki OCI):   UNAVAILABLE — requires Linux (cgroups + namespaces)"
fi

if [ -e /dev/kvm ]; then
    info "Tier 3 (Firecracker): AVAILABLE — compile with: cargo build --features firecracker-isolation"
else
    if [ "$(uname -s)" = "Linux" ]; then
        warn "Tier 3 (Firecracker): UNAVAILABLE — /dev/kvm not found (needs bare-metal Linux or nested virt)"
    else
        warn "Tier 3 (Firecracker): UNAVAILABLE — requires Linux + KVM"
    fi
fi

# ── 5. Verify workspace builds ───────────────────────────────────────────────

echo ""
echo "=== Verifying Build ==="
echo ""

echo "  Running cargo check..."
if cargo check --workspace 2>/dev/null; then
    info "Workspace compiles"
else
    warn "cargo check failed — run manually to see errors"
fi

echo ""
echo "=== Setup Complete ==="
echo ""
echo "Quick start:"
echo "  cargo test --workspace        # Run all tests"
echo "  cargo build --release         # Release build"
echo ""
echo "Feature flags:"
echo "  --features semantic           # Enable LanceDB semantic memory (axocoatl-memory)"
echo "  --features oci-isolation      # Enable youki OCI containers (axocoatl-isolation)"
echo "  --features firecracker-isolation  # Enable Firecracker VMs (axocoatl-isolation)"
