#!/usr/bin/env bash
# Refuse a release binary built with the verification-only `live-verify`
# cargo feature (axocoatl-daemon, axocoatl-cli). That feature's stale-access
# switch is the only code that names AXOCOATL_VERIFY_STALE_ACCESS_ONCE or logs
# "VERIFICATION SWITCH"; a binary without the feature holds neither string.
set -euo pipefail
export LC_ALL=C

fail() {
  echo "check-no-live-verify: $*" >&2
  exit 1
}

[[ $# -ge 1 ]] || {
  echo 'Usage: check-no-live-verify.sh <binary>...' >&2
  exit 2
}

for binary in "$@"; do
  [[ -f "$binary" ]] || fail "no binary at $binary"
  for marker in AXOCOATL_VERIFY_STALE_ACCESS_ONCE 'VERIFICATION SWITCH'; do
    if grep -a -q -F -- "$marker" "$binary"; then
      fail "$binary was built with the verification-only live-verify feature (it holds \"$marker\"); release builds must not enable it"
    fi
  done
  echo "check-no-live-verify: $binary has no verification switch"
done
