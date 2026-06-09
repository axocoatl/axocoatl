# Security Policy

## Supported versions

The `main` branch and the latest `0.x.y` release receive security fixes.
Older versions do not.

## Reporting a vulnerability

**Please do not open a public GitHub issue for security bugs.**

Email **security@axocoatl.ai**, or use GitHub's private vulnerability reporting
(the **Report a vulnerability** button on the repository's Security tab), with:

- A clear description of the issue and its impact.
- Steps to reproduce, ideally with a minimal repro repo or snippet.
- The version / commit you observed it on.
- Your contact info for follow-up.

We will acknowledge receipt within **3 business days** and aim to have a fix
or mitigation plan within **14 days** for high-severity issues. Lower-severity
fixes may roll into the next regular release.

## Scope

In scope:

- Axocoatl daemon, CLI, server, and library crates in this repository.
- The dashboard served by `axocoatl-server` (XSS, CSRF, privilege escalation,
  data exfiltration).
- The MCP approval/permission gate.
- The session sandbox (container escape, host filesystem leakage).

Out of scope:

- Vulnerabilities in upstream dependencies (please report to that project;
  we will pull in the fix once it lands).
- LLM provider behavior — Axocoatl trusts the configured provider's responses.
- Misconfiguration of external services (Ollama, podman, etc.) outside
  Axocoatl's default setup.

## Disclosure

We follow coordinated disclosure. Once a patch is available, we will:

1. Publish a release with the fix and a brief security advisory.
2. Credit reporters who wish to be named.

Thanks for helping keep Axocoatl users safe.
