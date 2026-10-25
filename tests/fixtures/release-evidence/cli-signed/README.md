# A bundle signed by the real CLI

`bundle/` is a release-evidence bundle whose `manifest.dsse.json` was made by
`axocoatl receipt sign-evidence` with a throwaway receipt key from a fresh
data root; `gate-keys` holds that key's public line. Its Tier A path carries
`A-x86/ci-run.json`, the record of a green CI run that
`collect-evidence.sh --ci-run` writes. The evidence checker's
tests (`scripts/release-gate/evidence.test.mjs`) verify it, so the Rust signer
and the Node verifier cannot drift apart. The key's private half was never
kept. To regenerate: build `axocoatl`, then follow the steps in that test's
comment.
