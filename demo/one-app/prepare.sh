#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
# shellcheck source=scenario-contract.sh
source "$SCRIPT_DIR/scenario-contract.sh"

usage() {
  cat <<EOF
Usage: $0 [--scenario NAME]
       $0 --list-scenarios

Prepare one fresh, isolated Axocoatl demonstration root. With no arguments,
the original northstar-storefront scenario and root are preserved.
EOF
}

SCENARIO="northstar-storefront"
case "$#" in
  0) ;;
  1)
    case "$1" in
      --help|-h)
        usage
        exit 0
        ;;
      --list-scenarios)
        scenario_list
        exit 0
        ;;
      *)
        usage >&2
        exit 2
        ;;
    esac
    ;;
  2)
    if [ "$1" != "--scenario" ]; then
      usage >&2
      exit 2
    fi
    SCENARIO="$2"
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac
scenario_load "$SCENARIO"

case "$(uname -s)" in
  Darwin) DEMO_TMP_PARENT="/private/tmp" ;;
  *) DEMO_TMP_PARENT="/tmp" ;;
esac
if [ "$SCENARIO" = "northstar-storefront" ]; then
  DEFAULT_DEMO_ROOT="$DEMO_TMP_PARENT/axocoatl-one-app-showcase"
else
  DEFAULT_DEMO_ROOT="$DEMO_TMP_PARENT/axocoatl-one-app-showcase-$SCENARIO"
fi
DEMO_ROOT="${AXOCOATL_DEMO_ROOT:-$DEFAULT_DEMO_ROOT}"
WORKSPACE="$DEMO_ROOT/workspace"
MARKER="$DEMO_ROOT/.axocoatl-showcase"
SCENARIO_MARKER="$DEMO_ROOT/.axocoatl-showcase-scenario"
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

if [ ! -d "$SCENARIO_FIXTURE" ] || [ -L "$SCENARIO_FIXTURE" ]; then
  echo "Scenario fixture is missing or unsafe: $SCENARIO_FIXTURE" >&2
  exit 2
fi
if find "$SCENARIO_FIXTURE" -type l -print -quit | grep -q .; then
  echo "Scenario fixtures must not contain symbolic links: $SCENARIO_FIXTURE" >&2
  exit 2
fi

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
printf '%s\n' "$SCENARIO" >"$SCENARIO_MARKER"
cp -R "$SCENARIO_FIXTURE/." "$WORKSPACE/"

git -C "$WORKSPACE" init --quiet --initial-branch=main
git -C "$WORKSPACE" config user.name "Axocoatl Demo"
git -C "$WORKSPACE" config user.email "demo@localhost"
git -C "$WORKSPACE" add .
git -C "$WORKSPACE" commit --quiet -m "$SCENARIO_COMMIT"
git -C "$WORKSPACE" tag demo-seed

CHECK_LOG="$DEMO_ROOT/seed-check.log"
"$SCRIPT_DIR/verify-scenario.sh" "$SCENARIO" "$WORKSPACE" "$CHECK_LOG"

if [ -n "$(git -C "$WORKSPACE" status --porcelain)" ]; then
  echo "Scenario verification changed the prepared workspace unexpectedly." >&2
  git -C "$WORKSPACE" status --short >&2
  exit 1
fi
if [ "$(git -C "$WORKSPACE" rev-parse HEAD)" != "$(git -C "$WORKSPACE" rev-parse demo-seed^{commit})" ]; then
  echo "The demo-seed tag does not identify the prepared scenario commit." >&2
  exit 1
fi

echo
echo "Demo prepared."
echo "Scenario:  $SCENARIO"
echo "Workspace: $WORKSPACE"
echo "Expected red check: $CHECK_LOG"
if [ "$DEMO_ROOT" = "$DEMO_TMP_PARENT/axocoatl-one-app-showcase" ]; then
  echo "Next: $SCRIPT_DIR/start.sh"
else
  printf 'Next: AXOCOATL_DEMO_ROOT=%q %q\n' "$DEMO_ROOT" "$SCRIPT_DIR/start.sh"
fi
