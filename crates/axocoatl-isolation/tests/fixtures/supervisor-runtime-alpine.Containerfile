# Alpine (musl, BusyBox sh, nc and wget) image for the supervisor runtime tests.
# BASE is alpine:3.20 pinned by the digest of the target platform's image;
# scripts/test-exec-supervisor-runtime.sh passes it. The packages are the ones
# the recovery tools image adds.
ARG BASE
FROM ${BASE}
RUN apk add --no-cache coreutils findutils git grep
CMD ["/bin/sh"]
