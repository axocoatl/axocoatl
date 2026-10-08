#!/bin/sh
# A fake `claude` for the tests of `axocoatl connect claude-code`. It acts
# like `claude setup-token`: prompts, waits for a keypress (Enter), then
# prints the token in the file `token` beside it, the way the file `mode`
# names:
#   colored  ANSI colors around the token and a color change inside it
#   split    the token in three write() calls, two inside its prefix, with
#            pauses longer than the relay's idle flush
#   wrapped  lines exactly as wide as the terminal when it is narrower than
#            200 columns (how a narrow terminal wraps it), else one line
#   twice    the same token twice
#   two      two different tokens
#   none     no token; exits 1
# It never reads its environment for the token, and writes nowhere else.
set -u
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if [ "${1:-}" != "setup-token" ] || [ "$#" -ne 1 ]; then
  printf 'fake claude: unexpected arguments: %s\n' "$*" >&2
  exit 64
fi
mode=$(cat "$here/mode")
token=$(cat "$here/token")
printf '\033[2J\033[H\033[1mWelcome to Claude Code\033[0m\n'
printf 'Browser did not open? Use the url below to sign in:\n\n'
printf '  https://claude.ai/oauth/authorize?code=true&client_id=fake-client\n\n'
printf 'Press Enter to continue\n'
IFS= read -r _ || exit 70
cols=$(stty size 2>/dev/null | cut -d' ' -f2)
if [ "$mode" = none ]; then
  printf 'Error: the sign-in was cancelled\n'
  exit 1
fi
printf '\033[32m\342\234\223\033[0m Long-lived authentication token created successfully!\n\n'
printf 'Your OAuth token (valid for 1 year):\n\n'
case "$mode" in
  colored)
    head=$(printf '%s' "$token" | cut -c1-40)
    tail=$(printf '%s' "$token" | cut -c41-)
    printf '\033[1m\033[38;5;214m%s\033[0m\033[38;2;10;200;30m%s\033[0m\n' "$head" "$tail"
    ;;
  split)
    first=$(printf '%s' "$token" | cut -c1-4)
    second=$(printf '%s' "$token" | cut -c5-30)
    third=$(printf '%s' "$token" | cut -c31-)
    printf '\033[33m%s' "$first"
    sleep 0.4
    printf '%s' "$second"
    sleep 0.3
    printf '\033[1m%s\033[0m\n' "$third"
    ;;
  wrapped)
    if [ "${cols:-0}" -gt 0 ] && [ "${cols:-0}" -lt 200 ]; then
      printf '\033[36m'
      printf '%s\n' "$token" | fold -w "$cols"
      printf '\033[0m'
    else
      printf '\033[36m%s\033[0m\n' "$token"
    fi
    ;;
  twice)
    printf '%s\n\nexport CLAUDE_CODE_OAUTH_TOKEN=%s\n' "$token" "$token"
    ;;
  two)
    other="sk-ant-oat01-$(printf '%s' "$token" | cut -c14- | tr 'a-f' 'g-l')"
    printf '%s\n%s\n' "$token" "$other"
    ;;
esac
printf '\nStore this token securely. You will not be able to see it again.\n'
exit 0
