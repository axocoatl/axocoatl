# Toolchain recipe: go. Owner: workstream toolchain.
# Appended to the go toolchain base (Go 1.27.2 on Debian 12, git). The
# module and build caches live in the Workspace's toolchain cache volume
# (mounted at /opt/axocoatl-cache). Only a loadout run's setup reaches the
# module proxy (it sets GOPROXY itself); every other command sees
# GOPROXY=off and fails at once on a module that is not cached, instead of
# waiting on a refused connection. GOTOOLCHAIN=local keeps the image's Go.
ENV AXOCOATL_TOOLCHAIN=go \
    AXOCOATL_CACHE=/opt/axocoatl-cache \
    GOMODCACHE=/opt/axocoatl-cache/go/mod \
    GOCACHE=/opt/axocoatl-cache/go/build \
    GOFLAGS=-modcacherw \
    GOPROXY=off \
    GOTOOLCHAIN=local

RUN mkdir -p /opt/axocoatl-cache \
 && go version

LABEL io.axocoatl.toolchain=go \
      io.axocoatl.toolchain.version=go-1.27.2
