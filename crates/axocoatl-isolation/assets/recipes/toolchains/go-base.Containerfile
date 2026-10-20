# Base of Axocoatl's go toolchain image (`axocoatl recipe build go`). Owner:
# workstream toolchain.
#
# The official Go 1.27.2 image on Debian 12 (bookworm), pinned by its
# multi-architecture image index digest. It already holds git, gcc and the
# CA bundle (Go has no slim image); the check below proves the POSIX and Git
# command surface Axocoatl probes Session images for, so a Session on this
# image never provisions packages at start.
FROM docker.io/library/golang:1.27.2-bookworm@sha256:5cf287a799e6b94384bad13d16b14904c531f51ba65792237e122ce42b392f61

RUN for command in sh git env grep tee rm mkdir mv cp cat head tail od wc find realpath ls rmdir test sha256sum base64; do \
      command -v "$command" >/dev/null || { echo "toolchain base lacks $command" >&2; exit 1; }; \
    done

LABEL io.axocoatl.role=recipe \
      io.axocoatl.recipe.base=golang-1.27.2-bookworm
