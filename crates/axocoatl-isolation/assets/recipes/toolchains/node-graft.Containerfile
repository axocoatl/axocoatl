# Node.js for a toolchain image that also holds an agent or e2e recipe, whose
# installs need node and npm. Owner: workstream toolchain.
#
# Node 24.21.0 and its bundled npm and corepack, copied from the same image
# (and image index digest) the recipe base starts from, so every recipe
# fragment finds the Node it was pinned against.
COPY --from=docker.io/library/node:24-bookworm-slim@sha256:d6aa754f16b3197301076f047b5def2f02ea1dbbc2ca920407d46d7ec7f87b20 /usr/local/bin/node /usr/local/bin/node
COPY --from=docker.io/library/node:24-bookworm-slim@sha256:d6aa754f16b3197301076f047b5def2f02ea1dbbc2ca920407d46d7ec7f87b20 /usr/local/lib/node_modules /usr/local/lib/node_modules

ENV NPM_CONFIG_UPDATE_NOTIFIER=false \
    NPM_CONFIG_FUND=false \
    NPM_CONFIG_AUDIT=false

RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
 && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
 && ln -s ../lib/node_modules/corepack/dist/corepack.js /usr/local/bin/corepack \
 && node -e 'const [major, minor] = process.versions.node.split(".").map(Number); if (!(major > 24 || (major === 24 && minor >= 8))) { console.error("the node graft needs Node >= 24.8, found " + process.version); process.exit(1); }' \
 && npm --version >/dev/null

# Checks a downloaded npm tarball against a pinned Subresource Integrity
# value (sha512-...): `axocoatl-verify-integrity <file> <integrity>`.
RUN printf '%s\n' \
      '#!/usr/bin/env node' \
      'const crypto = require("crypto"), fs = require("fs");' \
      'const [file, wanted] = process.argv.slice(2);' \
      'const [algorithm, expected] = wanted.split(/-(.*)/s);' \
      'const actual = crypto.createHash(algorithm).update(fs.readFileSync(file)).digest("base64");' \
      'if (actual !== expected) { console.error("integrity mismatch for " + file + ": " + algorithm + "-" + actual); process.exit(1); }' \
      > /usr/local/bin/axocoatl-verify-integrity \
 && chmod 0755 /usr/local/bin/axocoatl-verify-integrity
