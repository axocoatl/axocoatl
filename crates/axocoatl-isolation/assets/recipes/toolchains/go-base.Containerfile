# Base of Axocoatl's go toolchain image (`axocoatl recipe build go`). Owner:
# workstream toolchain.
#
# The official Go 1.27.2 image on Debian 12 (bookworm), pinned by its
# multi-architecture image index digest. It already holds git, gcc, make and
# the CA bundle (Go has no slim image); the check below proves the POSIX and
# Git command surface Axocoatl probes Session images for, so a Session on
# this image never provisions packages at start. The official image's
# GOPATH (/go) is writable by every user and first on its PATH; here it is
# gone: a Session user's GOPATH is in their own home, never on PATH.
FROM docker.io/library/golang:1.27.2-bookworm@sha256:5cf287a799e6b94384bad13d16b14904c531f51ba65792237e122ce42b392f61

ENV GOPATH= \
    PATH=/usr/local/go/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

RUN rm -rf /go \
 && for command in sh git env grep tee rm mkdir mv cp cat head tail od wc find realpath ls rmdir test sha256sum base64 make; do \
      command -v "$command" >/dev/null || { echo "toolchain base lacks $command" >&2; exit 1; }; \
    done

LABEL io.axocoatl.role=recipe \
      io.axocoatl.recipe.base=golang-1.27.2-bookworm
