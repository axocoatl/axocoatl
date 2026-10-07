# Base of every Axocoatl recipe image (`axocoatl recipe build`). Owner:
# workstream agents.
#
# Node 24.21.0 on Debian 12 (bookworm) slim, pinned by the image index
# digest: Node >= 24.8 holds for every fragment (e2e@0.18.0 needs it). Git
# and the CA bundle complete the POSIX and Git command surface Axocoatl
# probes Session images for (sh, git, env, grep, tee, rm, mkdir, mv, cp,
# cat, head, wc, find, realpath, ls, rmdir, test, sha256sum, base64), so a
# Session on this image never provisions packages at start. The image runs
# as root, like Axocoatl's curated images; hardened Sessions run every
# command as their own non-root workload users.
FROM docker.io/library/node:24-bookworm-slim@sha256:d6aa754f16b3197301076f047b5def2f02ea1dbbc2ca920407d46d7ec7f87b20

ENV NPM_CONFIG_UPDATE_NOTIFIER=false \
    NPM_CONFIG_FUND=false \
    NPM_CONFIG_AUDIT=false

RUN export DEBIAN_FRONTEND=noninteractive \
 && apt-get update -qq \
 && apt-get install -y --no-install-recommends ca-certificates git \
 && rm -rf /var/lib/apt/lists/* \
 && node -e 'const [major, minor] = process.versions.node.split(".").map(Number); if (!(major > 24 || (major === 24 && minor >= 8))) { console.error("recipe base needs Node >= 24.8, found " + process.version); process.exit(1); }' \
 && for command in sh git env grep tee rm mkdir mv cp cat head wc find realpath ls rmdir test sha256sum base64; do \
      command -v "$command" >/dev/null || { echo "recipe base lacks $command" >&2; exit 1; }; \
    done

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

LABEL io.axocoatl.role=recipe \
      io.axocoatl.recipe.base=node-24.21.0-bookworm
