
# Recipe fragment: Codex CLI 0.160.1, the native build of @openai/codex for
# this machine (linux-x64 or linux-arm64, static musl), pinned by version and
# npm integrity. Owner: workstream agents. Run headless by Axocoatl as an
# external writer: `codex exec --json`, as the Session's non-root writer
# user, through the Session's egress routes (the API key stays on the host).
RUN set -eu; \
    case "$(uname -m)" in \
      x86_64) platform=linux-x64; triple=x86_64-unknown-linux-musl; integrity='sha512-sIDhqV+bsZKKVaVFVY5iB+pAzyOz2XRm3H1KXCsSJwGj5p98qnrT0Fweo1Hfe7WnyTp8mfBNdbVzUeO+mHYugA==' ;; \
      aarch64) platform=linux-arm64; triple=aarch64-unknown-linux-musl; integrity='sha512-JLyjBlmjPvwTaicHemw+y5xQSz3Uja2r7F/xkvS8gFufTA2g132Ak+0fo35xnJ/k2uc9fTtjm0LrsEGI78L6Ng==' ;; \
      *) echo "codex: no pinned build for $(uname -m)" >&2; exit 1 ;; \
    esac; \
    work=$(mktemp -d); cd "$work"; \
    npm pack --silent "@openai/codex@0.160.1-${platform}" >/dev/null; \
    axocoatl-verify-integrity "openai-codex-0.160.1-${platform}.tgz" "$integrity"; \
    mkdir -p /opt/axocoatl/codex; \
    tar -xzf "openai-codex-0.160.1-${platform}.tgz" -C /opt/axocoatl/codex --strip-components=3 --no-same-owner "package/vendor/${triple}"; \
    chmod -R a+rX /opt/axocoatl/codex; \
    ln -s /opt/axocoatl/codex/bin/codex /usr/local/bin/codex; \
    cd /; rm -rf "$work" /root/.npm; \
    codex --version

LABEL io.axocoatl.recipe.codex=0.160.1
