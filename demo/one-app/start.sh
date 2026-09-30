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

if ! command -v node >/dev/null 2>&1; then
  echo "Node.js is required to check the local Ollama service." >&2
  exit 1
fi

# The presenter configuration. AXOCOATL_DEMO_CONFIG selects another reviewed
# configuration in this directory (for example axocoatl.team.yaml) or an
# absolute path; the default keeps every other scenario on axocoatl.demo.yaml.
DEMO_CONFIG="${AXOCOATL_DEMO_CONFIG:-axocoatl.demo.yaml}"
case "$DEMO_CONFIG" in
  /*) ;;
  *) DEMO_CONFIG="$SCRIPT_DIR/$DEMO_CONFIG" ;;
esac
if [ ! -f "$DEMO_CONFIG" ] || [ -L "$DEMO_CONFIG" ]; then
  echo "AXOCOATL_DEMO_CONFIG is not a regular configuration file: $DEMO_CONFIG" >&2
  exit 2
fi

# The configuration reads the Ollama port from this variable. Native Sessions
# admit only an Ollama whose cloud models are disabled; point this at such a
# service instead of reconfiguring the one you already run.
AXOCOATL_DEMO_OLLAMA_PORT="${AXOCOATL_DEMO_OLLAMA_PORT:-11434}"
case "$AXOCOATL_DEMO_OLLAMA_PORT" in
  ''|*[!0-9]*)
    echo "AXOCOATL_DEMO_OLLAMA_PORT must be a TCP port number." >&2
    exit 2
    ;;
esac
if [ "$AXOCOATL_DEMO_OLLAMA_PORT" -lt 1 ] || [ "$AXOCOATL_DEMO_OLLAMA_PORT" -gt 65535 ]; then
  echo "AXOCOATL_DEMO_OLLAMA_PORT must be between 1 and 65535." >&2
  exit 2
fi
export AXOCOATL_DEMO_OLLAMA_PORT
OLLAMA_URL="http://127.0.0.1:$AXOCOATL_DEMO_OLLAMA_PORT"

if ! OLLAMA_TAGS="$(curl -fsS --max-time 2 "$OLLAMA_URL/api/tags")"; then
  echo "Ollama is not listening on 127.0.0.1:$AXOCOATL_DEMO_OLLAMA_PORT." >&2
  echo "Start it with: ollama serve, or set AXOCOATL_DEMO_OLLAMA_PORT." >&2
  exit 1
fi
if ! OLLAMA_STATUS="$(curl -fsS --max-time 2 "$OLLAMA_URL/api/status")" ||
  ! printf '%s' "$OLLAMA_STATUS" | node -e '
    let body = "";
    process.stdin.on("data", chunk => { body += chunk; });
    process.stdin.on("end", () => {
      try { process.exit(JSON.parse(body)?.cloud?.disabled === true ? 0 : 1); }
      catch { process.exit(1); }
    });
  '; then
  echo "The Ollama service on 127.0.0.1:$AXOCOATL_DEMO_OLLAMA_PORT does not report" >&2
  echo "cloud models disabled in /api/status, so native Sessions refuse it." >&2
  echo "Run a separate local service with OLLAMA_NO_CLOUD=1 and set" >&2
  echo "AXOCOATL_DEMO_OLLAMA_PORT to its port. Do not reconfigure a service you" >&2
  echo "did not start for this demo." >&2
  exit 1
fi
# Every model the selected configuration names must be installed there.
REQUIRED_MODELS="$(awk '/^[[:space:]]*model:/ { gsub(/["'\'']/, "", $2); print $2 }' "$DEMO_CONFIG" | sort -u)"
for model in $REQUIRED_MODELS; do
  if ! printf '%s' "$OLLAMA_TAGS" | MODEL="$model" node -e '
    let body = "";
    process.stdin.on("data", chunk => { body += chunk; });
    process.stdin.on("end", () => {
      try {
        const names = new Set((JSON.parse(body).models || []).map(item => item.name));
        process.exit(names.has(process.env.MODEL) ? 0 : 1);
      } catch { process.exit(1); }
    });
  '; then
    echo "Required model $model is not installed in the Ollama service on" >&2
    echo "127.0.0.1:$AXOCOATL_DEMO_OLLAMA_PORT." >&2
    echo "Run: OLLAMA_HOST=127.0.0.1:$AXOCOATL_DEMO_OLLAMA_PORT ollama pull $model" >&2
    exit 1
  fi
done
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

# Port 8765 is the storefront's logical port inside a Session container. It is
# published on a dynamic loopback host port and reached through Preview, so a
# host process on 8765 does not conflict with the demo.

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
AXOCOATL_BIN="${AXOCOATL_DEMO_BIN:-}"
if [ -n "$AXOCOATL_BIN" ]; then
  case "$AXOCOATL_BIN" in
    /*) ;;
    *)
      echo "AXOCOATL_DEMO_BIN must be an absolute path to the exact release candidate." >&2
      exit 2
      ;;
  esac
  if [ ! -f "$AXOCOATL_BIN" ] || [ ! -x "$AXOCOATL_BIN" ]; then
    echo "AXOCOATL_DEMO_BIN is not an executable file: $AXOCOATL_BIN" >&2
    exit 1
  fi
  # The deterministic local MCP fixture remains a separate debug helper named
  # by the demo configuration; only the product binary is overridden.
  "$CARGO_BIN" build -p mcp-bridge
else
  "$CARGO_BIN" build -p axocoatl-cli -p mcp-bridge
  AXOCOATL_BIN="$REPO_ROOT/target/debug/axocoatl"
fi

"$AXOCOATL_BIN" validate "$DEMO_CONFIG"
AXOCOATL_VERSION="$("$AXOCOATL_BIN" --version)"
if command -v shasum >/dev/null 2>&1; then
  AXOCOATL_SHA256="$(shasum -a 256 "$AXOCOATL_BIN" | awk '{print $1}')"
else
  AXOCOATL_SHA256="$(sha256sum "$AXOCOATL_BIN" | awk '{print $1}')"
fi
if command -v shasum >/dev/null 2>&1; then
  CONFIG_SHA256="$(shasum -a 256 "$DEMO_CONFIG" | awk '{print $1}')"
else
  CONFIG_SHA256="$(sha256sum "$DEMO_CONFIG" | awk '{print $1}')"
fi

export AXOCOATL_DATA_DIR="$DEMO_ROOT/data"
export AXOCOATL_SOCKET_PATH="$DEMO_ROOT/run/axocoatl.sock"
export RUST_LOG="${RUST_LOG:-info}"

echo
echo "Axocoatl demo"
echo "App:       http://127.0.0.1:18080"
echo "Workspace: $WORKSPACE"
echo "Binary:    $AXOCOATL_BIN"
echo "Version:   $AXOCOATL_VERSION"
echo "SHA-256:   $AXOCOATL_SHA256"
echo "Config:    $DEMO_CONFIG"
echo "Config SHA-256: $CONFIG_SHA256"
echo "Ollama:    $OLLAMA_URL (cloud models disabled)"
echo "Prompts:   $SCRIPT_DIR/PROMPTS.md"
echo "Seed:      $SCRIPT_DIR/seed-runtime-demos.sh"
echo

exec "$AXOCOATL_BIN" dev -c "$DEMO_CONFIG"
