# Toolchain recipe: rust. Owner: workstream toolchain.
# Appended to the rust toolchain base (Rust 1.99.1 on Debian 12 slim, git,
# make). Cargo's registry lives in the Workspace's toolchain cache volume
# (mounted at /opt/axocoatl-cache), which only a run's setup writes: a
# second run needs no download. The build directory is the Session's own,
# under /tmp, so no target/ directory is written into the repository and
# nothing one run builds is reused by another.
ENV AXOCOATL_TOOLCHAIN=rust \
    AXOCOATL_CACHE=/opt/axocoatl-cache \
    CARGO_HOME=/opt/axocoatl-cache/rust/cargo \
    CARGO_TARGET_DIR=/tmp/axocoatl-cargo-target \
    CARGO_TERM_COLOR=never

RUN mkdir -p /opt/axocoatl-cache \
 && cargo --version \
 && rustc --version

LABEL io.axocoatl.toolchain=rust \
      io.axocoatl.toolchain.version=rust-1.99.1
