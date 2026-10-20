# Toolchain recipe: go. Owner: workstream toolchain.
# Appended to the go toolchain base (Go 1.27.2 on Debian 12, git). The
# module cache lives in the Workspace's toolchain cache volume (mounted at
# /opt/axocoatl-cache), which only a run's setup writes; the build cache is
# the Session user's own (in their home), so nothing one run builds is
# reused by another. Only a loadout run's setup reaches the module proxy (it
# sets GOPROXY itself); every other command sees GOPROXY=off and fails at
# once on a module that is not cached, instead of waiting on a refused
# connection. GOTOOLCHAIN=local keeps the image's Go.
ENV AXOCOATL_TOOLCHAIN=go \
    AXOCOATL_CACHE=/opt/axocoatl-cache \
    GOMODCACHE=/opt/axocoatl-cache/go/mod \
    GOPROXY=off \
    GOTOOLCHAIN=local

RUN mkdir -p /opt/axocoatl-cache \
 && go version \
 && [ -z "$(go env GOPATH | grep -x /go)" ]

LABEL io.axocoatl.toolchain=go \
      io.axocoatl.toolchain.version=go-1.27.2
