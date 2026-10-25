#!/usr/bin/env bash
# CI only: make Podman pull Docker Hub images through mirror.gcr.io, Google's
# public cache of Docker Hub, so the anonymous pulls of a CI or release run
# (the Session images, the toolchain images and the test images, from many
# jobs at once) do not meet Docker Hub's per-address limit for anonymous
# pulls. Podman tries the mirror first and docker.io when the mirror fails.
# An image keeps its docker.io name, and one pulled by digest is the same
# bytes from either. It writes one file under /etc/containers, with sudo,
# and only on a GitHub Actions runner.
set -euo pipefail

[[ $# -eq 0 ]] || {
  echo 'Usage: ci-registry-mirror.sh' >&2
  exit 2
}
[[ "${GITHUB_ACTIONS:-}" == true ]] || {
  echo 'ci-registry-mirror: this changes /etc/containers; it runs only on a CI runner' >&2
  exit 2
}

conf=/etc/containers/registries.conf.d/50-axocoatl-ci-docker-hub-mirror.conf
sudo mkdir -p "${conf%/*}"
sudo tee "$conf" >/dev/null <<'CONF'
# Written by scripts/ci-registry-mirror.sh for this CI run.
[[registry]]
prefix = "docker.io"
location = "docker.io"

[[registry.mirror]]
location = "mirror.gcr.io"
CONF
echo "Podman pulls docker.io images through mirror.gcr.io first ($conf)."
