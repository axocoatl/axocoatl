#!/bin/sh
# A fake `podman` for the tests of `axocoatl connect codex`'s login
# container, with no container at all. It appends what it was run with to
# the file `calls` beside it (the subcommand, its first argument, and
# `run`'s --timeout; never a token) and does what the file `mode` names:
#   ok        `exec --interactive --tty NAME codex … login [--device-auth]`
#             prints Codex 0.160.1's sign-in screen (the browser sign-in's
#             address after look-alikes, and then another address of the same
#             form; or the device page in color), waits for Enter and exits
#             0; the read (`exec NAME sh -c …`) prints the file `auth`
#   timeout   the login waits for Enter, then after 2 seconds (longer than
#             the lifetime the test gives the container) says what Podman
#             says when the container is stopped under `podman exec` and
#             exits 125; the read and `container exists` find no container
#   readfail  the read exits 125 while the container is still running
# `probe` exits at once (the tests run it until the file is not busy).
set -u
[ "${1:-}" = probe ] && exit 0
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
mode=$(cat "$here/mode")
printf '%s %s\n' "${1:-}" "${2:-}" >> "$here/calls"
case "${1:-}" in
  run)
    for argument in "$@"; do
      case "$argument" in
        --timeout=*) printf '%s\n' "$argument" >> "$here/calls" ;;
      esac
    done
    exit 0
    ;;
  rm)
    exit 0
    ;;
  container)
    case "${2:-}" in
      exists)
        [ "$mode" = timeout ] && exit 1
        exit 0
        ;;
      inspect)
        printf 'true\n'
        exit 0
        ;;
    esac
    exit 125
    ;;
  exec)
    if [ "${2:-}" = --interactive ]; then
      device=no
      for argument in "$@"; do
        [ "$argument" = --device-auth ] && device=yes
      done
      if [ "$device" = yes ]; then
        printf '\nWelcome to Codex [v\033[90m0.160.1\033[0m]\n\033[90mOpenAI'"'"'s command-line coding agent\033[0m\n'
        printf '\nFollow these steps to sign in with ChatGPT using device code authorization:\n'
        printf '\n0. Not this one: https://auth.openai.com/codex/devices or http://auth.openai.com/codex/device\n'
        printf '\n1. Open this link in your browser and sign in to your account\n   \033[94mhttps://auth.openai.com/codex/device\033[0m\n'
        printf '\n2. Enter this one-time code \033[90m(expires in 15 minutes)\033[0m\n   \033[94mWXYZ-1234\033[0m\n\n'
      else
        printf 'Starting local login server on http://localhost:1455.\n'
        printf 'If your browser did not open, navigate to this URL to authenticate:\n\n'
        printf 'http://auth.openai.com/oauth/authorize?state=plain-http\n'
        printf 'https://auth.openai.com.evil.test/oauth/authorize?state=look-alike\n'
        printf '\033]8;;https://auth.openai.com/oauth/authorize?state=hidden-target\033\\https://evil.test/\033]8;;\033\\\n'
        printf 'https://auth.openai.com/oauth/authorize?response_type=code&client_id=app_fake&redirect_uri=http%%3A%%2F%%2Flocalhost%%3A1455%%2Fauth%%2Fcallback&code_challenge=AbCdEfGhIjKlMnOp-qr_stUVwxyz0123456789ABCDE&code_challenge_method=S256&state=Qsk-pLkJhGfDsAeyJ0x&scope=openid+profile+email+offline_access&originator=codex_cli_rs\n\n'
        printf 'On a remote or headless machine? Use `codex login --device-auth` instead.\n'
        printf 'https://auth.openai.com/oauth/authorize?response_type=code&state=printed-again\n'
      fi
      printf 'Press Enter once you have signed in\n'
      IFS= read -r _ || exit 70
      if [ "$mode" = timeout ]; then
        sleep 2
        printf 'Error: no exec session with ID 0123abcd found\n' >&2
        exit 125
      fi
      printf 'Successfully logged in\n'
      exit 0
    fi
    case "$mode" in
      ok)
        cat "$here/auth"
        exit 0
        ;;
      timeout)
        printf 'Error: no container with name or ID "%s" found: no such container\n' "${2:-}" >&2
        exit 125
        ;;
    esac
    exit 125
    ;;
esac
printf 'fake podman: unexpected arguments: %s\n' "$*" >&2
exit 64
