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

The payload must not depend on where things are on the build machine. Panic locations
embed the paths of the compiled source files, including the registry crates under the
Cargo home, so the script remaps the checkout to `/axocoatl-source` and the Cargo home
(`CARGO_HOME`, or `~/.cargo`) to `/cargo-home`, and refuses a payload that still contains
the checkout, Cargo registry or target build directory it used. The checkout path, the
Cargo home and `--target-dir` therefore do not change the bytes. The manifest records
both remaps.

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

The checked-in payloads are built in a container of the Rust 1.95.0 Debian bookworm
image for linux/arm64 with the two musl standard-library targets added, without
network, linking with that toolchain's bundled `rust-lld`. The image is pinned by
digest: `docker.io/library/rust:1.95.0-bookworm` was the index
`sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1`, whose
linux/arm64 image is the one named below. Build it from an empty directory and tag it
`localhost/axocoatl-supervisor-build:20260914` (this step downloads the image and the two
standard libraries; the payload build below runs without network):

```sh
mkdir empty
printf '%s\n' \
  'FROM docker.io/library/rust@sha256:8e45ae5b178fa788bbbd818b42a1f93a6e2c03e7144badd5e0a37087537177e1' \
  'RUN rustup target add --toolchain 1.95.0 aarch64-unknown-linux-musl x86_64-unknown-linux-musl' \
  | podman build --platform linux/arm64 -t localhost/axocoatl-supervisor-build:20260914 -f - empty
rmdir empty
```

In that image `rustc -vV` reports commit `59807616e1fa2540724bfbac14d7976d7e4a3860`, and
the bundled `rust-lld` has SHA-256
`bffd8fec823b2fbf3e91ce38d3dbaf0ffb75694cd3495224eca0da5342577b18`; each manifest
records both. The locked crates come from a Cargo registry mounted as an overlay, so
the build cannot change it. From the repository root:

```sh
mkdir out
podman run --rm --network none -v "$PWD:/src:ro" -v "$PWD/out:/out" \
  -v "$HOME/.cargo/registry:/usr/local/cargo/registry:O" -w /src --entrypoint bash \
  localhost/axocoatl-supervisor-build:20260914 -c '
    set -e
    lld=/usr/local/rustup/toolchains/1.95.0-aarch64-unknown-linux-gnu/lib/rustlib/aarch64-unknown-linux-gnu/bin/rust-lld
    for arch in x86_64 aarch64; do
      ./scripts/build-exec-supervisor.sh "$arch" "/out/$arch" --linker "$lld" --target-dir /tmp/target
    done'
```

Build twice, and also with a different checkout path, Cargo home and target directory,
and compare the SHA-256 of both payloads and manifests: they must be identical. The
1.3.1 payloads were built five times in that image, once with the command above and
four times with `CARGO_HOME` `/cargo-home` or `/usr/local/cargo`, the checkout at `/src`
or `/work/checkout/axocoatl`, and two other target directories; all five gave
byte-identical payloads and manifests. The 1.3.2 payloads, whose source differs from
1.3.1's only in the package version, were built twice, once with the command above and
once with `CARGO_HOME=/cargo-home`, the checkout at `/work/checkout/axocoatl` and another
target directory, with byte-identical payloads and manifests. The 1.4.0 payloads, again
differing only in the package version, were built the same two ways, also byte-identical.
The 1.5.0 payloads differ from 1.4.0's in the package version, in letting hardened
commands rename files into other directories, and in serde_json's `float_roundtrip`
parsing. On 2026-10-23 they were built the same two ways, and a third way with the
checkout at `/opt/vfy/axo`, `CARGO_HOME=/var/cargo-v`, `TMPDIR=/var/tmp`, another target
directory and aarch64 built first; all three gave byte-identical payloads and manifests.
In the image above, the 1.5.0 source from before the serde_json change still builds to
exactly the payloads checked in for it then.

Before replacing the checked-in assets, run the real protocol and process-lifecycle
tests for both architectures in Alpine and Debian containers. ELF inspection alone
cannot prove cancellation, descendant cleanup, output handling, or protocol behavior.
The artifact manifest deliberately records structural verification separately and
does not claim container execution passed.

`scripts/test-exec-supervisor-runtime.sh` runs those tests
(`crates/axocoatl-isolation/tests/supervisor_runtime.rs`) with Podman:

```sh
./scripts/test-exec-supervisor-runtime.sh prepare   # build the test images
./scripts/test-exec-supervisor-runtime.sh run       # run the suite once per image
./scripts/test-exec-supervisor-runtime.sh remove    # remove the test images
```

`prepare` builds eight images from the Containerfiles in
`crates/axocoatl-isolation/tests/fixtures/`: Alpine 3.20 and Debian bookworm-slim, for
linux/amd64 and linux/arm64, each as root and with `USER 1024:1024`, from base images
pinned by digest. It downloads those base images and the Alpine and Debian packages.
The Debian images keep Debian's `/bin/sh` (dash), glibc and coreutils, but their `nc`
and `wget` are Alpine's statically linked BusyBox, because the network test reads
BusyBox messages and Debian bookworm's BusyBox `wget` crashes on a failed connection.
`run` builds and pulls no image; a missing one is an error. For every image it checks
the image's platform, runs the eight tests with the image's uid, and checks that the
program installed in the container is the embedded payload for the image's
architecture; it ends with one PASS or FAIL line per image.

Name an architecture (`run aarch64`, `run x86_64`, and the same for `prepare`) to limit
the run to it. The read-only and network-denying tests need Landlock in the kernel.
User-mode emulation such as qemu-user provides no Landlock, so in an image of a foreign
architecture the supervisor refuses those two restricted commands (as it does on any
kernel without Landlock) and the two tests fail. Run each architecture on a Linux kernel
of that architecture; a Podman machine on Apple silicon is aarch64. A virtual machine
that emulates the other architecture as a whole computer (QEMU without hardware
acceleration, booting a Linux kernel of that architecture) does provide Landlock: the
1.3.1 x86_64 supervisor passed all eight tests that way on an Apple silicon Mac, in an
x86_64 Alpine 3.22 virtual machine (Linux 6.12) with Podman 5.6 installed in it, in each
x86_64 image with rootless Podman and with Podman as root, running a test binary
cross-built for `x86_64-unknown-linux-musl`. On 2026-10-13 the 1.3.2 payloads passed the
same way: the aarch64 one 32 of 32 on a Linux 7.1 aarch64 kernel with rootless Podman 5.8
(older than the Podman 6.1 tested on macOS for 1.5.0),
the x86_64 one 64 of 64 in that x86_64 Alpine virtual machine (Linux 6.12, Podman 5.6).
On 2026-10-13 the 1.4.0 payloads passed the same suites: 32 of 32 on aarch64 and 64 of 64
on x86_64. On 2026-10-23 the 1.5.0 aarch64 payload passed 32 of 32 on a Linux 7.2 aarch64
kernel with rootless Podman 6.1, in a Podman machine on Apple silicon. There the x86_64
one could run only under qemu-user: in each of the four x86_64 images it passed the six
tests that need no Landlock and refused the two restricted commands, as described above.

The test's temporary directory (`TMPDIR`, where each test's Workspace is created) may be
anywhere, `/tmp` included. The test mounts the Workspace in the container at its own path,
and a write restriction drops every writable root that contains the Workspace, so with the
Workspace under `/tmp` a restricted command cannot write `/tmp` itself; it writes the
scratch directory the supervisor makes for it outside the Workspace (its `TMPDIR`). The
read-only test checks both: it protects the Workspace, and then a repository it makes
under the container's `/tmp`, and checks that the command writes its `TMPDIR` (`0700`, its
user's, removed afterwards) but neither repository, and `/tmp` only when the repository
does not lie under it.

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
