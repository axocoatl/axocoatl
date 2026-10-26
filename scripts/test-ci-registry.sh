#!/usr/bin/env bash
# Capture the real CI writer without sudo or a machine-wide configuration edit.
set -euo pipefail
script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/axocoatl-registry-test.XXXXXX")
trap 'rm -rf -- "$work"' EXIT HUP INT TERM
mkdir "$work/bin"
cat >"$work/bin/sudo" <<'SUDO'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$AXO_REGISTRY_TEST_OUTPUT/calls"
case "$*" in
  'mkdir -p /etc/containers/registries.conf.d') ;;
  'tee /etc/containers/registries.conf.d/50-axocoatl-ci-docker-hub-mirror.conf')
    cat >"$AXO_REGISTRY_TEST_OUTPUT/registries.conf" ;;
  *) echo "unexpected privileged command: $*" >&2; exit 1 ;;
esac
SUDO
chmod +x "$work/bin/sudo"
export AXO_REGISTRY_TEST_OUTPUT="$work"
export PATH="$work/bin:$PATH"

if GITHUB_ACTIONS=false bash "$script_dir/ci-registry.sh" >"$work/out" 2>&1; then
  echo 'registry writer ran outside CI' >&2; exit 1
fi
grep -Fq 'only on a CI runner' "$work/out"
[[ ! -e $work/calls ]]
if GITHUB_ACTIONS=true bash "$script_dir/ci-registry.sh" unexpected >"$work/out" 2>&1; then
  echo 'registry writer accepted arguments' >&2; exit 1
fi
[[ ! -e $work/calls ]]
GITHUB_ACTIONS=true bash "$script_dir/ci-registry.sh" >"$work/out"

python3 - "$work/registries.conf" <<'PYTHON'
import pathlib, sys, tomllib
registries = tomllib.loads(pathlib.Path(sys.argv[1]).read_text())["registry"]
# containers/image chooses only the longest matching namespace, with a path,
# tag or digest separator. Verify the generated config's effective scope.
def route(image):
    matches = [entry for entry in registries if any(
        image.startswith(entry.get("prefix", entry["location"]) + separator)
        for separator in ("/", ":", "@")
    )]
    if not matches:
        return image, []
    entry = max(matches, key=lambda item: len(item.get("prefix", item["location"])))
    prefix = entry.get("prefix", entry["location"])
    suffix = image[len(prefix):]
    assert not entry.get("insecure", False), "CI must retain TLS verification"
    return entry["location"] + suffix, [mirror["location"] + suffix for mirror in entry.get("mirror", [])]
# Official and third-party images must both keep their origin: a cache can
# contain the pinned manifests while missing a layer, including the Go image
# that failed the actual x86 fresh-user journey.
for image in (
    "docker.io/library/alpine:3.20",
    "docker.io/library/rust@sha256:" + "a" * 64,
    "docker.io/library/golang:1.27.2-bookworm@sha256:5cf287a799e6b94384bad13d16b14904c531f51ba65792237e122ce42b392f61",
    "docker.io/searxng/searxng:2026.10.2-19ffbcd30@sha256:c642712fcedcdaa78fac44f71eada86aff510745826ba1bd1a368211fea2ce7f",
    "docker.io/menci/archlinuxarm:latest",
    "docker.io/library-lookalike/image:latest",
    "ghcr.io/example/image:latest",
):
    assert route(image) == (image, []), (image, route(image))
PYTHON
[[ $(wc -l <"$work/calls" | tr -d ' ') == 2 ]]
echo 'CI registry contract: PASS (CI guard, write boundary, pinned official and third-party origins, TLS)'
