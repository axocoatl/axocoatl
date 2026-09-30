# Building the embedded process supervisor

Axocoatl is distributed as one `axocoatl` executable. Its Linux process supervisor
is first-party code in `crates/axocoatl-exec`. The host application embeds the Linux
x86_64 and aarch64 payloads from
`crates/axocoatl-isolation/assets/exec-supervisor`; an ordinary Cargo or source
installation compiles the host and includes those prebuilt bytes. Installation
does not acquire another executable, and startup does not download the supervisor.

The supervisor runs inside the selected Linux sandbox. Its source implements the
bounded command protocol, cancellation, child-process reaping, and separate command
exit and process-quiescence results. Packaging verification does not establish that
every application execution path uses this boundary. Runtime integration and
end-to-end command tests are separate requirements.

## Rebuild the payloads

These are contributor commands, not additional installation steps. Prepare the
following build dependencies explicitly:

- Rust 1.95.0 with both `x86_64-unknown-linux-musl` and
  `aarch64-unknown-linux-musl` standard-library targets;
- a Linux ELF linker supporting the selected architecture, such as Rust's bundled
  `rust-lld`;
- Python 3.11 or newer;
- the repository's locked Cargo dependencies in the local Cargo cache.

The build script does not install toolchains or download dependencies. It runs
Cargo with `--locked --offline`, static musl linking, the repository release profile,
and one build job. It rejects compiler, release-profile, and wrapper environment
overrides instead of silently recording a different build as the standard payload.

From the repository root, choose new output directories and pass the installed
linker by its actual path:

```sh
./scripts/build-exec-supervisor.sh x86_64 /tmp/axocoatl-exec-x86_64 \
  --linker /absolute/path/to/rust-lld
./scripts/build-exec-supervisor.sh aarch64 /tmp/axocoatl-exec-aarch64 \
  --linker /absolute/path/to/rust-lld
```

An existing output directory is rejected. `--target-dir /absolute/path` optionally
selects a separate Cargo build cache. Each output directory contains one Linux
payload and a JSON manifest recording its SHA-256, ELF properties, compiler and
linker identity, package and protocol versions, and source provenance.

Before replacing the checked-in assets, run the real protocol and process-lifecycle
tests for both architectures in Alpine and Debian containers. ELF inspection alone
cannot prove cancellation, descendant cleanup, output handling, or protocol behavior.
The artifact manifest deliberately records structural verification separately and
does not claim container execution passed.

After validation, replace the two payloads and their matching manifests together,
and update the embedded descriptor's pinned identities. The installer and normal
Cargo commands remain unchanged.

## Source and package checks

The helper build records a complete snapshot of its workspace build inputs before
and after compilation. Ongoing freshness checks compare the helper's exact
manifest, inherited workspace declarations, locked dependency closure, build
profiles and configuration, and build recipe. An unrelated CLI version change
therefore leaves valid supervisor payloads usable; a changed supervisor dependency
requires rebuilding them. The complete original workspace snapshot remains build
provenance.

A separate fingerprint covers the package version,
`build.rs`, and every Rust file beneath `src/`. `axocoatl-exec` computes that
fingerprint during compilation; the host checks it against the embedded payload
identity. A protocol change changes the source fingerprint as well as the explicit
protocol-version check.

Cargo rewrites manifests and lockfiles when it creates a package. Those rewritten
bytes are not presented as identical to the original workspace. Package validation
instead checks that the actual `axocoatl-exec` archive retains the exact fingerprinted
source, that the `axocoatl-isolation` archive includes both matching payloads and
manifests, and that their bytes match the reviewed checkout.

```sh
python3 scripts/test-exec-supervisor-artifact.py self-test
python3 scripts/test-exec-supervisor-artifact.py verify-embedded .
python3 scripts/test-exec-supervisor-artifact.py verify-packages \
  target/package/axocoatl-exec-1.0.0.crate \
  target/package/axocoatl-isolation-1.0.0.crate --source-root .
```

Use the actual package versions for archive names. The self-tests use synthetic ELF
fixtures to test the validator; they never execute those fixtures and do not count
as a successful supervisor build or runtime test.
