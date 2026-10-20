# Toolchain recipe: rust. Owner: workstream toolchain.
# Appended to the rust toolchain base (Rust 1.99.1 on Debian 12 slim, git).
# Cargo's registry and the build directory live in the Workspace's
# toolchain cache volume (mounted at /opt/axocoatl-cache): a second run
# needs no download and rebuilds only what changed, and no target/
# directory is written into the repository.
ENV AXOCOATL_TOOLCHAIN=rust \
    AXOCOATL_CACHE=/opt/axocoatl-cache \
    CARGO_HOME=/opt/axocoatl-cache/rust/cargo \
    CARGO_TARGET_DIR=/opt/axocoatl-cache/rust/target \
    CARGO_TERM_COLOR=never

RUN mkdir -p /opt/axocoatl-cache \
 && cargo --version \
 && rustc --version

LABEL io.axocoatl.toolchain=rust \
      io.axocoatl.toolchain.version=rust-1.99.1
