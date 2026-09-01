#!/usr/bin/env bash
# Run the reviewed actionlint version locally and in CI.
set -euo pipefail

version=v1.7.12
shellcheck_version=v0.11.0
# GitHub introduced this hosted Intel runner label after actionlint v1.7.12's
# embedded label catalog. Ignore only that exact catalog warning.
runner_label_ignore='label "macos-15-intel" is unknown'
# GitHub added concurrency.queue in May 2026. actionlint v1.7.12 predates
# that schema addition, so ignore only its exact stale-schema diagnostic; the
# repository contract below validates every production queue structurally.
concurrency_queue_ignore='unexpected key "queue" for "concurrency" section'

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-actionlint.XXXXXX")
trap 'rm -rf -- "$work_dir"' EXIT HUP INT TERM

case "$(uname -s):$(uname -m)" in
  Darwin:arm64)
    shellcheck_platform=darwin.aarch64
    shellcheck_checksum=339b930feb1ea764467013cc1f72d09cd6b869ebf1013296ba9055ab2ffbd26f
    ;;
  Darwin:x86_64)
    shellcheck_platform=darwin.x86_64
    shellcheck_checksum=c2c15e08df0e8fbc374c335b230a7ee958c313fa5714817a59aa59f1aa594f51
    ;;
  Linux:aarch64 | Linux:arm64)
    shellcheck_platform=linux.aarch64
    shellcheck_checksum=68a8133197a50beb8803f8d42f9908d1af1c5540d4bb05fdfca8c1fa47decefc
    ;;
  Linux:x86_64 | Linux:amd64)
    shellcheck_platform=linux.x86_64
    shellcheck_checksum=b7af85e41cc99489dcc21d66c6d5f3685138f06d34651e6d34b42ec6d54fe6f6
    ;;
  *)
    echo "run-actionlint: ShellCheck $shellcheck_version is unavailable for $(uname -s) $(uname -m)" >&2
    exit 1
    ;;
esac

shellcheck_archive="shellcheck-${shellcheck_version}.${shellcheck_platform}.tar.gz"
shellcheck_archive_path="$work_dir/$shellcheck_archive"
curl --fail --location --silent --show-error \
  --output "$shellcheck_archive_path" \
  "https://github.com/koalaman/shellcheck/releases/download/${shellcheck_version}/${shellcheck_archive}"
case "$(uname -s)" in
  Darwin) shellcheck_actual_checksum=$(shasum -a 256 "$shellcheck_archive_path" | awk '{ print $1 }') ;;
  Linux) shellcheck_actual_checksum=$(sha256sum "$shellcheck_archive_path" | awk '{ print $1 }') ;;
esac
[[ "$shellcheck_actual_checksum" == "$shellcheck_checksum" ]] || {
  echo "run-actionlint: ShellCheck archive checksum did not match $shellcheck_checksum" >&2
  exit 1
}
tar -xzf "$shellcheck_archive_path" -C "$work_dir"
shellcheck_path="$work_dir/shellcheck-${shellcheck_version}/shellcheck"
[[ -x "$shellcheck_path" ]] || {
  echo "run-actionlint: ShellCheck archive did not contain the expected executable" >&2
  exit 1
}

[[ "$("$shellcheck_path" --version | awk '$1 == "version:" { print $2 }')" == "${shellcheck_version#v}" ]] \
  || { echo "run-actionlint: ShellCheck version did not match $shellcheck_version" >&2; exit 1; }

command -v go >/dev/null 2>&1 || {
  echo "run-actionlint: Go is required to install actionlint $version" >&2
  exit 1
}
GOBIN="$work_dir/bin" go install "github.com/rhysd/actionlint/cmd/actionlint@$version"
actionlint_path="$work_dir/bin/actionlint"
[[ "$("$actionlint_path" -version | sed -n '1p')" == "$version" ]] \
  || { echo "run-actionlint: installed actionlint version did not match $version" >&2; exit 1; }

unset SHELLCHECK_OPTS
"$actionlint_path" \
  -shellcheck "$shellcheck_path" \
  -ignore "$runner_label_ignore" \
  -ignore "$concurrency_queue_ignore" \
  "$@"
