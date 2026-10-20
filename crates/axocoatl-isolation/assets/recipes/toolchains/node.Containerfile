# Toolchain recipe: node. Owner: workstream toolchain.
# Appended to the recipe base (Node 24.21.0 on Debian 12 slim, git). The
# caches live under /opt/axocoatl-cache, where a loadout run mounts the
# Workspace's toolchain cache volume, so a second run installs from the
# cache instead of the network. Corepack's pnpm and yarn shims are enabled;
# corepack keeps the package managers it downloads in the same cache.
ENV AXOCOATL_TOOLCHAIN=node \
    AXOCOATL_CACHE=/opt/axocoatl-cache \
    npm_config_cache=/opt/axocoatl-cache/node/npm \
    COREPACK_HOME=/opt/axocoatl-cache/node/corepack \
    COREPACK_ENABLE_DOWNLOAD_PROMPT=0 \
    YARN_CACHE_FOLDER=/opt/axocoatl-cache/node/yarn

RUN corepack enable \
 && mkdir -p /opt/axocoatl-cache \
 && node --version \
 && npm --version

LABEL io.axocoatl.toolchain=node \
      io.axocoatl.toolchain.version=node-24.21.0
