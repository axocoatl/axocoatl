# Axocoatl documentation site

The public documentation for Axocoatl. The site is organized around complete
tasks: start the runtime, use the workbench, configure it, operate it, understand
its internals, and look up exact interfaces.

## Work locally

Run these commands from `sites/docs`:

```sh
npm ci
node scripts/audit-gate.mjs
npm run check:content
npm run build
npm run check:links
npm run preview
```

`prebuild.sh` copies the canonical brand assets from `branding/` into the ignored
`public/` output before a build. Do not edit those generated copies.
The built-link check verifies internal `href`/`src` targets and fails when the
canonical favicon was not copied into `dist/`.

## Dependency audit exceptions

`scripts/audit-gate.mjs` audits the site's dependencies. The documentation gate
(`scripts/verify-docs-gate.sh`) runs it after `npm ci`, and the Security workflow
runs `node scripts/audit-gate.mjs --package-lock-only` whenever the lockfile or the
exceptions change and once a day. It runs `npm audit --json` and fails on every
high or critical advisory that `audit-exceptions.json` does not cover. Moderate and
lower advisories are printed but do not fail the gate.

An exception is for an advisory that has no fixed version and does not apply to
how this site uses the package. Upgrade instead whenever a fix exists. Each entry
records:

| Field | Meaning |
|---|---|
| `advisory` | The GitHub advisory id, for example `GHSA-ch52-4w7c-c8xp`. |
| `package` | The vulnerable package. |
| `vulnerable` | The vulnerable range as reviewed, for example `<=4.2.0`. |
| `dependents` | The packages that depend on it directly. |
| `scope` | `build`: the package runs only while the site is built and is not part of the published files. |
| `reason` | Why the advisory does not apply here. |
| `reviewed` | The date of the review (`YYYY-MM-DD`, UTC). |
| `expires` | The last day the exception holds, at most 90 days after `reviewed`. |

An exception covers an advisory only while all of these hold, so it lapses on its
own:

- today (UTC) is on or before `expires`;
- the package's published `latest` version is still inside `vulnerable` and inside
  the range npm reports for the advisory, so a published fix ends the exception;
- in `package-lock.json`, aliased copies included: every package that depends
  directly on an installed copy of the package is one of `dependents`; every
  dependency path to such a copy from this site, its workspaces and its linked
  folders goes through one of them; and every installed copy can be traced to one
  of those starting points, so a copy the lockfile does not connect fails the gate.

When the gate fails on an exception, upgrade if a fix exists; otherwise review the
advisory again and update `vulnerable`, `dependents`, `reviewed` and `expires`.
The gate warns about exceptions that no longer match a high or critical advisory;
remove them.

`node --test scripts/audit-gate.test.mjs` tests the gate against recorded audit
output in `scripts/audit-gate-fixtures/`; the documentation gate runs it too.

## Source discipline

- Product behavior comes from the current repository and `docs/PRODUCT.md`.
- Runtime behavior comes from `docs/ARCHITECTURE.md` and current source.
- Voice and visual identity come from `BRAND.md`.
- CLI and HTTP reference pages are checked against the command and router source by
  `npm run check:content`, and every page must be in the sidebar.
- Performance and quality claims come only from measured results listed in
  `docs/CLAIMS.md`. `npm run check:content` also checks the public surfaces (this site,
  the README, `llms.txt`, `docs/PRODUCT.md`, `docs/ARCHITECTURE.md` and the marketing
  pages): every `measured:` block id must be in the ledger, withdrawn claims must not
  reappear, Claude Opus results must be labeled as Claude Code subagents and not
  Axocoatl, and measured review results next to OpenRouter must say that OpenRouter
  reviewers were not measured.

Do not publish, deploy, or change marketing copy from this package.
