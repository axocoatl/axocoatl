#!/bin/sh
# A stand-in for `podman` on macOS for the new-user gate's Tier B
# (docs/RELEASE_GATE.md), where a hosted runner cannot run a Podman machine.
# It answers the read-only commands doctor and onboarding run the way Podman
# does in one state, read from $PODMAN_SHIM_STATE (a file), and appends every
# call's arguments to $PODMAN_SHIM_LOG:
#
#   no-machine  Podman 5.8.2, no machine defined
#   stopped     Podman 5.8.2, podman-machine-default stopped
#   running     Podman 5.8.2, the machine running rootless
#   rootful     Podman 5.8.2, the machine running rootful
#   podman6     Podman 6.1.4, machine list with a numeric Memory and the
#               libkrun provider (the podman.io package's default)
#
# (A missing Podman is no shim on PATH at all.) `machine start` turns a
# stopped machine on, as a run's admission does. Anything else, which would
# create or change something, fails and is logged.
state_file=${PODMAN_SHIM_STATE:?PODMAN_SHIM_STATE names the state file}
log=${PODMAN_SHIM_LOG:?PODMAN_SHIM_LOG names the call log}
state=$(cat "$state_file")
printf '%s\n' "$*" >>"$log"

version=5.8.2
provider=applehv
memory='"8589934592"'
machines=1
running=false
rootful=false
case "$state" in
  no-machine) machines=0 ;;
  stopped) ;;
  running) running=true ;;
  rootful) running=true rootful=true ;;
  podman6) running=true version=6.1.4 provider=libkrun memory=8589934592 ;;
  *)
    echo "podman stand-in: unknown state '$state'" >&2
    exit 125
    ;;
esac

machine_list() {
  if [ "$machines" = 0 ]; then
    echo '[]'
  else
    printf '[{"Name":"podman-machine-default*","Default":true,"Running":%s,"VMType":"%s","CPUs":6,"Memory":%s,"DiskSize":"107374182400"}]\n' \
      "$running" "$provider" "$memory"
  fi
}

runtime_info() {
  rootless=true
  [ "$rootful" = true ] && rootless=false
  printf '{"host":{"arch":"arm64","kernel":"6.12.9-200.fc42.aarch64","distribution":{"distribution":"fedora","variant":"coreos","version":"42"},"security":{"rootless":%s,"selinuxEnabled":true},"serviceIsRemote":true,"idMappings":{"uidmap":[{"container_id":0,"host_id":501,"size":1},{"container_id":1,"host_id":100000,"size":1000000}],"gidmap":[{"container_id":0,"host_id":1000,"size":1},{"container_id":1,"host_id":100000,"size":1000000}]}},"version":{"APIVersion":"%s","Version":"%s"}}\n' \
    "$rootless" "$version" "$version"
}

case "$1 ${2:-}" in
  "--version "* | "version "*) echo "podman version $version" ;;
  "info "*)
    if [ "$running" = true ]; then
      runtime_info
    else
      # Podman's own message, backquotes and all.
      # shellcheck disable=SC2016
      echo 'Cannot connect to Podman. Please verify your connection to the Linux system using `podman system connection list`, or try `podman machine init` and `podman machine start` to manage a new Linux VM' >&2
      exit 125
    fi
    ;;
  "machine list") machine_list ;;
  "machine inspect")
    [ "$machines" = 1 ] || { echo 'Error: podman-machine-default: VM does not exist' >&2; exit 125; }
    printf '[{"Name":"podman-machine-default","Rootful":%s,"Resources":{"CPUs":6,"Memory":8192,"DiskSize":100}}]\n' "$rootful"
    ;;
  "machine info") printf '{"Host":{"VMType":"%s"}}\n' "$provider" ;;
  "machine start")
    [ "$machines" = 1 ] || { echo 'Error: podman-machine-default: VM does not exist' >&2; exit 125; }
    [ "$state" = stopped ] && echo running >"$state_file"
    echo 'Machine "podman-machine-default" started successfully'
    ;;
  "system connection") echo '[]' ;;
  *)
    echo "podman stand-in ($state): \`podman $*\` is not available in Tier B" >&2
    exit 125
    ;;
esac
