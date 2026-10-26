#!/usr/bin/env bash
# CI only: pull Docker Hub images from their origin, retaining every image
# name, pinned digest and TLS check. Opportunistic mirrors can serve a valid
# index and child manifest while a blob redirects to a missing cached object.
# Podman 4.9 does not fall back after that blob failure. This affected both
# third-party and official images, so do not select a mirror by namespace.
set -euo pipefail

[[ $# -eq 0 ]] || {
  echo 'Usage: ci-registry.sh' >&2
  exit 2
}
[[ "${GITHUB_ACTIONS:-}" == true ]] || {
  echo 'ci-registry: this changes /etc/containers; it runs only on a CI runner' >&2
  exit 2
}

# Replace the existing CI-owned drop-in so its former mirror cannot remain.
conf=/etc/containers/registries.conf.d/50-axocoatl-ci-docker-hub-mirror.conf
sudo mkdir -p "${conf%/*}"
sudo tee "$conf" >/dev/null <<'CONF'
# Written by scripts/ci-registry.sh for this CI run.
[[registry]]
prefix = "docker.io"
location = "docker.io"
CONF
echo "Podman pulls Docker Hub images from their origin with unchanged digest pins and TLS verification ($conf)."
