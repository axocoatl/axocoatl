# Base of Axocoatl's rust toolchain image (`axocoatl recipe build rust`).
# Owner: workstream toolchain.
#
# The official Rust 1.99.1 image on Debian 12 (bookworm) slim (rustup, cargo,
# rustc, gcc and the C library headers a build script links with), pinned
# by its multi-architecture image index digest. Git and the CA bundle
# complete the POSIX and Git command surface Axocoatl probes Session images
# for, so a Session on this image never provisions packages at start; make
# is there for `make test` checks. The official image makes rustup's and
# cargo's directories writable by every user; here they are root's alone,
# so no Session user can change the toolchain the checks run.
FROM docker.io/library/rust:1.99.1-slim-bookworm@sha256:a24e292b4af7bdee351a31d3a6379972a5652f0ea22a1b6d6883324ac4f5097c

RUN export DEBIAN_FRONTEND=noninteractive \
 && apt-get update -qq \
 && apt-get install -y --no-install-recommends ca-certificates git make \
 && rm -rf /var/lib/apt/lists/* \
 && chmod -R go-w /usr/local/cargo /usr/local/rustup \
 && for command in sh git env grep tee rm mkdir mv cp cat head tail od wc find realpath ls rmdir test sha256sum base64; do \
      command -v "$command" >/dev/null || { echo "toolchain base lacks $command" >&2; exit 1; }; \
    done

LABEL io.axocoatl.role=recipe \
      io.axocoatl.recipe.base=rust-1.99.1-slim-bookworm
