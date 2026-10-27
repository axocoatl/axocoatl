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
- Recovery publication and live-site checks: pending at this control commit.

Completion requires the recovery workflow to succeed, all four archives and
checksums to be public, all 22 exact crate versions to be non-yanked with proved
checksums/index state, the public release to be stable/latest, and both custom
domains to serve their frozen-source deployment payloads.
