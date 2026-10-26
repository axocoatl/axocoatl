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

if GITHUB_ACTIONS=false bash "$script_dir/ci-registry-mirror.sh" >"$work/out" 2>&1; then
  echo 'registry writer ran outside CI' >&2; exit 1
fi
grep -Fq 'only on a CI runner' "$work/out"
[[ ! -e $work/calls ]]
if GITHUB_ACTIONS=true bash "$script_dir/ci-registry-mirror.sh" unexpected >"$work/out" 2>&1; then
  echo 'registry writer accepted arguments' >&2; exit 1
fi
[[ ! -e $work/calls ]]
GITHUB_ACTIONS=true bash "$script_dir/ci-registry-mirror.sh" >"$work/out"

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
for image in ("docker.io/library/alpine:3.20", "docker.io/library/rust@sha256:" + "a" * 64):
    primary, mirrors = route(image)
    assert primary == image, (image, primary)
    assert mirrors == [image.replace("docker.io/", "mirror.gcr.io/", 1)], (image, mirrors)
for image in (
    "docker.io/searxng/searxng:2026.10.2-19ffbcd30@sha256:c642712fcedcdaa78fac44f71eada86aff510745826ba1bd1a368211fea2ce7f",
    "docker.io/menci/archlinuxarm:latest",
    "docker.io/library-lookalike/image:latest",
    "ghcr.io/example/image:latest",
):
    assert route(image) == (image, []), (image, route(image))
PYTHON
[[ $(wc -l <"$work/calls" | tr -d ' ') == 2 ]]
echo 'CI registry mirror contract: PASS (CI guard, write boundary, official-image cache, direct pinned third-party images)'
