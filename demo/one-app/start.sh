#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH= cd -- "$SCRIPT_DIR/../.." && pwd)"
case "$(uname -s)" in
  Darwin) DEFAULT_DEMO_ROOT="/private/tmp/axocoatl-one-app-showcase" ;;
  *) DEFAULT_DEMO_ROOT="/tmp/axocoatl-one-app-showcase" ;;
esac
DEMO_ROOT="${AXOCOATL_DEMO_ROOT:-$DEFAULT_DEMO_ROOT}"
WORKSPACE="$DEMO_ROOT/workspace"
DEMO_IMAGE="localhost/axocoatl-one-app-demo:latest"

DEMO_PARENT="$(dirname -- "$DEMO_ROOT")"
DEMO_NAME="$(basename -- "$DEMO_ROOT")"
case "$DEMO_PARENT" in
  /private/tmp|/tmp) ;;
  *)
    echo "The demo root must be a direct child of /private/tmp or /tmp." >&2
    exit 2
    ;;
esac
case "$DEMO_NAME" in
  axocoatl-one-app-showcase|axocoatl-one-app-showcase-*) ;;
  *)
    echo "The demo root name must begin with axocoatl-one-app-showcase." >&2
    exit 2
    ;;
esac

if [ -L "$DEMO_ROOT" ]; then
  echo "Refusing symlink demo root: $DEMO_ROOT" >&2
  exit 2
fi

tcp_port_open() {
  (exec 3<>"/dev/tcp/127.0.0.1/$1") >/dev/null 2>&1
}

session_key_for() {
  if command -v shasum >/dev/null 2>&1; then
    printf 'session\0%s' "$1" | shasum -a 256 | cut -c1-16
  else
    printf 'session\0%s' "$1" | sha256sum | cut -c1-16
  fi
}

if ! command -v shasum >/dev/null 2>&1 && ! command -v sha256sum >/dev/null 2>&1; then
  echo "A SHA-256 utility is required (shasum or sha256sum)." >&2
  exit 1
fi

if [ ! -f "$DEMO_ROOT/.axocoatl-showcase" ] || [ ! -d "$WORKSPACE/.git" ]; then
  echo "Demo workspace is not prepared. Run $SCRIPT_DIR/prepare.sh first." >&2
  exit 1
fi

if ! curl -fsS --max-time 2 http://127.0.0.1:11434/api/tags >/dev/null; then
  echo "Ollama is not listening on 127.0.0.1:11434." >&2
  echo "Start it with: ollama serve" >&2
  exit 1
fi
if ! ollama list | awk 'NR > 1 { print $1 }' | grep -qx 'qwen3:8b'; then
  echo "Required model qwen3:8b is not installed. Run: ollama pull qwen3:8b" >&2
  exit 1
fi
if ! podman info >/dev/null 2>&1; then
  echo "Podman is not ready. Run $SCRIPT_DIR/prepare.sh first." >&2
  exit 1
fi
if ! podman system check --quick >/dev/null 2>&1; then
  echo "Podman reports damaged local storage. Review 'podman system check --quick'" >&2
  echo "before starting the demo." >&2
  exit 1
fi
if ! podman image exists "$DEMO_IMAGE"; then
  echo "The demo image is missing. Run $SCRIPT_DIR/prepare.sh while Podman is active." >&2
  exit 1
fi

if tcp_port_open 18080; then
  echo "Port 18080 is already in use; Axocoatl's demo endpoint cannot bind." >&2
  exit 1
fi

EXISTING_CONTAINERS="$(podman ps -a --filter name=axo-ses- --format '{{.Names}} {{.Status}}')"
KNOWN_CONTAINERS=""
if [ -n "$EXISTING_CONTAINERS" ]; then
  UNKNOWN_CONTAINERS=""
  while IFS=' ' read -r container_name _container_status; do
    known=false
    for session_file in "$DEMO_ROOT"/data/sessions/ses-*.json; do
      [ -f "$session_file" ] || continue
      session_id="$(basename "$session_file" .json)"
      session_key="$(session_key_for "$session_id")"
      case "$container_name" in
        "axo-ses-$session_id"|"axo-ses-attempt-$session_key-"*) known=true ;;
      esac
    done
    if [ "$known" != true ]; then
      UNKNOWN_CONTAINERS="${UNKNOWN_CONTAINERS}${container_name}\n"
    else
      KNOWN_CONTAINERS="${KNOWN_CONTAINERS}${container_name}\n"
    fi
  done <<< "$EXISTING_CONTAINERS"
  if [ -n "$UNKNOWN_CONTAINERS" ]; then
    echo "Refusing to start beside Axocoatl containers not owned by this demo:" >&2
    printf '%b' "$UNKNOWN_CONTAINERS" >&2
    echo "Close them with their owning daemon before starting this demo." >&2
    exit 1
  fi
  echo "Resuming containers already owned by this demo data directory."
fi

if tcp_port_open 8765; then
  PORT_OWNERS="$(
    podman ps --format '{{.Names}}\t{{.Ports}}' |
      awk 'index($0, ":8765->") { print $1 }'
  )"
  UNKNOWN_PORT_OWNER=false
  if [ -z "$PORT_OWNERS" ]; then
    UNKNOWN_PORT_OWNER=true
  else
    while IFS= read -r owner; do
      [ -n "$owner" ] || continue
      if ! printf '%b' "$KNOWN_CONTAINERS" | grep -Fqx "$owner"; then
        UNKNOWN_PORT_OWNER=true
      fi
    done <<< "$PORT_OWNERS"
  fi
  if [ "$UNKNOWN_PORT_OWNER" = true ]; then
    echo "Port 8765 is already in use outside this demo's known session containers." >&2
    exit 1
  fi
  echo "Port 8765 is already published by a resumable demo session container."
fi

CARGO_BIN="${CARGO_BIN:-$(command -v cargo || true)}"
if [ -z "$CARGO_BIN" ]; then
  DEFAULT_CARGO_BIN="${CARGO_HOME:-${HOME:-}/.cargo}/bin/cargo"
  if [ -x "$DEFAULT_CARGO_BIN" ]; then
    CARGO_BIN="$DEFAULT_CARGO_BIN"
  fi
fi
if [ -z "$CARGO_BIN" ] || [ ! -x "$CARGO_BIN" ]; then
  echo "cargo was not found. Install Rust or set CARGO_BIN to the cargo executable." >&2
  exit 1
fi

cd "$REPO_ROOT"
"$CARGO_BIN" build -p axocoatl-cli
target/debug/axocoatl validate "$SCRIPT_DIR/axocoatl.demo.yaml"

export AXOCOATL_DATA_DIR="$DEMO_ROOT/data"
export AXOCOATL_SOCKET_PATH="$DEMO_ROOT/run/axocoatl.sock"
export RUST_LOG="${RUST_LOG:-info}"

echo
echo "Axocoatl demo"
echo "App:       http://127.0.0.1:18080"
echo "Workspace: $WORKSPACE"
echo "Prompts:   $SCRIPT_DIR/PROMPTS.md"
echo

exec target/debug/axocoatl dev -c "$SCRIPT_DIR/axocoatl.demo.yaml"
