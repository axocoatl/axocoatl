#!/usr/bin/env bash
# Exercise actions/verify-receipt's shell steps without GitHub: a fake `gh`
# serves a local repository's base and head commits, its pull request files
# (paginated) and its head; a fake `curl` serves release archives; crafted
# event files stand in for pull_request events. Receipts are signed with
# throwaway Ed25519 keys made here with OpenSSL 3 and verified by the real
# `axocoatl` binary under test. Network-free.
#
# Usage: test-verify-receipt-action.sh [AXOCOATL_BINARY]
# (default: target/debug/axocoatl). OPENSSL may name an OpenSSL 3 binary.
set -euo pipefail

[[ $# -le 1 ]] || {
  echo 'Usage: test-verify-receipt-action.sh [AXOCOATL_BINARY]' >&2
  exit 2
}

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH='' cd -- "$script_dir/.." && pwd)
action_dir="$repo_root/actions/verify-receipt"
binary=${1:-$repo_root/target/debug/axocoatl}
[[ -x $binary ]] || {
  echo "test-verify-receipt-action: no axocoatl binary at $binary (build it, or pass its path)" >&2
  exit 1
}
binary=$(CDPATH='' cd -- "$(dirname -- "$binary")" && pwd)/$(basename -- "$binary")
for tool in git jq tar ruby; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "test-verify-receipt-action: $tool is required" >&2
    exit 1
  }
done

openssl=""
for candidate in "${OPENSSL:-}" openssl /opt/homebrew/opt/openssl@3/bin/openssl \
  /opt/homebrew/bin/openssl /usr/local/opt/openssl@3/bin/openssl; do
  [[ -n $candidate ]] || continue
  if "$candidate" genpkey -algorithm ed25519 >/dev/null 2>&1; then
    openssl=$candidate
    break
  fi
done
[[ -n $openssl ]] || {
  echo 'test-verify-receipt-action: OpenSSL 3 with Ed25519 is required (set OPENSSL)' >&2
  exit 1
}

work=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-verify-receipt-test.XXXXXX")
trap 'rm -rf -- "$work"' EXIT HUP INT TERM
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=Test GIT_AUTHOR_EMAIL=test@example.invalid
export GIT_COMMITTER_NAME=Test GIT_COMMITTER_EMAIL=test@example.invalid

failures=0
passes=0

sha256_hex() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum | awk '{print $1}'
  else
    shasum -a 256 | awk '{print $1}'
  fi
}

b64() {
  base64 | tr -d '\n'
}

# ---------------------------------------------------------------------------
# Keys and receipts

# make_key NAME: a throwaway Ed25519 key; NAME.line is its trusted-key line.
make_key() {
  local dir="$work/keys" name=$1
  mkdir -p "$dir"
  "$openssl" genpkey -algorithm ed25519 -out "$dir/$name.pem" 2>/dev/null
  "$openssl" pkey -in "$dir/$name.pem" -pubout -outform DER 2>/dev/null | tail -c 32 >"$dir/$name.pub"
  b64 <"$dir/$name.pub" >"$dir/$name.b64"
  printf 'sha256:%s' "$(sha256_hex <"$dir/$name.pub")" >"$dir/$name.id"
  printf 'ed25519 %s test-%s %s\n' "$(cat "$dir/$name.b64")" "$name" \
    "$(sha256_hex <"$dir/$name.pub" | cut -c1-16)" >"$dir/$name.line"
}

# statement KEY SUBJECTS REMOVED VERDICT EXIT_CODE: an in-toto statement as
# a run would have it, the signer being KEY.
statement() {
  jq -c -n --argjson subject "$2" --argjson removed "$3" --arg verdict "$4" \
    --argjson exit_code "$5" --arg public_key "$(cat "$work/keys/$1.b64")" \
    --arg key_id "$(cat "$work/keys/$1.id")" '{
    _type: "https://in-toto.io/Statement/v1",
    subject: $subject,
    predicateType: "https://axocoatl.ai/receipt/v1",
    predicate: {
      run: {run_id: "run-00000000-0000-4000-8000-000000000001",
            session_id: "ses-00000000-0000-4000-8000-000000000002",
            started_at_ms: 1790000000000, finished_at_ms: 1790000060000,
            axocoatl_version: "1.4.0", task_sha256: ("0" * 64)},
      repository: {base_commit: null, dirty_paths: []},
      loadout: {id: "fix", version: 1, kind: "fix", digest: ("1" * 64), builtin: true},
      agents: [{id: "writer", role: "writer", runtime: "native", provider: "ollama",
                model: "test-writer"}],
      sandbox: {backend: "podman", network: "egress", workload: "hardened", policies: []},
      network: {events: 0, allowed_connections: 0, refused_connections: 0,
                route_requests: 0, routes: []},
      checks: [{name: "tests", state: "passed", exit_code: 0, timeout_ms: 180000,
                argv: ["sh", "-c", "sh test.sh"]}],
      review: {provider: "ollama", model: "test-reviewer", runtime: "native",
               verdict: "approved", passed: true, rounds: 1, max_rounds: 3, findings: 0},
      adjudications: [], verdict: $verdict, exit_code: $exit_code, attention: [],
      not_covered: {count: 0, areas: []}, warnings: [],
      usage: {input_tokens: 1, output_tokens: 1, cost_microunits: 0, cost_usd: 0,
              complete: true, cost_known: true, cost_computed: false},
      files: {source: "judged_capture", links: ["readme-link"], removed: $removed,
              complete: true, unattributed: []},
      signer: {algorithm: "ed25519", public_key: $public_key, key_id: $key_id}
    }}'
}

# sign KEY STATEMENT_FILE: a DSSE envelope of the statement, signed over
# DSSE's pre-authentication encoding.
sign() {
  local type=application/vnd.in-toto+json pae="$work/pae" sig="$work/sig"
  printf 'DSSEv1 %d %s %d ' "${#type}" "$type" "$(wc -c <"$2" | tr -d ' ')" >"$pae"
  cat "$2" >>"$pae"
  "$openssl" pkeyutl -sign -inkey "$work/keys/$1.pem" -rawin -in "$pae" -out "$sig" 2>/dev/null
  jq -c -n --arg payload "$(b64 <"$2")" --arg keyid "$(cat "$work/keys/$1.id")" \
    --arg sig "$(b64 <"$sig")" \
    '{payloadType: "application/vnd.in-toto+json", payload: $payload,
      signatures: [{keyid: $keyid, sig: $sig}]}'
}

# body ENVELOPE_FILE: a pull request description carrying the receipt block,
# with the CRLF line endings GitHub's editor saves.
body() {
  printf '## Axocoatl run\r\n\r\nVerdict: pass\r\n\r\n<!-- axocoatl-receipt:v1\r\n'
  b64 <"$1" | fold -w 76 | sed 's/$/\r/'
  printf -- '-->\r\n'
}

# subjects DIR PATH...: the subject list of PATHs as they are under DIR.
subjects() {
  local dir=$1 path digest
  shift
  for path in "$@"; do
    if [[ -L $dir/$path ]]; then
      digest=$(printf '%s' "$(readlink "$dir/$path")" | sha256_hex)
    else
      digest=$(sha256_hex <"$dir/$path")
    fi
    jq -c -n --arg name "$path" --arg digest "$digest" '{name: $name, digest: {sha256: $digest}}'
  done | jq -c -s 'sort_by(.name)'
}

# ---------------------------------------------------------------------------
# The repository: a base commit and a pull request's head

make_key trusted
make_key stranger

upstream="$work/upstream"
git init -q -b main "$upstream"
mkdir -p "$upstream/src" "$upstream/.axocoatl"
for i in $(seq -w 1 130); do
  printf 'file %s\nline two\n' "$i" >"$upstream/src/f$i.txt"
done
printf 'old name\nkept the same\nacross the rename\n' >"$upstream/old-name.txt"
printf 'gone\n' >"$upstream/gone.txt"
printf '# Widgets\n' >"$upstream/README.md"
{
  printf '# Keys whose receipts this repository trusts\n'
  cat "$work/keys/trusted.line"
} >"$upstream/.axocoatl/receipt-keys"
git -C "$upstream" add -A
git -C "$upstream" commit -q -m base
base=$(git -C "$upstream" rev-parse HEAD)

git -C "$upstream" checkout -q -b pr
for i in $(seq -w 1 120); do
  printf 'changed %s\n' "$i" >>"$upstream/src/f$i.txt"
done
git -C "$upstream" rm -q gone.txt
git -C "$upstream" mv old-name.txt new-name.txt
printf 'one more line\n' >>"$upstream/new-name.txt"
ln -s README.md "$upstream/readme-link"
git -C "$upstream" add -A
git -C "$upstream" commit -q -m 'the run'
head=$(git -C "$upstream" rev-parse HEAD)

changed_paths=(new-name.txt readme-link)
for i in $(seq -w 1 120); do
  changed_paths+=("src/f$i.txt")
done
removed_paths='["gone.txt","old-name.txt"]'

checkout_at() { # DIR COMMIT
  git clone -q "$upstream" "$1"
  git -C "$1" checkout -q --detach "$2"
}
checkout_at "$work/head" "$head"
all_subjects=$(subjects "$work/head" "${changed_paths[@]}")

# A head with one more file the receipt does not cover; its name holds a
# workflow command's escape so the annotation must escape it.
git -C "$upstream" checkout -q -b pr-extra "$head"
printf 'not from the run\n' >"$upstream/extra%0A::warning::name.txt"
git -C "$upstream" add -A
git -C "$upstream" commit -q -m 'a human edit'
head_extra=$(git -C "$upstream" rev-parse HEAD)
checkout_at "$work/head-extra" "$head_extra"

# A head that adds the stranger's key to the keys file, on a base that lists
# only the trusted key, and on a base without the file.
git -C "$upstream" checkout -q -b pr-own-key "$head"
cat "$work/keys/stranger.line" >>"$upstream/.axocoatl/receipt-keys"
git -C "$upstream" add -A
git -C "$upstream" commit -q -m 'trust my key'
head_own_key=$(git -C "$upstream" rev-parse HEAD)
checkout_at "$work/head-own-key" "$head_own_key"
own_key_subjects=$(subjects "$work/head-own-key" "${changed_paths[@]}" .axocoatl/receipt-keys)

git -C "$upstream" checkout -q -b base-no-keys "$base"
git -C "$upstream" rm -q .axocoatl/receipt-keys
git -C "$upstream" commit -q -m 'no keys yet'
base_no_keys=$(git -C "$upstream" rev-parse HEAD)
git -C "$upstream" checkout -q -b pr-new-keys "$base_no_keys"
git -C "$upstream" checkout -q "$head" -- src new-name.txt readme-link
git -C "$upstream" rm -q gone.txt old-name.txt
mkdir -p "$upstream/.axocoatl"
cat "$work/keys/stranger.line" >"$upstream/.axocoatl/receipt-keys"
git -C "$upstream" add -A
git -C "$upstream" commit -q -m 'add my key'
head_new_keys=$(git -C "$upstream" rev-parse HEAD)
checkout_at "$work/head-new-keys" "$head_new_keys"
new_keys_subjects=$(subjects "$work/head-new-keys" "${changed_paths[@]}" .axocoatl/receipt-keys)
git -C "$upstream" checkout -q "$base"

# ---------------------------------------------------------------------------
# Fake gh and curl

fakebin="$work/fakebin"
mkdir -p "$fakebin"
cat >"$fakebin/gh" <<'GH'
#!/usr/bin/env bash
# A fake `gh api` for the endpoints the action calls, served from a local
# repository: contents at a ref, a pull request's files (as GitHub lists
# them, a page of per_page at a time) and the pull request's head.
set -euo pipefail
printf '%s\n' "$*" >>"$FAKE_GH_LOG"
[[ ${1:-} == api ]] || { echo "fake gh: only 'gh api' is served" >&2; exit 2; }
shift
paginate=false jq_expr="" header="" endpoint=""
while (($#)); do
  case $1 in
    --paginate) paginate=true ;;
    --jq) jq_expr=$2; shift ;;
    -H) header=$2; shift ;;
    -*) echo "fake gh: unexpected flag $1" >&2; exit 2 ;;
    *) [[ -z $endpoint ]] || { echo 'fake gh: two endpoints' >&2; exit 2; }; endpoint=$1 ;;
  esac
  shift
done
[[ -n ${GH_TOKEN:-} ]] || { echo 'fake gh: GH_TOKEN is not set' >&2; exit 4; }
emit() {
  if [[ -n $jq_expr ]]; then jq -r "$jq_expr"; else cat; fi
}
prefix="repos/$FAKE_GH_REPO_NAME"
files_json() {
  local status old new
  git -C "$FAKE_GH_UPSTREAM" diff --name-status -z -M "$FAKE_GH_BASE" "$FAKE_GH_HEAD" \
    | while IFS= read -r -d '' status; do
        case $status in
          R*)
            IFS= read -r -d '' old; IFS= read -r -d '' new
            jq -c -n --arg f "$new" --arg p "$old" '{status: "renamed", filename: $f, previous_filename: $p}' ;;
          *)
            IFS= read -r -d '' new
            case $status in
              A) status=added ;; D) status=removed ;; M) status=modified ;; T) status=changed ;;
              C*) status=copied ;; *) status=unknown ;;
            esac
            jq -c -n --arg s "$status" --arg f "$new" '{status: $s, filename: $f}' ;;
        esac
      done | jq -c -s '.'
}
case $endpoint in
  "$prefix/contents/"*)
    rest=${endpoint#"$prefix/contents/"}
    path=${rest%%\?ref=*}
    ref=${rest#*\?ref=}
    [[ $header == 'Accept: application/vnd.github.raw+json' ]] \
      || { echo 'fake gh: contents asked without the raw media type' >&2; exit 2; }
    if git -C "$FAKE_GH_UPSTREAM" cat-file -e "$ref:$path" 2>/dev/null; then
      git -C "$FAKE_GH_UPSTREAM" show "$ref:$path"
    else
      echo 'gh: Not Found (HTTP 404)' >&2
      exit 1
    fi
    ;;
  "$prefix/pulls/$FAKE_GH_NUMBER/files?per_page="*)
    per_page=${endpoint##*per_page=}
    all=$(files_json)
    count=$(jq 'length' <<<"$all")
    pages=$(( (count + per_page - 1) / per_page ))
    ((pages > 0)) || pages=1
    $paginate || pages=1
    for ((page = 0; page < pages; page++)); do
      jq -c --argjson from $((page * per_page)) --argjson n "$per_page" '.[$from:$from + $n]' <<<"$all" | emit
    done
    ;;
  "$prefix/pulls/$FAKE_GH_NUMBER")
    count=$(files_json | jq 'length')
    jq -c -n --arg sha "${FAKE_GH_NOW_HEAD:-$FAKE_GH_HEAD}" --argjson n "$count" \
      '{number: 7, head: {sha: $sha}, changed_files: $n}' | emit
    ;;
  *)
    echo "gh: Not Found (HTTP 404) for $endpoint" >&2
    exit 1
    ;;
esac
GH
cat >"$fakebin/curl" <<'CURL'
#!/usr/bin/env bash
# A fake curl that serves release assets from FAKE_RELEASES.
set -euo pipefail
output="" url=""
while (($#)); do
  case $1 in
    --output) output=$2; shift ;;
    --proto | --retry | --connect-timeout | --max-time) shift ;;
    -*) ;;
    *) url=$1 ;;
  esac
  shift
done
printf '%s\n' "$url" >>"$FAKE_CURL_LOG"
prefix=https://github.com/axocoatl/axocoatl/releases/download/
[[ $url == "$prefix"* && -f $FAKE_RELEASES/${url#"$prefix"} ]] || {
  echo "curl: (22) The requested URL returned error: 404" >&2
  exit 22
}
cp "$FAKE_RELEASES/${url#"$prefix"}" "$output"
CURL
chmod +x "$fakebin/gh" "$fakebin/curl"

# ---------------------------------------------------------------------------
# Running the verify step

# event BODY_FILE BASE HEAD: a pull_request event.
event() {
  jq -n --rawfile body "$1" --arg base "$2" --arg head "$3" '{
    action: "synchronize", number: 7,
    pull_request: {number: 7, body: $body,
      base: {sha: $base, ref: "main", repo: {full_name: "octo/widgets"}},
      head: {sha: $head, ref: "axocoatl/fix-0001", repo: {full_name: "octo/widgets"}}}}'
}

# receipt NAME KEY SUBJECTS [VERDICT EXIT_CODE]: NAME.json (envelope) and
# NAME.md (a description carrying it).
receipt() {
  local name=$1
  statement "$2" "$3" "$removed_paths" "${4:-pass}" "${5:-0}" >"$work/$name.statement"
  sign "$2" "$work/$name.statement" >"$work/$name.json"
  body "$work/$name.json" >"$work/$name.md"
}

# verify NAME CHECKOUT BODY_FILE BASE HEAD [VAR=VALUE...]: run verify.sh as
# the action does; its exit code, annotations, outputs and summary are left
# in $work/run/NAME.*.
verify() {
  local name=$1 checkout=$2 body_file=$3 base_sha=$4 head_sha=$5
  shift 5
  local run="$work/run/$name"
  mkdir -p "$run.tmp"
  : >"$run.output"
  : >"$run.summary"
  : >"$run.gh"
  if [[ -n ${event_override:-} ]]; then
    cp "$event_override" "$run.event"
  else
    event "$body_file" "$base_sha" "$head_sha" >"$run.event"
  fi
  local status=0
  (
    cd "$checkout"
    env PATH="$fakebin:$PATH" GITHUB_EVENT_PATH="$run.event" GITHUB_OUTPUT="$run.output" \
      GITHUB_STEP_SUMMARY="$run.summary" RUNNER_TEMP="$run.tmp" AXOCOATL_BIN="$binary" \
      GH_TOKEN=fake-token KEYS_FILE=.axocoatl/receipt-keys REQUIRE_RECEIPT=true \
      REQUIRE_PASS=true ALLOW_UNCOVERED_FILES=false FAKE_GH_LOG="$run.gh" \
      FAKE_GH_UPSTREAM="$upstream" FAKE_GH_REPO_NAME=octo/widgets FAKE_GH_NUMBER=7 \
      FAKE_GH_BASE="$base_sha" FAKE_GH_HEAD="$head_sha" "$@" \
      "$action_dir/verify.sh"
  ) >"$run.log" 2>&1 || status=$?
  printf '%s\n' "$status" >"$run.status"
}

ok() {
  passes=$((passes + 1))
  echo "ok: $1"
}

bad() {
  failures=$((failures + 1))
  echo "FAIL: $1: $2" >&2
  [[ -z ${3:-} ]] || sed 's/^/  | /' "$3" >&2
}

# expect NAME passes|fails RESULT [TEXT...]: the step's exit status, its
# `result` output, and each TEXT in its log or summary.
expect() {
  local name=$1 want=$2 result=$3 run="$work/run/$1" text
  shift 3
  local status
  status=$(cat "$run.status")
  if [[ $want == passes && $status != 0 ]] || [[ $want == fails && $status == 0 ]]; then
    bad "$name" "expected the step to $want; it exited $status" "$run.log"
    return
  fi
  if ! grep -qx "result=$result" "$run.output"; then
    bad "$name" "expected result=$result; outputs were: $(tr '\n' ' ' <"$run.output")" "$run.log"
    return
  fi
  for text in "$@"; do
    if ! grep -qF -- "$text" "$run.log" "$run.summary"; then
      bad "$name" "expected to see: $text" "$run.log"
      return
    fi
  done
  ok "$name"
}

# never_read_at NAME SHA: the fake gh was never asked for contents at SHA.
never_read_at() {
  if grep -q "contents/.*ref=$2" "$work/run/$1.gh"; then
    bad "$1" "read the keys file at $2, which is not the base commit" "$work/run/$1.gh"
  elif ! grep -q "contents/.axocoatl/receipt-keys?ref=" "$work/run/$1.gh"; then
    bad "$1" "never read the keys file" "$work/run/$1.gh"
  else
    ok "$1: keys read only at the base commit"
  fi
}

trusted_id=$(cat "$work/keys/trusted.id")
stranger_id=$(cat "$work/keys/stranger.id")

receipt valid trusted "$all_subjects"
verify valid "$work/head" "$work/valid.md" "$base" "$head"
expect valid passes valid "run-00000000-0000-4000-8000-000000000001" "$trusted_id" \
  "123 changed file(s), 0 not covered" "tests passed" "ollama:test-reviewer" "test-writer"
if grep -qx "verdict=pass" "$work/run/valid.output" && grep -qx "key-id=$trusted_id" "$work/run/valid.output" \
  && grep -qx "run-id=run-00000000-0000-4000-8000-000000000001" "$work/run/valid.output"; then
  ok "valid: outputs"
else
  bad "valid: outputs" "missing run-id, verdict or key-id" "$work/run/valid.output"
fi
never_read_at valid "$head"
if [[ $(grep -c 'pulls/7/files?per_page=100' "$work/run/valid.gh") == 1 ]] \
  && grep -q -- '--paginate' "$work/run/valid.gh"; then
  ok "valid: files listed with pagination"
else
  bad "valid: files listed with pagination" "no paginated listing" "$work/run/valid.gh"
fi

printf 'A description without a receipt.\n' >"$work/none.md"
verify none-required "$work/head" "$work/none.md" "$base" "$head"
expect none-required fails missing "carries no Axocoatl receipt"
verify none-optional "$work/head" "$work/none.md" "$base" "$head" REQUIRE_RECEIPT=false
expect none-optional passes missing "none is required" "::notice"

receipt untrusted stranger "$all_subjects"
verify untrusted "$work/head" "$work/untrusted.md" "$base" "$head"
expect untrusted fails untrusted "signed by key $stranger_id" "axocoatl receipt key"

receipt own-key stranger "$own_key_subjects"
verify own-key "$work/head-own-key" "$work/own-key.md" "$base" "$head_own_key"
expect own-key fails untrusted "signed by key $stranger_id"
never_read_at own-key "$head_own_key"

receipt new-keys stranger "$new_keys_subjects"
verify new-keys "$work/head-new-keys" "$work/new-keys.md" "$base_no_keys" "$head_new_keys"
expect new-keys fails untrusted "signed by key $stranger_id" "has no .axocoatl/receipt-keys, so no key is trusted"
never_read_at new-keys "$head_new_keys"

# The receipt's digest of src/f001.txt is of its content with one byte
# changed.
first=$(sha256_hex <"$work/head/src/f001.txt")
flipped=$(sed '1s/^f/F/' "$work/head/src/f001.txt" | sha256_hex)
receipt mismatch trusted "$(jq -c --arg old "$first" --arg new "$flipped" \
  'map(if .digest.sha256 == $old then .digest.sha256 = $new else . end)' <<<"$all_subjects")"
verify mismatch "$work/head" "$work/mismatch.md" "$base" "$head"
expect mismatch fails mismatch "1 covered path(s) differ" "src/f001.txt"

verify uncovered "$work/head-extra" "$work/valid.md" "$base" "$head_extra"
expect uncovered fails uncovered "1 changed file(s) of this pull request are not covered" \
  "extra%250A::warning::name.txt: added by the pull request" "allow-uncovered-files: true"
if grep -q '^::warning::name' "$work/run/uncovered.log"; then
  bad "uncovered: escaping" "a path started a workflow command" "$work/run/uncovered.log"
else
  ok "uncovered: a path cannot start a workflow command"
fi
verify uncovered-allowed "$work/head-extra" "$work/valid.md" "$base" "$head_extra" ALLOW_UNCOVERED_FILES=true
expect uncovered-allowed passes valid "::warning title=Axocoatl receipt::1 changed file(s)" "1 not covered"

verify not-at-head "$work/head" "$work/valid.md" "$base" "$head_extra"
expect not-at-head fails error "not the pull request's head $head_extra" "github.event.pull_request.head.sha"

printf 'local edit\n' >>"$work/head/README.md"
verify local-change "$work/head" "$work/valid.md" "$base" "$head"
git -C "$work/head" checkout -q -- README.md
expect local-change fails error "local changes to tracked files" "README.md"

verify moved "$work/head" "$work/valid.md" "$base" "$head" FAKE_GH_NOW_HEAD="$head_extra"
expect moved fails error "head moved to $head_extra"

receipt attention trusted "$all_subjects" needs_attention 2
verify not-passed "$work/head" "$work/attention.md" "$base" "$head"
expect not-passed fails not-passed "verdict is 'needs_attention'"
verify not-passed-allowed "$work/head" "$work/attention.md" "$base" "$head" REQUIRE_PASS=false
expect not-passed-allowed passes valid "needs\\_attention"

# A payload changed after signing.
jq -c --arg payload "$(jq -c '.predicate.verdict = "pass" | .predicate.exit_code = 0' "$work/attention.statement" | b64)" \
  '.payload = $payload' "$work/attention.json" >"$work/tampered.json"
body "$work/tampered.json" >"$work/tampered.md"
verify tampered "$work/head" "$work/tampered.md" "$base" "$head"
expect tampered fails invalid "does not verify (invalid)"

verify bad-keys-path "$work/head" "$work/valid.md" "$base" "$head" KEYS_FILE=../outside
expect bad-keys-path fails error "keys-file must be a path in the repository"

verify bad-boolean "$work/head" "$work/valid.md" "$base" "$head" REQUIRE_PASS=yes
expect bad-boolean fails error "require-pass must be 'true' or 'false'"

jq -n '{ref: "refs/heads/main"}' >"$work/push.event"
event_override="$work/push.event" verify push-event "$work/head" "$work/valid.md" "$base" "$head"
expect push-event fails error "this event has no pull request"

# ---------------------------------------------------------------------------
# The action as a runner runs it: action.yml's steps in order, each step's
# env from its expressions (inputs with their defaults, the install step's
# outputs), each script under `bash --noprofile --norc -eo pipefail`, and
# the action's outputs from its steps'.

cat >"$work/run-composite.rb" <<'RUBY'
require "json"
require "tmpdir"
require "yaml"

action_dir, given_json, outputs_file = ARGV
action = YAML.load_file(File.join(action_dir, "action.yml"))
given = JSON.parse(given_json)
unknown = given.keys - action.fetch("inputs").keys
abort("undeclared inputs: #{unknown.join(", ")}") unless unknown.empty?
steps = {}
inputs = {}
evaluate = lambda do |expression|
  case expression.strip
  when /\Ainputs\.([A-Za-z0-9_-]+)\z/ then inputs.fetch(Regexp.last_match(1))
  when /\Asteps\.([A-Za-z0-9_-]+)\.outputs\.([A-Za-z0-9_-]+)\z/
    steps.fetch(Regexp.last_match(1), {}).fetch(Regexp.last_match(2), "")
  when "github.token" then "fake-token"
  else abort("unsupported expression: #{expression}")
  end
end
expand = ->(text) { text.to_s.gsub(/\$\{\{(.*?)\}\}/m) { evaluate.call(Regexp.last_match(1)) } }
action.fetch("inputs").each do |name, input|
  inputs[name] = given.key?(name) ? given[name] : expand.call(input["default"])
end
status = true
Dir.mktmpdir("composite") do |dir|
  action.fetch("runs").fetch("steps").each_with_index do |step, index|
    abort("step #{index + 1} is not a bash run step") unless step["shell"] == "bash" && step["run"]
    output = File.join(dir, "output-#{index}")
    script = File.join(dir, "step-#{index}.sh")
    File.write(output, "")
    File.write(script, step["run"])
    env = (step["env"] || {}).transform_values { |value| expand.call(value) }
    env["GITHUB_ACTION_PATH"] = action_dir
    env["GITHUB_OUTPUT"] = output
    status = system(env, "bash", "--noprofile", "--norc", "-eo", "pipefail", script)
    values = File.readlines(output, chomp: true).select { |line| line.include?("=") }
                 .map { |line| line.split("=", 2) }.to_h
    steps[step["id"]] = values if step["id"]
    break unless status
  end
end
File.write(outputs_file, action.fetch("outputs").map { |name, output| "#{name}=#{expand.call(output["value"])}\n" }.join)
exit(status ? 0 : 1)
RUBY

# composite NAME CHECKOUT BODY_FILE BASE HEAD INPUTS_JSON
composite() {
  local name=$1 checkout=$2 body_file=$3 base_sha=$4 head_sha=$5 inputs=$6
  local run="$work/run/$name"
  mkdir -p "$run.tmp"
  : >"$run.output"
  : >"$run.summary"
  : >"$run.gh"
  event "$body_file" "$base_sha" "$head_sha" >"$run.event"
  local status=0
  (
    cd "$checkout"
    env PATH="$fakebin:$PATH" GITHUB_EVENT_PATH="$run.event" GITHUB_STEP_SUMMARY="$run.summary" \
      RUNNER_TEMP="$run.tmp" RUNNER_OS=Linux RUNNER_ARCH=X64 FAKE_GH_LOG="$run.gh" \
      FAKE_GH_UPSTREAM="$upstream" FAKE_GH_REPO_NAME=octo/widgets FAKE_GH_NUMBER=7 \
      FAKE_GH_BASE="$base_sha" FAKE_GH_HEAD="$head_sha" \
      ruby "$work/run-composite.rb" "$action_dir" "$inputs" "$run.output"
  ) >"$run.log" 2>&1 || status=$?
  printf '%s\n' "$status" >"$run.status"
}

composite composite-valid "$work/head" "$work/valid.md" "$base" "$head" \
  "$(jq -c -n --arg binary "$binary" '{binary: $binary}')"
expect composite-valid passes valid "Using $binary" "123 changed file(s), 0 not covered"
if grep -qx "run-id=run-00000000-0000-4000-8000-000000000001" "$work/run/composite-valid.output" \
  && grep -qx "key-id=$trusted_id" "$work/run/composite-valid.output" \
  && grep -qx "verdict=pass" "$work/run/composite-valid.output"; then
  ok "composite-valid: the action's outputs"
else
  bad "composite-valid: the action's outputs" "missing run-id, key-id or verdict" "$work/run/composite-valid.output"
fi
composite composite-own-key "$work/head-own-key" "$work/own-key.md" "$base" "$head_own_key" \
  "$(jq -c -n --arg binary "$binary" '{binary: $binary, "keys-file": ".axocoatl/receipt-keys"}')"
expect composite-own-key fails untrusted "signed by key $stranger_id"
composite composite-uncovered-allowed "$work/head-extra" "$work/valid.md" "$base" "$head_extra" \
  "$(jq -c -n --arg binary "$binary" '{binary: $binary, "allow-uncovered-files": "true"}')"
expect composite-uncovered-allowed passes valid "1 not covered"

# ---------------------------------------------------------------------------
# The install step

releases="$work/releases"
make_release() { # TAG TARGET REPORTED_VERSION
  local dir="$releases/$1" stage="$work/stage-$1-$2" archive="axocoatl-$1-$2.tar.gz"
  mkdir -p "$dir" "$stage"
  # shellcheck disable=SC2016 # the fake binary's own $1
  printf '#!/bin/sh\n[ "$1" = --version ] && echo "axocoatl %s"\n' "$3" >"$stage/axocoatl"
  chmod +x "$stage/axocoatl"
  printf 'license\n' >"$stage/LICENSE"
  printf 'notices\n' >"$stage/THIRD_PARTY_LICENSES.txt"
  tar -czf "$dir/$archive" -C "$stage" axocoatl LICENSE THIRD_PARTY_LICENSES.txt
  printf '%s  %s\n' "$(sha256_hex <"$dir/$archive")" "$archive" >"$dir/$archive.sha256"
}
make_release v1.4.0 x86_64-unknown-linux-gnu 1.4.0
make_release v1.4.0 aarch64-unknown-linux-gnu 1.4.0
make_release v1.4.1 x86_64-unknown-linux-gnu 1.3.1
make_release v1.4.2 x86_64-unknown-linux-gnu 1.4.2
printf '%s  %s\n' "$(printf 'other' | sha256_hex)" axocoatl-v1.4.2-x86_64-unknown-linux-gnu.tar.gz \
  >"$releases/v1.4.2/axocoatl-v1.4.2-x86_64-unknown-linux-gnu.tar.gz.sha256"
make_release v1.4.3 x86_64-unknown-linux-gnu 1.4.3
printf 'extra line\n' >>"$releases/v1.4.3/axocoatl-v1.4.3-x86_64-unknown-linux-gnu.tar.gz.sha256"
x64_sha=$(awk '{print $1}' "$releases/v1.4.0/axocoatl-v1.4.0-x86_64-unknown-linux-gnu.tar.gz.sha256")

# run_install NAME [VAR=VALUE...]
run_install() {
  local name=$1 run="$work/install/$1"
  shift
  mkdir -p "$run.tmp"
  : >"$run.output"
  : >"$run.curl"
  local status=0
  env PATH="$fakebin:$PATH" GITHUB_OUTPUT="$run.output" RUNNER_TEMP="$run.tmp" \
    RUNNER_OS=Linux RUNNER_ARCH=X64 AXOCOATL_VERSION=v1.4.0 AXOCOATL_SHA256= AXOCOATL_BINARY= \
    FAKE_RELEASES="$releases" FAKE_CURL_LOG="$run.curl" "$@" \
    "$action_dir/install.sh" >"$run.log" 2>&1 || status=$?
  printf '%s\n' "$status" >"$run.status"
}

# installed NAME passes|fails [TEXT...]
installed() {
  local name=$1 want=$2 run="$work/install/$1" text status
  shift 2
  status=$(cat "$run.status")
  if [[ $want == passes && $status != 0 ]] || [[ $want == fails && $status == 0 ]]; then
    bad "install $name" "expected the step to $want; it exited $status" "$run.log"
    return
  fi
  for text in "$@"; do
    if ! grep -qF -- "$text" "$run.log" "$run.output" "$run.curl"; then
      bad "install $name" "expected to see: $text" "$run.log"
      return
    fi
  done
  if [[ $want == passes ]]; then
    local bin
    bin=$(sed -n 's/^binary=//p' "$run.output")
    [[ -n $bin && -x $bin ]] || { bad "install $name" "no executable binary output" "$run.output"; return; }
  elif grep -q '^binary=' "$run.output"; then
    bad "install $name" "a failed install still set the binary output" "$run.output"
    return
  fi
  ok "install $name"
}

run_install x64
installed x64 passes \
  "https://github.com/axocoatl/axocoatl/releases/download/v1.4.0/axocoatl-v1.4.0-x86_64-unknown-linux-gnu.tar.gz" \
  "https://github.com/axocoatl/axocoatl/releases/download/v1.4.0/axocoatl-v1.4.0-x86_64-unknown-linux-gnu.tar.gz.sha256" \
  "Installed axocoatl 1.4.0"
run_install arm64 RUNNER_ARCH=ARM64
installed arm64 passes "axocoatl-v1.4.0-aarch64-unknown-linux-gnu.tar.gz"
run_install pinned AXOCOATL_SHA256="$x64_sha"
installed pinned passes "as the sha256 input pins"
run_install pinned-other AXOCOATL_SHA256="$(printf 'x' | sha256_hex)"
installed pinned-other fails "but the sha256 input pins"
run_install bad-checksum AXOCOATL_VERSION=v1.4.2
installed bad-checksum fails "but the release's checksum file says"
run_install bad-manifest AXOCOATL_VERSION=v1.4.3
installed bad-manifest fails "must be exactly one line"
run_install wrong-version AXOCOATL_VERSION=v1.4.1
installed wrong-version fails "says 'axocoatl 1.3.1', not 'axocoatl 1.4.1'"
run_install missing-release AXOCOATL_VERSION=v9.9.9
installed missing-release fails "could not download"
run_install not-a-tag AXOCOATL_VERSION=latest
installed not-a-tag fails "is not a release tag"
run_install macos RUNNER_OS=macOS RUNNER_ARCH=ARM64
installed macos fails "runs on Linux x86_64 or ARM64 runners"
run_install given-binary AXOCOATL_BINARY="$binary"
installed given-binary passes "not a downloaded release"
if [[ -s $work/install/given-binary.curl ]]; then
  bad "install given-binary" "downloaded although a binary was given" "$work/install/given-binary.curl"
else
  ok "install given-binary: nothing downloaded"
fi

echo
if ((failures > 0)); then
  echo "verify-receipt action: $failures failure(s), $passes passed" >&2
  exit 1
fi
echo "verify-receipt action: PASS ($passes checks)"
