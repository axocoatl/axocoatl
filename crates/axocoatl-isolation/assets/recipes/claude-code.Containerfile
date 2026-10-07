
# Recipe fragment: Claude Code CLI 2.1.292, the native build of
# @anthropic-ai/claude-code for this machine (linux-x64 or linux-arm64,
# glibc), pinned by version and npm integrity. Owner: workstream agents.
# Run headless by Axocoatl as an external writer: `claude -p` with
# stream-json output, as the Session's non-root writer user, through the
# Session's egress routes (the OAuth token stays on the host).
ENV DISABLE_AUTOUPDATER=1 \
    DISABLE_TELEMETRY=1 \
    DISABLE_ERROR_REPORTING=1 \
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1

RUN set -eu; \
    case "$(uname -m)" in \
      x86_64) platform=linux-x64; integrity='sha512-DNQrJZomQAX23jkJXnfYtIyJdSf5CBnexXwO2hABCs7oiZpa3IbPWIo7K6SPk5tzRIERzbwpfK3tmija1C7+kQ==' ;; \
      aarch64) platform=linux-arm64; integrity='sha512-tkYfopUsz3AbyprMXvaXlTUewee0Of6fKd+i7Mczb0PFVi3C5SN72WqTwL7ONLD1HyoQ63Tj7TOUUOCPLf/0/w==' ;; \
      *) echo "claude-code: no pinned build for $(uname -m)" >&2; exit 1 ;; \
    esac; \
    work=$(mktemp -d); cd "$work"; \
    npm pack --silent "@anthropic-ai/claude-code-${platform}@2.1.292" >/dev/null; \
    axocoatl-verify-integrity "anthropic-ai-claude-code-${platform}-2.1.292.tgz" "$integrity"; \
    mkdir -p /opt/axocoatl/claude-code; \
    tar -xzf "anthropic-ai-claude-code-${platform}-2.1.292.tgz" -C /opt/axocoatl/claude-code --strip-components=1 --no-same-owner; \
    chmod -R a+rX /opt/axocoatl/claude-code; \
    ln -s /opt/axocoatl/claude-code/claude /usr/local/bin/claude; \
    cd /; rm -rf "$work" /root/.npm; \
    claude --version

LABEL io.axocoatl.recipe.claude-code=2.1.292
