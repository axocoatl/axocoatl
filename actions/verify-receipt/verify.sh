#!/usr/bin/env bash
# Verify the Axocoatl receipt a pull request's description carries
# (docs/design/1.4-receipts.md, section 3):
#
# 1. take the `<!-- axocoatl-receipt:v1 … -->` block from the description in
#    GITHUB_EVENT_PATH (a pull_request or pull_request_target event);
# 2. read the trusted keys from the pull request's BASE commit with
#    `gh api repos/<repo>/contents/<keys-file>?ref=<base sha>`, never from its
#    head, so a pull request cannot trust its own key;
# 3. run `axocoatl receipt verify - --keys <those keys> --repo <checkout>` on
#    the checked-out head commit;
# 4. list the pull request's changed files (`gh api …/pulls/<n>/files`,
#    paginated) and require the receipt to cover each one;
# 5. write a job summary and the step outputs, and fail on any problem.
#
# Environment: AXOCOATL_BIN, GH_TOKEN, KEYS_FILE, REQUIRE_RECEIPT,
# REQUIRE_PASS, ALLOW_UNCOVERED_FILES, and GITHUB_EVENT_PATH, GITHUB_OUTPUT,
# GITHUB_STEP_SUMMARY and RUNNER_TEMP as GitHub Actions sets them. Needs
# bash, git, gh and jq. It runs nothing from the checkout: it only reads its
# files, without following symbolic links.
set -euo pipefail

BLOCK_START='<!-- axocoatl-receipt:v1'
TITLE='Axocoatl receipt'
# GitHub lists at most this many files of a pull request.
FILES_API_LIMIT=3000
# Paths listed one by one in annotations; the summary lists more.
ANNOTATED_PATHS=50
SUMMARY_PATHS=200

result=error

# A workflow command's message: % and line breaks escaped, so nothing a
# receipt or a path holds can start a command of its own.
escape() {
  local text=$1
  text=${text//'%'/'%25'}
  text=${text//$'\r'/'%0D'}
  text=${text//$'\n'/'%0A'}
  printf '%s' "$text"
}

annotate() {
  printf '::%s title=%s::%s\n' "$1" "$TITLE" "$(escape "$2")"
}

output() {
  # Values are ids and words; anything else is dropped rather than written.
  printf '%s=%s\n' "$1" "$(printf '%s' "$2" | tr -cd 'A-Za-z0-9:._-')" >>"$GITHUB_OUTPUT"
}

summary() {
  if [[ -n ${GITHUB_STEP_SUMMARY:-} ]]; then
    cat >>"$GITHUB_STEP_SUMMARY"
  else
    cat >/dev/null
  fi
}

# Markdown for one table cell or list item: no line breaks, and the
# characters that would start markup escaped.
md_filter='def md: tostring | gsub("[\r\n]+"; " ") | gsub("(?<c>[\\\\`*_\\[\\]<>|~#!&])"; "\\\(.c)");'

# Fail with result $1, one error annotation per further argument, and a job
# summary that lists them.
fail() {
  result=$1
  shift
  local message
  for message in "$@"; do
    annotate error "$message"
  done
  output result "$result"
  {
    printf '%s\n' "## $TITLE: not verified (\`$result\`)" ""
    printf '%s\n' "$@" | jq -R -r "$md_filter"' "- " + md'
  } | summary
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || fail error "this action needs '$1' on the runner"
}

boolean() {
  case $2 in
    true | false) ;;
    *) fail error "$1 must be 'true' or 'false', not '$2'" ;;
  esac
}

[[ -n ${GITHUB_OUTPUT:-} ]] || {
  echo "GITHUB_OUTPUT is not set; this script runs as a step of the verify-receipt action" >&2
  exit 1
}
need jq
need git
need gh
[[ -n ${AXOCOATL_BIN:-} && -x ${AXOCOATL_BIN:-} ]] \
  || fail error "the Axocoatl binary from the install step is missing"

keys_file=${KEYS_FILE:-.axocoatl/receipt-keys}
require_receipt=${REQUIRE_RECEIPT:-true}
require_pass=${REQUIRE_PASS:-true}
allow_uncovered=${ALLOW_UNCOVERED_FILES:-false}
boolean require-receipt "$require_receipt"
boolean require-pass "$require_pass"
boolean allow-uncovered-files "$allow_uncovered"

# The keys file is a repository path: no leading slash, no empty, `.` or
# `..` segment, and only characters a contents URL carries unencoded.
[[ $keys_file =~ ^[A-Za-z0-9._-]+(/[A-Za-z0-9._-]+)*$ ]] \
  && [[ ! /$keys_file/ =~ /\.\.?/ ]] \
  || fail error "keys-file must be a path in the repository such as .axocoatl/receipt-keys, not '$keys_file'"

event=${GITHUB_EVENT_PATH:-}
[[ -n $event && -f $event ]] || fail error "GITHUB_EVENT_PATH does not name the event file"
jq -e '.pull_request | type == "object"' "$event" >/dev/null 2>&1 \
  || fail error "this action runs on pull_request or pull_request_target events; this event has no pull request"

number=$(jq -r '.pull_request.number' "$event")
base_sha=$(jq -r '.pull_request.base.sha' "$event")
head_sha=$(jq -r '.pull_request.head.sha' "$event")
repo=$(jq -r '.pull_request.base.repo.full_name' "$event")
[[ $number =~ ^[0-9]+$ ]] || fail error "the event's pull request has no number"
[[ $base_sha =~ ^[0-9a-f]{40}([0-9a-f]{24})?$ ]] || fail error "the event's pull request has no base commit"
[[ $head_sha =~ ^[0-9a-f]{40}([0-9a-f]{24})?$ ]] || fail error "the event's pull request has no head commit"
[[ $repo =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || fail error "the event's pull request has no base repository"

work=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/axocoatl-receipt.XXXXXX")
jq -j '.pull_request.body // ""' "$event" >"$work/body"

if ! grep -qF -- "$BLOCK_START" "$work/body"; then
  missing="the pull request's description carries no Axocoatl receipt (no '$BLOCK_START' block). Keep as PR adds one; a receipt longer than 40,000 characters stays in the run's record bundle instead."
  if [[ $require_receipt == true ]]; then
    fail missing "$missing"
  fi
  annotate notice "$missing Passing, because require-receipt is false."
  output result missing
  printf '%s\n' "## $TITLE: none" "" \
    "This pull request carries no receipt, and none is required (\`require-receipt: false\`)." | summary
  exit 0
fi

# The checkout must be the head commit the event names: actions/checkout
# checks out a merge commit for pull_request events by default.
checkout_hint="check out the pull request's head before this step (actions/checkout with ref: \${{ github.event.pull_request.head.sha }})"
root=$(git rev-parse --show-toplevel 2>"$work/git.err") \
  || fail error "the working directory is not a Git checkout: $(head -c 300 "$work/git.err"); $checkout_hint"
checked_out=$(git -C "$root" rev-parse --verify 'HEAD^{commit}' 2>/dev/null) || checked_out=none
[[ $checked_out == "$head_sha" ]] \
  || fail error "the checkout is at $checked_out, not the pull request's head $head_sha; $checkout_hint"
git -C "$root" status --porcelain=v1 --untracked-files=no >"$work/status" 2>"$work/git.err" \
  || fail error "git status failed in the checkout: $(head -c 300 "$work/git.err")"
[[ ! -s $work/status ]] \
  || fail error "the checkout has local changes to tracked files, so its files are not the head commit's: $(head -n 5 "$work/status" | tr '\n' ' ')"

# The trusted keys, from the base commit only. A base without the file
# trusts no key, so every receipt is untrusted.
trust_hint="To trust a computer's receipts, add the line 'axocoatl receipt key' prints on it to $keys_file on the base branch; a pull request cannot add its own key."
if gh api -H 'Accept: application/vnd.github.raw+json' \
  "repos/$repo/contents/$keys_file?ref=$base_sha" >"$work/keys" 2>"$work/keys.err"; then
  keys_note=""
elif grep -q 'HTTP 404' "$work/keys.err"; then
  : >"$work/keys"
  keys_note="the base commit ${base_sha:0:12} has no $keys_file, so no key is trusted"
else
  fail error "could not read $keys_file at the base commit $base_sha: $(head -c 300 "$work/keys.err")"
fi

code=0
"$AXOCOATL_BIN" receipt verify - --keys "$work/keys" --repo "$root" --json \
  <"$work/body" >"$work/verify.json" 2>"$work/verify.err" || code=$?
jq -e 'type == "object" and (.result | type == "string")' "$work/verify.json" >/dev/null 2>&1 \
  || fail error "axocoatl receipt verify failed (exit code $code): $(head -c 300 "$work/verify.err")"

problems() {
  jq -r '.problems[]?' "$work/verify.json"
}
key_id=$(jq -r '.key_id // ""' "$work/verify.json")
case $code in
  0) ;;
  1)
    result=$(jq -r '.result' "$work/verify.json")
    [[ $result == malformed ]] || result=invalid
    lines=()
    while IFS= read -r line; do
      lines+=("$line")
    done < <(problems)
    fail "$result" "the receipt does not verify ($result)" ${lines[@]+"${lines[@]}"}
    ;;
  2)
    output key-id "$key_id"
    fail untrusted \
      "the receipt is signed by key $key_id, which $keys_file at the base commit ${base_sha:0:12} does not list${keys_note:+ ($keys_note)}" \
      "$trust_hint"
    ;;
  4)
    output key-id "$key_id"
    lines=()
    while IFS= read -r line; do
      lines+=("$line")
    done < <(jq -r --argjson n "$ANNOTATED_PATHS" \
      '.files.mismatches[:$n][]? | "\(.path): \(.problem)"' "$work/verify.json")
    count=$(jq -r '.files.mismatches | length' "$work/verify.json")
    fail mismatch \
      "the checked-out head does not match the receipt: $count covered path(s) differ from the files the run left" \
      ${lines[@]+"${lines[@]}"}
    ;;
  *)
    fail error "axocoatl receipt verify could not check the receipt (exit code $code): $(problems | head -c 600)"
    ;;
esac

# Valid and trusted, and the head's files match it.
"$AXOCOATL_BIN" receipt show - --json <"$work/body" >"$work/statement.json" 2>"$work/show.err" \
  || fail error "axocoatl receipt show failed: $(head -c 300 "$work/show.err")"
run_id=$(jq -r '.predicate.run.run_id' "$work/statement.json")
verdict=$(jq -r '.predicate.verdict' "$work/statement.json")
output key-id "$key_id"
output run-id "$run_id"
output verdict "$verdict"

# The pull request's changed files, and its head as GitHub sees it now, so
# a push during this check cannot pass a listing of another commit.
gh api --paginate "repos/$repo/pulls/$number/files?per_page=100" \
  --jq '.[] | [.status, .filename, (.previous_filename // "")] | @json' \
  >"$work/files.lines" 2>"$work/files.err" \
  || fail error "could not list the pull request's files: $(head -c 300 "$work/files.err")"
gh api "repos/$repo/pulls/$number" --jq '[.head.sha, .changed_files] | @json' \
  >"$work/pull.json" 2>"$work/pull.err" \
  || fail error "could not read the pull request: $(head -c 300 "$work/pull.err")"
now_head=$(jq -r '.[0]' "$work/pull.json")
changed_files=$(jq -r '.[1]' "$work/pull.json")
[[ $now_head == "$head_sha" ]] \
  || fail error "the pull request's head moved to $now_head while this check ran on $head_sha; the check for $now_head verifies it"
jq -s '.' "$work/files.lines" >"$work/files.json"
listed=$(jq 'length' "$work/files.json")

# Each changed file must be covered: a removed file among the receipt's
# removed paths, a renamed file's new name among its subjects and its old
# name among its removed paths, any other file among its subjects.
jq -n --slurpfile statement "$work/statement.json" --slurpfile files "$work/files.json" '
  ($statement[0]) as $s
  | (reduce $s.subject[].name as $name ({}; .[$name] = true)) as $subjects
  | (reduce ($s.predicate.files.removed // [])[] as $name ({}; .[$name] = true)) as $removed
  | [ $files[0][] as [$status, $name, $previous]
      | if $status == "removed" then
          if $removed[$name] then empty
          else {path: $name, why: "removed by the pull request; the receipt does not name it as removed"} end
        elif $status == "renamed" then
          (if $subjects[$name] then empty
           else {path: $name, why: "renamed from \($previous); the receipt does not cover it"} end),
          (if $removed[$previous] then empty
           else {path: $previous, why: "renamed to \($name); the receipt does not name it as removed"} end)
        else
          if $subjects[$name] then empty
          else {path: $name, why: "\($status) by the pull request; the receipt does not cover it"} end
        end ]
' >"$work/uncovered.json"
uncovered=$(jq 'length' "$work/uncovered.json")

coverage_problems=()
if [[ $listed != "$changed_files" ]]; then
  if [[ $changed_files =~ ^[0-9]+$ ]] && ((changed_files > FILES_API_LIMIT)); then
    coverage_problems+=("the pull request changes $changed_files files; GitHub lists at most $FILES_API_LIMIT, so the rest cannot be checked against the receipt")
  else
    fail error "GitHub listed $listed of the pull request's $changed_files changed files"
  fi
fi
if ((uncovered > 0)); then
  coverage_problems+=("$uncovered changed file(s) of this pull request are not covered by the receipt")
  while IFS= read -r line; do
    coverage_problems+=("$line")
  done < <(jq -r --argjson n "$ANNOTATED_PATHS" '.[:$n][] | "\(.path): \(.why)"' "$work/uncovered.json")
fi
complete=$(jq -r '.predicate.files.complete' "$work/statement.json")

# shellcheck disable=SC2016 # a jq program, not shell
summary_filter="$md_filter"'
  def code: tostring | gsub("[\r\n]+"; " ") | gsub("[`|]"; "\u0027") | "`" + . + "`";
  $statement[0] as $s | $s.predicate as $p | $report[0] as $r | $uncovered[0] as $u
  | "## '"$TITLE"': \($heading)\n",
    "| | |", "| --- | --- |",
    "| Run | \($p.run.run_id | code) (Axocoatl \($p.run.axocoatl_version | md)) |",
    "| Verdict | **\($p.verdict | md)** (exit code \($p.exit_code)) |",
    "| Signed by | \($r.key_id | code), trusted by \($keys | code) at the base commit \($base[0:12] | code) |",
    "| Checks | \(if ($p.checks | length) == 0 then "none required"
                  else [$p.checks[] | "\(.name | md) \(.state | md)"] | join(", ") end) |",
    "| Review | \(if $p.review == null then "none required"
                  else "\("\($p.review.provider):\($p.review.model)" | code): \($p.review.verdict | md)" end) |",
    "| Writers | \([$p.agents[] | select(.role == "writer")
                  | "\(.runtime | md)\(if .program_version then " " + (.program_version | md) else "" end) \("\(.provider):\(.model)" | code)"]
                  | if length == 0 then "none" else join(", ") end) |",
    "| Files | \($s.subject | length) covered, \($p.files.removed | length) removed, digests \($p.files.source | code)\(if $p.files.complete then "" else "; the receipt does not cover every path the run changed" end) |",
    "| Pull request | \($listed) changed file(s), \($u | length) not covered |",
    "| Statement | \($r.statement_sha256 | code) |",
    (if ($u | length) > 0 then
       "\n### Not covered by the receipt\n",
       ($u[:$shown][] | "- \(.path | code): \(.why | md)"),
       (if ($u | length) > $shown then "- and \(($u | length) - $shown) more" else empty end)
     else empty end),
    "\nA receipt proves which key signed these facts and that the covered files are the ones the run left; it does not prove the change is correct."
'
write_summary() {
  jq -r -n "$summary_filter" --arg heading "$1" --arg keys "$keys_file" --arg base "$base_sha" \
    --argjson listed "$listed" --argjson shown "$SUMMARY_PATHS" \
    --slurpfile statement "$work/statement.json" --slurpfile report "$work/verify.json" \
    --slurpfile uncovered "$work/uncovered.json" | summary
}

if ((${#coverage_problems[@]} > 0)); then
  if [[ $allow_uncovered == false ]]; then
    write_summary "not verified (\`uncovered\`)"
    result=uncovered
    for message in "${coverage_problems[@]}"; do
      annotate error "$message"
    done
    annotate error "a pull request changes only what its receipt covers, unless the workflow sets allow-uncovered-files: true"
    output result uncovered
    exit 1
  fi
  for message in "${coverage_problems[@]}"; do
    annotate warning "$message"
  done
fi
[[ $complete == true ]] \
  || annotate warning "the receipt does not cover every path the run changed: $(jq -r '.predicate.files.unattributed | join("; ")' "$work/statement.json")"

if [[ $require_pass == true && $verdict != pass ]]; then
  write_summary "verified, but the run did not pass"
  annotate error "the receipt is valid, but its run's verdict is '$verdict', not 'pass' (require-pass: true)"
  output result not-passed
  exit 1
fi

write_summary "verified"
output result valid
printf 'Verified the receipt of run %s (verdict %s), signed by %s, which %s at the base commit trusts; %s changed file(s), %s not covered.\n' \
  "$(escape "$run_id")" "$(escape "$verdict")" "$key_id" "$keys_file" "$listed" "$uncovered"
