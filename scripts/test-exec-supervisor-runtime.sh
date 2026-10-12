#!/usr/bin/env bash
# Run the embedded execution supervisors' protocol and process-lifecycle tests
# (crates/axocoatl-isolation/tests/supervisor_runtime.rs) in Alpine and Debian
# images of both architectures, as root and as a non-root user.
set -euo pipefail

fail() { echo "test-exec-supervisor-runtime: $*" >&2; exit 1; }
usage() {
  cat <<'USAGE'
Usage: test-exec-supervisor-runtime.sh <prepare|run|remove> [x86_64|aarch64]...

  prepare  Build the eight test images with Podman: Alpine 3.20 and Debian
           bookworm-slim, linux/amd64 and linux/arm64, each as root and with
           USER 1024:1024. The base images are pinned by digest. This step
           downloads the base images and the Alpine and Debian packages, and
           building for the other architecture needs user-mode emulation.
  run      Run the supervisor_runtime suite once per image. Each run checks the
           image's platform, the USER uid the sandbox sees, and that the
           installed program is the embedded payload for the image's
           architecture. It builds and pulls no image; a missing one is an error.
  remove   Remove the eight test images (not the pinned base images).

Naming architectures limits prepare and run to them. The read-only and
network-denying tests need Landlock, which user-mode emulation such as
qemu-user does not provide: the supervisor then refuses the restricted command
and those two tests fail. Run each architecture on a Linux kernel of that
architecture (a Podman machine on Apple silicon is aarch64).

CONTAINER_CONNECTION selects the Podman connection, as for the tests.
USAGE
}

[[ $# -ge 1 ]] || { usage >&2; exit 1; }
command=$1
shift
case "$command" in
  prepare|run|remove) ;;
  --help|-h) usage; exit 0 ;;
  *) usage >&2; exit 1 ;;
esac
architectures=(x86_64 aarch64)
if [[ $# -gt 0 ]]; then
  for architecture in "$@"; do
    case "$architecture" in x86_64|aarch64) ;; *) fail "unknown architecture $architecture" ;; esac
  done
  architectures=("$@")
fi

script_directory="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source_root="$(CDPATH= cd -- "$script_directory/.." && pwd)"
fixtures="$source_root/crates/axocoatl-isolation/tests/fixtures"
assets="$source_root/crates/axocoatl-isolation/assets/exec-supervisor"
repository=localhost/axocoatl-supervisor-runtime
command -v podman >/dev/null 2>&1 || fail 'podman is required'

# Platform images of alpine:3.20 and debian:bookworm-slim, by digest.
base_image() {
  case "$1/$2" in
    alpine/x86_64) echo docker.io/library/alpine@sha256:c64c687cbea9300178b30c95835354e34c4e4febc4badfe27102879de0483b5e ;;
    alpine/aarch64) echo docker.io/library/alpine@sha256:45e09956dc667c5eff3583c9d94830261fb1ca0be10a0a7db36266edf5de9e1d ;;
    debian/x86_64) echo docker.io/library/debian@sha256:a4672c0cb26fbdde88e38fa2dfb6c681942306680e41e4378b28770b6e79ee91 ;;
    debian/aarch64) echo docker.io/library/debian@sha256:a1b86db52ce3daef089e45aabe36dfec4091f82464c25c1fdcf03de197cbe82a ;;
    *) fail "no base image for $1/$2" ;;
  esac
}
platform() {
  case "$1" in x86_64) echo linux/amd64 ;; aarch64) echo linux/arm64 ;; *) fail "unknown architecture $1" ;; esac
}
check_platform() {
  local image=$1 architecture=$2 actual
  actual="$(podman image inspect --format '{{.Os}}/{{.Architecture}}' "$image")" || fail "image $image is missing; run prepare first"
  [[ "$actual" == "$(platform "$architecture")" ]] || fail "image $image is $actual, not $(platform "$architecture")"
}

systems=(alpine debian)
users=(root nonroot)

case "$command" in
  prepare)
    for system in "${systems[@]}"; do
      for architecture in "${architectures[@]}"; do
        root_image="$repository:$system-$architecture-root"
        build_arguments=(--build-arg "BASE=$(base_image "$system" "$architecture")")
        if [[ "$system" == debian ]]; then
          build_arguments+=(--build-arg "PROBE_BASE=$(base_image alpine "$architecture")")
        fi
        echo "== building $root_image"
        podman build --platform "$(platform "$architecture")" --pull=missing "${build_arguments[@]}" \
          -f "$fixtures/supervisor-runtime-$system.Containerfile" -t "$root_image" "$fixtures"
        echo "== building $repository:$system-$architecture-nonroot"
        podman build --platform "$(platform "$architecture")" --pull=never \
          --build-arg "ROOT_IMAGE=$root_image" \
          -f "$fixtures/supervisor-runtime-nonroot.Containerfile" -t "$repository:$system-$architecture-nonroot" "$fixtures"
        check_platform "$root_image" "$architecture"
        check_platform "$repository:$system-$architecture-nonroot" "$architecture"
      done
    done
    ;;
  remove)
    for system in "${systems[@]}"; do
      for architecture in x86_64 aarch64; do
        for user in nonroot root; do
          if podman image exists "$repository:$system-$architecture-$user"; then
            podman rmi "$repository:$system-$architecture-$user"
          fi
        done
      done
    done
    ;;
  run)
    command -v cargo >/dev/null 2>&1 || fail 'cargo is required'
    command -v python3 >/dev/null 2>&1 || fail 'python3 is required'
    cd -- "$source_root"
    cargo test --locked -p axocoatl-isolation --test supervisor_runtime --no-run
    results=()
    failed=0
    for system in "${systems[@]}"; do
      for architecture in "${architectures[@]}"; do
        expected_program="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["payload"]["sha256"])' \
          "$assets/axocoatl-exec-supervisor-linux-$architecture.manifest.json")"
        for user in "${users[@]}"; do
          image="$repository:$system-$architecture-$user"
          check_platform "$image" "$architecture"
          if [[ "$user" == root ]]; then uid=0; else uid=1024; fi
          echo "== $image (uid $uid, payload $expected_program)"
          if AXO_SUPERVISOR_TEST_IMAGE="$image" AXO_SUPERVISOR_TEST_EXPECTED_UID="$uid" \
            AXO_SUPERVISOR_TEST_EXPECTED_PROGRAM_SHA256="$expected_program" \
            cargo test --locked -p axocoatl-isolation --test supervisor_runtime -- --ignored --test-threads=1; then
            results+=("PASS $image")
          else
            results+=("FAIL $image")
            failed=1
          fi
        done
      done
    done
    printf '%s\n' "${results[@]}"
    exit "$failed"
    ;;
esac
