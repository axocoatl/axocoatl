# Debian (glibc, dash as sh, GNU coreutils) image for the supervisor runtime
# tests. BASE is debian:bookworm-slim and PROBE_BASE alpine:3.20, each pinned by
# the digest of the target platform's image; scripts/test-exec-supervisor-runtime.sh
# passes both. The network test reads BusyBox nc and wget messages. Debian
# bookworm's BusyBox wget crashes (SIGSEGV) on a failed connection and its nc
# prints different messages, so nc and wget here are Alpine's statically linked
# BusyBox; everything else, including /bin/sh, is Debian's.
ARG BASE
ARG PROBE_BASE
FROM ${PROBE_BASE} AS probe
RUN apk add --no-cache busybox-static

FROM ${BASE}
RUN apt-get update \
 && apt-get install -y --no-install-recommends git \
 && rm -rf /var/lib/apt/lists/*
COPY --from=probe /bin/busybox.static /usr/local/lib/axocoatl-probe-busybox
RUN ln -s /usr/local/lib/axocoatl-probe-busybox /usr/local/bin/nc \
 && ln -s /usr/local/lib/axocoatl-probe-busybox /usr/local/bin/wget
CMD ["/bin/sh"]
