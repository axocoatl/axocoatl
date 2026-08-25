#!/bin/sh
# End-to-end installer regressions with a fully local release fixture.
set -eu

if [ "${TEST_INSTALL_TOOL_MODE:-}" = "1" ]; then
  case "${0##*/}" in
    uname)
      case "${1:-}" in
        -s) printf '%s\n' "${TEST_OS:-Linux}" ;;
        -m) printf '%s\n' "${TEST_ARCH:-x86_64}" ;;
        *) exit 2 ;;
      esac
      ;;
    getconf)
      [ "${1:-}" = "GNU_LIBC_VERSION" ] || exit 2
      [ "${TEST_GLIBC:-glibc 2.35}" != "unavailable" ] || exit 1
      printf '%s\n' "${TEST_GLIBC:-glibc 2.35}"
      ;;
    sw_vers)
      [ "${1:-}" = "-productVersion" ] || exit 2
      [ "${TEST_MACOS:-15.0}" != "unavailable" ] || exit 1
      if [ "${TEST_MACOS:-15.0}" = "compat" ]; then
        case "${SYSTEM_VERSION_COMPAT:-}" in
          0) printf '%s\n' 11.0 ;;
          *) printf '%s\n' 10.16 ;;
        esac
      else
        printf '%s\n' "${TEST_MACOS:-15.0}"
      fi
      ;;
    curl)
      output=""
      url=""
      while [ "$#" -gt 0 ]; do
        case "$1" in
          -o)
            [ "$#" -ge 2 ] || exit 2
            output=$2
            shift 2
            ;;
          -*) shift ;;
          *) url=$1; shift ;;
        esac
      done
      [ -n "$url" ] || exit 2
      printf '%s\n' "$url" >> "$TEST_CURL_LOG"
      case "$url" in
        */releases/latest)
          printf '%s\n' '{"tag_name":"v1.0.0"}'
          ;;
        *.tar.gz.sha256)
          [ -n "$output" ] || exit 2
          archive_name="${url##*/}"
          archive_name="${archive_name%.sha256}"
          case "${TEST_CHECKSUM_MODE:-valid}" in
            valid) printf '%s  %s\n' "$TEST_DIGEST" "$archive_name" > "$output" ;;
            missing) exit 22 ;;
            malformed) printf '%s\n' "not-a-sha256  $archive_name" > "$output" ;;
            mismatch) printf '%064d  %s\n' 0 "$archive_name" > "$output" ;;
            *) exit 2 ;;
          esac
          ;;
        *.tar.gz)
          [ -n "$output" ] || exit 2
          /bin/cp "$TEST_FIXTURE_ARCHIVE" "$output"
          ;;
        *) exit 22 ;;
      esac
      ;;
    sha256sum)
      printf '%s  %s\n' "$TEST_DIGEST" "$1"
      ;;
    install)
      printf '%s\n' "$*" >> "$TEST_INSTALL_LOG"
      ;;
    *) exit 127 ;;
  esac
  exit 0
fi

script_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
installer="$script_dir/install.sh"
test_driver="$script_dir/test-install.sh"
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-install-test.XXXXXX")"

cleanup() {
  rm -rf "$work_dir"
}
trap cleanup 0 HUP INT TERM

fail() {
  echo "test-install: $*" >&2
  exit 1
}

fixture_dir="$work_dir/fixture"
mkdir -p "$fixture_dir"
cat > "$fixture_dir/axocoatl" <<'EOF'
#!/bin/sh
echo 'axocoatl 1.0.0'
EOF
chmod 0755 "$fixture_dir/axocoatl"
printf '%s\n' 'Axocoatl license fixture' > "$fixture_dir/LICENSE"
printf '%s\n' 'Axocoatl third-party notice fixture' > "$fixture_dir/THIRD_PARTY_LICENSES.txt"
fixture_archive="$work_dir/axocoatl-v1.0.0-fixture.tar.gz"
tar -czf "$fixture_archive" -C "$fixture_dir" \
  axocoatl LICENSE THIRD_PARTY_LICENSES.txt

fake_bin="$work_dir/bin"
no_hash_bin="$work_dir/bin-no-hash"
mkdir -p "$fake_bin" "$no_hash_bin"

link_tool() {
  target=$1
  name=$2
  ln -s "$target" "$fake_bin/$name"
  ln -s "$target" "$no_hash_bin/$name"
}

for tool in awk cut grep gzip head mkdir mktemp rm tar tr; do
  tool_path="$(command -v "$tool")"
  [ -n "$tool_path" ] || fail "required test host tool is missing: $tool"
  link_tool "$tool_path" "$tool"
done
for tool in curl getconf install sw_vers uname; do
  link_tool "$test_driver" "$tool"
done
ln -s "$test_driver" "$fake_bin/sha256sum"

digest="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
case_count=0

run_case() {
  name=$1
  expected=$2
  expected_text=$3
  os=$4
  glibc=$5
  macos=$6
  checksum=$7
  path=$8

  case_count=$((case_count + 1))
  output="$work_dir/$name.out"
  curl_log="$work_dir/$name.curl"
  install_log="$work_dir/$name.install"
  : > "$curl_log"
  : > "$install_log"
  case_tmp="$work_dir/$name-tmp"
  case_home="$work_dir/$name-home"
  mkdir -p "$case_tmp" "$case_home"

  set +e
  TEST_OS="$os" \
  TEST_INSTALL_TOOL_MODE=1 \
  TEST_ARCH=x86_64 \
  TEST_GLIBC="$glibc" \
  TEST_MACOS="$macos" \
  TEST_CHECKSUM_MODE="$checksum" \
  TEST_DIGEST="$digest" \
  TEST_FIXTURE_ARCHIVE="$fixture_archive" \
  TEST_CURL_LOG="$curl_log" \
  TEST_INSTALL_LOG="$install_log" \
  TMPDIR="$case_tmp" \
  HOME="$case_home" \
  PATH="$path" \
    /bin/sh "$installer" > "$output" 2>&1
  status=$?
  set -e

  case "$expected" in
    pass)
      [ "$status" -eq 0 ] || fail "$name unexpectedly failed: $(cat "$output")"
      [ -s "$install_log" ] || fail "$name did not reach the isolated install stub"
      ;;
    fail)
      [ "$status" -ne 0 ] || fail "$name unexpectedly succeeded"
      ;;
    *) fail "invalid expectation for $name: $expected" ;;
  esac
  grep -F "$expected_text" "$output" >/dev/null \
    || fail "$name did not report '$expected_text': $(cat "$output")"
}

run_case linux-glibc-floor pass "checksum verified" Linux "glibc 2.35" unavailable valid "$fake_bin"
run_case darwin-macos-floor pass "checksum verified" Darwin unavailable 11.0 valid "$fake_bin"
run_case darwin-macos-compat pass "checksum verified" Darwin unavailable compat valid "$fake_bin"
run_case darwin-old-macos fail "require macOS 11 or newer; detected macOS 10.15.7" Darwin unavailable 10.15.7 valid "$fake_bin"
run_case darwin-version-unavailable fail "cannot report its macOS version" Darwin unavailable unavailable valid "$fake_bin"
run_case linux-old-glibc fail "require GNU libc 2.35 or newer" Linux "glibc 2.34" unavailable valid "$fake_bin"
run_case linux-musl fail "Alpine/musl needs a source build" Linux "musl libc 1.2.5" unavailable valid "$fake_bin"
run_case windows-native fail "runs on Windows through WSL2" Windows_NT unavailable unavailable valid "$fake_bin"
run_case checksum-missing fail "checksum download failed" Linux "glibc 2.35" unavailable missing "$fake_bin"
run_case checksum-malformed fail "malformed checksum file" Linux "glibc 2.35" unavailable malformed "$fake_bin"
run_case checksum-mismatch fail "checksum mismatch" Linux "glibc 2.35" unavailable mismatch "$fake_bin"
run_case checksum-tool-missing fail "neither sha256sum nor shasum is available" Linux "glibc 2.35" unavailable valid "$no_hash_bin"

[ ! -s "$work_dir/linux-old-glibc.curl" ] || fail "old glibc reached the network stub"
[ ! -s "$work_dir/linux-musl.curl" ] || fail "musl reached the network stub"
[ ! -s "$work_dir/darwin-old-macos.curl" ] || fail "old macOS reached the network stub"
[ ! -s "$work_dir/darwin-version-unavailable.curl" ] || fail "unknown macOS version reached the network stub"
[ ! -s "$work_dir/windows-native.curl" ] || fail "native Windows reached the network stub"

echo "Installer regression contract: PASS ($case_count local simulations)"
