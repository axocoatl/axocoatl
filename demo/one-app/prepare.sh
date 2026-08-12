#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
case "$(uname -s)" in
  Darwin) DEFAULT_DEMO_ROOT="/private/tmp/axocoatl-one-app-showcase" ;;
  *) DEFAULT_DEMO_ROOT="/tmp/axocoatl-one-app-showcase" ;;
esac
DEMO_ROOT="${AXOCOATL_DEMO_ROOT:-$DEFAULT_DEMO_ROOT}"
WORKSPACE="$DEMO_ROOT/workspace"
MARKER="$DEMO_ROOT/.axocoatl-showcase"
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

for required in curl git node npm podman; do
  if ! command -v "$required" >/dev/null 2>&1; then
    echo "Missing required command: $required" >&2
    exit 1
  fi
done

tcp_port_open() {
  (exec 3<>"/dev/tcp/127.0.0.1/$1") >/dev/null 2>&1
}

if ! podman info >/dev/null 2>&1; then
  if podman machine list >/dev/null 2>&1; then
    podman machine start
  else
    echo "Podman is not ready. Start it, then rerun this script." >&2
    exit 1
  fi
fi

# Never move the durable root out from under a live demo. A daemon can keep
# writing after the rename, splitting one run across the backup and the newly
# prepared directory. Refuse both a listening demo endpoint and every Axocoatl
# session container; the operator must resolve/close those sessions first.
if tcp_port_open 18080; then
  echo "Port 18080 is already in use; Axocoatl's demo endpoint cannot bind." >&2
  echo "Resolve attempts and Interrupts, close its session, and stop that daemon first." >&2
  exit 1
fi
if tcp_port_open 8765; then
  echo "Port 8765 is already in use; the storefront Browser preview would be unavailable." >&2
  echo "Stop the process that owns that port before preparing the demo." >&2
  exit 1
fi

AXO_SESSION_CONTAINERS="$(
  podman ps -a --format '{{.Names}}' |
    awk '/^axo-ses-/ { print }'
)"
if [ -n "$AXO_SESSION_CONTAINERS" ]; then
  echo "Refusing to prepare while Axocoatl session containers exist:" >&2
  printf '%s\n' "$AXO_SESSION_CONTAINERS" >&2
  echo "Identify and close their owning sessions; this script will not remove them." >&2
  exit 1
fi

if ! podman system check --quick >/dev/null 2>&1; then
  echo "Podman reports damaged local storage. Run 'podman system check --quick'" >&2
  echo "and review the affected images before repairing or rebuilding the demo." >&2
  exit 1
fi

podman build \
  --tag "$DEMO_IMAGE" \
  --file "$SCRIPT_DIR/Containerfile" \
  "$SCRIPT_DIR"
if ! podman image exists "$DEMO_IMAGE"; then
  echo "Podman did not retain the demo image after a successful build." >&2
  exit 1
fi

# Rotate the old root only after every non-mutating prerequisite and the image
# build have succeeded. A transient tool/storage/build failure must leave the
# last rehearsed demo intact.
if [ -e "$DEMO_ROOT" ]; then
  if [ ! -f "$MARKER" ]; then
    echo "Refusing to replace an unmarked directory: $DEMO_ROOT" >&2
    exit 2
  fi
  BACKUP="${DEMO_ROOT}.previous-$(date +%Y%m%d-%H%M%S)"
  mv -- "$DEMO_ROOT" "$BACKUP"
  echo "Previous demo moved to $BACKUP"
fi

mkdir -p "$WORKSPACE" "$DEMO_ROOT/data" "$DEMO_ROOT/run"
chmod 700 "$DEMO_ROOT" "$DEMO_ROOT/data" "$DEMO_ROOT/run"
touch "$MARKER"
cp -R "$SCRIPT_DIR/workspace-template/." "$WORKSPACE/"

git -C "$WORKSPACE" init --quiet --initial-branch=main
git -C "$WORKSPACE" config user.name "Axocoatl Demo"
git -C "$WORKSPACE" config user.email "demo@localhost"
git -C "$WORKSPACE" add .
git -C "$WORKSPACE" commit --quiet -m "Seed storefront discount regression"
git -C "$WORKSPACE" tag demo-seed

CHECK_LOG="$DEMO_ROOT/seed-check.log"
if (cd "$WORKSPACE" && npm run check >"$CHECK_LOG" 2>&1); then
  echo "The demo seed unexpectedly passed. The fixture must begin with one red check." >&2
  exit 1
fi
if ! grep -q "never returns a negative payable total" "$CHECK_LOG"; then
  echo "The demo failed for an unexpected reason. Read $CHECK_LOG" >&2
  exit 1
fi
if ! grep -Eq 'tests[[:space:]]+6[[:space:]]*$' "$CHECK_LOG" ||
   ! grep -Eq 'pass[[:space:]]+5[[:space:]]*$' "$CHECK_LOG" ||
   ! grep -Eq 'fail[[:space:]]+1[[:space:]]*$' "$CHECK_LOG"; then
  echo "The demo seed must have exactly 6 tests, 5 passing and 1 failing." >&2
  echo "Read $CHECK_LOG before presenting." >&2
  exit 1
fi

echo
echo "Demo prepared."
echo "Workspace: $WORKSPACE"
echo "Expected red check: $CHECK_LOG"
echo "Next: $SCRIPT_DIR/start.sh"
