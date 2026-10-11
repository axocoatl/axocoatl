#!/bin/sh
# A fake `codex` for the tests of `axocoatl connect codex`. It acts like
# `codex -c cli_auth_credentials_store="file" login [--device-auth]`: it
# checks its arguments, writes what it was run with to the file `seen`
# beside it (never a token), prints a login screen, waits for Enter (the
# approval in the browser, or the device code entered elsewhere), then does
# what the file `mode` names:
#   ok      writes the file `auth` beside it to $CODEX_HOME/auth.json (0600,
#           as Codex creates it), plus a log, a session file and a nested
#           directory, as Codex keeps state in CODEX_HOME; the screen shows
#           an OpenAI API key and a JWT from the file `screen`, which must
#           be hidden
#   apikey  writes an API-key auth.json (no ChatGPT sign-in)
#   none    writes no auth.json and exits 1
#   linked  makes $CODEX_HOME/auth.json a symbolic link to the file `auth`
# It never reads ~/.codex or its environment for a token, and writes nowhere
# but $CODEX_HOME and `seen`.
set -u
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
mode=$(cat "$here/mode")
device=no
if [ "$#" -eq 3 ] && [ "$1" = -c ] && [ "$2" = 'cli_auth_credentials_store="file"' ] \
  && [ "$3" = login ]; then
  :
elif [ "$#" -eq 4 ] && [ "$1" = -c ] && [ "$2" = 'cli_auth_credentials_store="file"' ] \
  && [ "$3" = login ] && [ "$4" = --device-auth ]; then
  device=yes
else
  printf 'fake codex: unexpected arguments: %s\n' "$*" >&2
  exit 64
fi
home_mode=$(stat -f '%Lp' "${CODEX_HOME:-/nonexistent}" 2>/dev/null \
  || stat -c '%a' "${CODEX_HOME:-/nonexistent}" 2>/dev/null)
{
  printf 'device=%s\n' "$device"
  printf 'codex_home=%s\n' "${CODEX_HOME:-}"
  printf 'pwd=%s\n' "$(pwd -P)"
  printf 'home_mode=%s\n' "$home_mode"
  for variable in CODEX_API_KEY OPENAI_API_KEY CODEX_ACCESS_TOKEN \
    CODEX_REFRESH_TOKEN_URL_OVERRIDE CODEX_SQLITE_HOME; do
    eval "value=\${$variable+set}"
    printf '%s=%s\n' "$variable" "${value:-unset}"
  done
} > "$here/seen"
if [ "$device" = yes ]; then
  printf '\nWelcome to Codex [fake]\n\nFollow these steps to sign in with ChatGPT using device code authorization:\n\n'
  printf '1. Open this link in your browser and sign in to your account\n   https://auth.openai.com/codex/device\n\n'
  printf '2. Enter this one-time code (expires in 15 minutes)\n   \033[1mWXYZ-1234\033[0m\n\n'
else
  printf 'Starting local login server on http://localhost:1455.\n'
  printf 'If your browser did not open, navigate to this URL to authenticate:\n\n'
  printf 'https://auth.openai.com/oauth/authorize?response_type=code&client_id=app_fake&redirect_uri=http%%3A%%2F%%2Flocalhost%%3A1455%%2Fauth%%2Fcallback&code_challenge=AbCdEfGhIjKlMnOp-qr_stUVwxyz0123456789ABCDE&code_challenge_method=S256&state=Qsk-pLkJhGfDsAeyJ0x&originator=codex_cli_rs\n\n'
fi
printf 'Press Enter once you have signed in\n'
IFS= read -r _ || exit 70
case "$mode" in
  none)
    printf 'Error logging in: the sign-in was cancelled\n'
    exit 1
    ;;
  apikey)
    umask 077
    printf '{"auth_mode":"apikey","OPENAI_API_KEY":"sk-proj-fakekeyfakekeyfakekey"}\n' \
      > "$CODEX_HOME/auth.json"
    ;;
  linked)
    ln -s "$here/auth" "$CODEX_HOME/auth.json"
    ;;
  ok)
    umask 077
    cp "$here/auth" "$CODEX_HOME/auth.json"
    mkdir -p "$CODEX_HOME/log" "$CODEX_HOME/sessions/2026/10/11"
    printf 'login started\nlogin finished\n' > "$CODEX_HOME/log/codex-login.log"
    cp "$here/auth" "$CODEX_HOME/sessions/2026/10/11/state.json"
    printf '\033[32mSuccessfully logged in\033[0m\n'
    printf 'debug: key %s\n' "$(sed -n 1p "$here/screen")"
    printf 'debug: token %s\n' "$(sed -n 2p "$here/screen")"
    ;;
esac
exit 0
