#!/usr/bin/env bash
# CI only: pull official Docker Hub images through mirror.gcr.io, Google's
# public cache, so the anonymous pulls of a CI or release run
# (the Session images, the toolchain images and the test images, from many
# jobs at once) do not meet Docker Hub's per-address limit for anonymous
# pulls. Third-party images go directly to Docker Hub: the cache can serve
# their multi-architecture index while missing its selected child manifest,
# after which Podman 4.9 does not try another source. Keep that failure out
# of runtime image creation without changing any image name or pinned digest.
# Official images keep their docker.io name, and a digest identifies the same
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
prefix = "docker.io/library"
location = "docker.io/library"

[[registry.mirror]]
location = "mirror.gcr.io/library"
CONF
echo "Podman pulls docker.io/library images through mirror.gcr.io first; third-party images use their origin ($conf)."
