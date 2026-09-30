#!/usr/bin/env bash
# Build a Linux supervisor payload without installing tools or downloading inputs.
set -euo pipefail

fail() { echo "build-exec-supervisor: $*" >&2; exit 1; }
usage() {
  cat <<'USAGE'
Usage: build-exec-supervisor.sh <x86_64|aarch64> <new-output-directory> --linker <installed-program> [--target-dir <directory>]

Prerequisites must be installed explicitly: Rust 1.95.0, its matching
<architecture>-unknown-linux-musl target, a suitable Linux linker, Python >=3.11,
and the locked Cargo dependencies. The build runs offline with one job.
No prebuilt-artifact bypass, toolchain install, model/container launch, or host
installation is performed. Output ELF checks do not replace running the real
helper's protocol tests in both Alpine and Debian containers.
USAGE
}

if [[ "${1:-}" == --help ]]; then usage; exit 0; fi
[[ $# -ge 4 ]] || { usage >&2; exit 1; }
architecture=$1
output_directory=$2
shift 2
case "$architecture" in x86_64|aarch64) ;; *) fail 'architecture must be x86_64 or aarch64' ;; esac
script_directory="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source_root="$(CDPATH= cd -- "$script_directory/.." && pwd)"
build_target_directory="$source_root/target/exec-supervisor"
linker=''
while [[ $# -gt 0 ]]; do
  case "$1" in
    --linker) [[ $# -ge 2 ]] || fail '--linker needs a value'; linker=$2; shift 2 ;;
    --target-dir) [[ $# -ge 2 ]] || fail '--target-dir needs a value'; build_target_directory=$2; shift 2 ;;
    *) fail "unknown argument: $1" ;;
  esac
done
[[ -n "$linker" ]] || fail 'an explicit installed Linux linker is required'
[[ ! -e "$output_directory" && ! -L "$output_directory" ]] || fail 'output directory already exists'
command -v python3 >/dev/null 2>&1 || fail 'Python >=3.11 is required'
python3 -c 'import sys; sys.exit(0 if sys.version_info >= (3, 11) else 1)' || fail 'Python >=3.11 is required'
command -v rustup >/dev/null 2>&1 || fail 'rustup with already-installed Rust 1.95.0 is required'
toolchain=1.95.0
target="$architecture-unknown-linux-musl"
# rustup run has no --install flag: a missing toolchain fails without fetching it.
rustup run "$toolchain" rustc --version | awk '$1 == "rustc" && $2 == "1.95.0" { found=1 } END { exit !found }' \
  || fail 'installed Rust 1.95.0 is required; install prerequisites explicitly'
rustup target list --installed --toolchain "$toolchain" | awk -v target="$target" '$0 == target { found=1 } END { exit !found }' \
  || fail "installed target $target is required; install prerequisites explicitly"
linker="$(command -v -- "$linker")" || fail 'the requested linker is not installed'
[[ -f "$linker" && -x "$linker" ]] || fail 'linker must resolve to an executable file'
linker="$(python3 -c 'import pathlib,sys; print(pathlib.Path(sys.argv[1]).resolve(strict=True))' "$linker")"
build_target_directory="$(python3 -c 'import pathlib,sys; print(pathlib.Path(sys.argv[1]).absolute())' "$build_target_directory")"
for setting in RUSTFLAGS CARGO_ENCODED_RUSTFLAGS RUSTC RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER; do
  [[ -z "${!setting-}" ]] || fail "unset $setting for this controlled build"
done
if compgen -e | awk '/^CARGO_PROFILE_RELEASE_/ { found=1 } END { exit !found }'; then
  fail 'unset CARGO_PROFILE_RELEASE_* overrides for this controlled build'
fi

temporary_directory="$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-exec-build.XXXXXX")"
trap 'rm -rf -- "$temporary_directory"' EXIT
validator="$script_directory/test-exec-supervisor-artifact.py"
python3 "$validator" snapshot "$source_root" "$temporary_directory/source.json"
rustup run "$toolchain" rustc -vV > "$temporary_directory/rustc.txt"
linker_literal="$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$linker")"
echo "Building axocoatl-exec-supervisor for $target with installed Rust $toolchain"
flag_separator=$'\x1f'
(
  cd -- "$source_root"
  CARGO_ENCODED_RUSTFLAGS="-C${flag_separator}target-feature=+crt-static${flag_separator}--remap-path-prefix=$source_root=/axocoatl-source" \
    CARGO_INCREMENTAL=0 \
    rustup run "$toolchain" cargo build --locked --offline --release --jobs 1 \
      --manifest-path "$source_root/Cargo.toml" -p axocoatl-exec --bin axocoatl-exec-supervisor \
      --target "$target" --target-dir "$build_target_directory" \
      --config "target.$target.linker=$linker_literal" \
      --config 'build.rustc-wrapper=""' --config 'build.rustc-workspace-wrapper=""'
)
python3 "$validator" package "$source_root" "$temporary_directory/source.json" \
  "$build_target_directory/$target/release/axocoatl-exec-supervisor" \
  "$architecture" "$output_directory" "$temporary_directory/rustc.txt" "$linker"
python3 "$validator" verify "$output_directory" "$architecture" --source-root "$source_root"
echo "Built and structurally verified $output_directory (container execution proof still required)"
