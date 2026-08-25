#!/usr/bin/env bash
# Verify the minimum toolchain for building Axocoatl from source.
#
# This script installs nothing and does not change the host. Rootless Podman is
# needed to run folder sessions and parallel attempts, but not to compile the
# workspace.

set -euo pipefail

info() { printf '[ok] %s\n' "$1"; }
warn() { printf '[!!] %s\n' "$1"; }
fail() {
    printf '[err] %s\n' "$1" >&2
    exit 1
}

printf '=== Axocoatl source setup ===\n\n'

command -v cargo >/dev/null 2>&1 || fail "Cargo not found. Install Rust from https://rustup.rs/"
command -v rustc >/dev/null 2>&1 || fail "rustc not found. Install Rust from https://rustup.rs/"

RUST_VERSION="$(rustc --version | awk '{print $2}')"
RUST_CORE="${RUST_VERSION%%-*}"
RUST_MAJOR="${RUST_CORE%%.*}"
RUST_REMAINDER="${RUST_CORE#*.}"
RUST_MINOR="${RUST_REMAINDER%%.*}"

if [[ ! "$RUST_MAJOR" =~ ^[0-9]+$ || ! "$RUST_MINOR" =~ ^[0-9]+$ ]]; then
    fail "Could not parse rustc version '$RUST_VERSION'"
fi
if (( RUST_MAJOR < 1 || (RUST_MAJOR == 1 && RUST_MINOR < 88) )); then
    fail "Rust 1.88 or newer is required (found $RUST_VERSION). Run: rustup update stable"
fi
info "Rust $RUST_VERSION"

if command -v podman >/dev/null 2>&1; then
    info "Podman found for local folder sessions and sandbox integration tests"
else
    warn "Podman not found; source builds work, but local folder sessions and parallel attempts require it"
fi

printf '\n=== Verifying workspace ===\n\n'
cargo check --workspace
info "Workspace compiles"

printf '\n=== Setup complete ===\n\n'
printf '%s\n' \
    'Build the CLI and embedded browser app:' \
    '  cargo build -p axocoatl-cli --release' \
    '' \
    'Run the repository gate:' \
    '  cargo fmt --all -- --check' \
    '  cargo clippy --workspace --all-targets --all-features -- -D warnings' \
    '  cargo test --workspace' \
    '  cargo test --doc --workspace' \
    '' \
    'The workbench uses rootless Podman locally or a configured E2B-compatible' \
    'remote backend.'
