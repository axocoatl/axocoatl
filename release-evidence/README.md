# Release evidence

Signed results of the clean-machine new-user gate, one bundle per release
candidate: `release-evidence/<version>/<commit>/`, where `<commit>` is the
commit the gate tested. `release.yml` refuses to publish a tag unless a bundle
here passes every required path in [docs/RELEASE_GATE.md](../docs/RELEASE_GATE.md)
and is signed by a key in [.axocoatl/gate-keys](../.axocoatl/gate-keys).

Make a bundle with `scripts/release-gate/collect-evidence.sh`, which signs it
with the gate account's signing key (`~/.axocoatl-gate-signer`, made once with
`collect-evidence.sh --init-signer`); never edit one by hand (its manifest
lists every file's SHA-256, and the signature covers the manifest). Check it with
`node scripts/release-gate/check-evidence.mjs --version X.Y.Z --tag-commit "$(git rev-parse HEAD)"`.
