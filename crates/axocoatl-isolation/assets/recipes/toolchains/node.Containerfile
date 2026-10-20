# Toolchain recipe: node. Owner: workstream toolchain.
# Appended to the recipe base (Node 24.21.0 on Debian 12 slim, git), plus
# make (`make test` is a common check). The package caches live under
# /opt/axocoatl-cache, where a loadout run mounts the Workspace's toolchain
# cache volume: a second run installs from the cache instead of the network.
# Only a run's setup writes there; afterwards the cache is root's and
# read-only to the Session's users, so npm writes no log files into it.
# Corepack's pnpm and yarn shims are enabled; corepack keeps the package
# managers it downloads in the same cache.
ENV AXOCOATL_TOOLCHAIN=node \
    AXOCOATL_CACHE=/opt/axocoatl-cache \
    npm_config_cache=/opt/axocoatl-cache/node/npm \
    npm_config_logs_max=0 \
    COREPACK_HOME=/opt/axocoatl-cache/node/corepack \
    COREPACK_ENABLE_DOWNLOAD_PROMPT=0 \
    YARN_CACHE_FOLDER=/opt/axocoatl-cache/node/yarn \
    YARN_GLOBAL_FOLDER=/opt/axocoatl-cache/node/yarn-berry \
    YARN_ENABLE_TELEMETRY=0

RUN export DEBIAN_FRONTEND=noninteractive \
 && apt-get update -qq \
 && apt-get install -y --no-install-recommends make \
 && rm -rf /var/lib/apt/lists/* \
 && corepack enable \
 && mkdir -p /opt/axocoatl-cache \
 && node --version \
 && npm --version

LABEL io.axocoatl.toolchain=node \
      io.axocoatl.toolchain.version=node-24.21.0
