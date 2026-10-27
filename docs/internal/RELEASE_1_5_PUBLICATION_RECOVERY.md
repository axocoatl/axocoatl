# 1.5.0 publication recovery

The product source and release tag remain
`19e3c89ca7b4d02cd742bccd671f7274e68b95d7`.

Release run [40439869836](https://github.com/axocoatl/axocoatl/actions/runs/40439869836),
attempt 1, passed all 15 prerequisite, native/archive, provenance and Linux
installation jobs. Its draft creation failed before any crate or public release
publication. GitHub rejected the exact changelog body with HTTP 422:
`body is too long (maximum is 125000 characters)`.

The extracted 1.5.0 section is 163,777 characters / 163,861 UTF-8 bytes.
Its SHA-256 is
`4c16b22931627111b5db7ddb44d0fa1f4b1c542edc47fcafc99ad12f851d42b6`.
The complete changelog remains unchanged in the frozen source.

## Publication control

`recover-v1.5.0.yml` is a manually dispatched, incident-specific publication
workflow. Its confirmation binds the frozen tag, source run and current control
commit. It shares the ordinary production release concurrency group.

Before publishing it verifies the complete original attempt's exact job IDs,
names, conclusions, sole failed draft step, and the exact verified archive
artifact ID, name, digest, source and expiry. It rejects incomplete responses or
any different source state. It separately checks the unchanged remote tag, main
ancestry, signed new-user evidence and public release order. Only the listed
publication-control files may differ between the frozen release and control
commit.

The workflow downloads `verified-release-40439869836-v1.5.0` (artifact
`12711066172`), rechecks every archive and checksum, and verifies the original
`release.yml` attestations against the frozen tag and source. It does not rebuild
the native binaries or relabel the failed original workflow as successful.

All remaining draft reconciliation, 22-crate packaging/publication, registry API
and sparse-index proof, public-release verification, and deployment steps retain
the ordinary workflow's checks. Source checkouts and packaging stay on the frozen
release commit, separate from publication-control files. Published crate proof is
bound to the recovery run. Both site workflows receive the frozen release tag and
commit explicitly.

The only release-body change is deterministic formatting through
`scripts/release-notes.py`. Sections at or below 125,000 UTF-8 bytes remain
byte-for-byte identical to the ordinary extraction. Oversized sections retain
their heading and link to the full changelog at the frozen commit, with the full
extracted section's digest. The same output is used for creation, reconciliation
and final verification. No partial list, subsection or code fence is published.
For this release the bounded body is 467 bytes, SHA-256
`347a07928cd09e61ff5c9c6b8746be9e2980a362ec11cf7587d914c73050b3a5`.

## Verification state

- Original product, browser, site, evidence, archive and Linux install gates: passed.
- Original workflow's publishing status: failed at draft creation; preserved.
- Recovery helper checks: size/Unicode boundaries, exact extraction parity,
  source/attempt/job/artifact binding failures, and workflow publication invariants.
- Recovery publication and live-site checks: passed; completion evidence follows.

Completion requires the recovery workflow to succeed, all four archives and
checksums to be public, all 22 exact crate versions to be non-yanked with proved
checksums/index state, the public release to be stable/latest, and both custom
domains to serve their frozen-source deployment payloads.

## Published result

[Recovery run 40454462239](https://github.com/axocoatl/axocoatl/actions/runs/40454462239)
completed successfully from publication-control commit
`869ad241f675a0e5f1cd97865fe5cb6622a34e91`.
[Axocoatl 1.5.0](https://github.com/axocoatl/axocoatl/releases/tag/v1.5.0)
was published at 2026-10-27 05:32:01 UTC (October 26, 10:32 PM PDT),
release ID `425060896`. The frozen product tag remains unchanged.

The final public API and download checks verified the stable/latest release,
exact bounded notes, all four platform archives and their four checksum files,
and all 22 non-yanked crate versions. The publication workflow also proved the
crate checksums through registry API and sparse-index responses.

Both frozen-source site deployments succeeded:

- Marketing: <https://116c0f28.axocoatl-marketing.pages.dev>
- Documentation: <https://99355788.axocoatl-docs.pages.dev>

The custom-domain homepage and changelog, documentation homepage and install
page, and installer all returned HTTP 200. The public installer is byte-identical
to both its deployment and frozen source, SHA-256
`9e0adcbead672c87a287f0267d0c06f58e9a87ede9e2ce1a24e4774926399334`.

The initial raw HTML comparison failed and is retained as such. Examination of
the saved responses proved the only differences on all four HTML paths: the
custom domain adds one 367-byte Cloudflare analytics script, and removes one pair
of `email_off` comment markers on each page except the marketing changelog.
Removing exactly those observed edge transformations leaves byte-identical HTML
for every checked path. The script and following newline have SHA-256
`62dd494351ede212709991565f73b3e551b7c59d9d33264c58b1eac0d8f945b8`.
No site content, hosting settings, or frozen release artifacts were changed to
satisfy this comparison. The original failed publication run remains failed;
the separate recovery run establishes completed publication.
