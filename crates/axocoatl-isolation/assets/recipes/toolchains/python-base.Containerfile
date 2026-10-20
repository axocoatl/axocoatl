# Base of Axocoatl's python toolchain image (`axocoatl recipe build python`).
# Owner: workstream toolchain.
#
# The official Python 3.13.16 image on Debian 12 (bookworm) slim, pinned by
# its multi-architecture image index digest. Git and the CA bundle complete
# the POSIX and Git command surface Axocoatl probes Session images for, so a
# Session on this image never provisions packages at start. The image runs
# as root, like Axocoatl's curated images; hardened Sessions run every
# command as their own non-root workload users.
FROM docker.io/library/python:3.13.16-slim-bookworm@sha256:a1165e272e578941b84abc79e4ab38a0305cd12803a5c4247979ac7655f4d641

RUN export DEBIAN_FRONTEND=noninteractive \
 && apt-get update -qq \
 && apt-get install -y --no-install-recommends ca-certificates git \
 && rm -rf /var/lib/apt/lists/* \
 && for command in sh git env grep tee rm mkdir mv cp cat head tail od wc find realpath ls rmdir test sha256sum base64; do \
      command -v "$command" >/dev/null || { echo "toolchain base lacks $command" >&2; exit 1; }; \
    done

LABEL io.axocoatl.role=recipe \
      io.axocoatl.recipe.base=python-3.13.16-bookworm
