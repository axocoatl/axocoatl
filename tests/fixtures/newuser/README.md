# New-user fixtures

Tiny repository templates that the clean-machine new-user gate
([docs/RELEASE_GATE.md](../../../docs/RELEASE_GATE.md)) runs `axocoatl run fix`
on, exactly as the published copy tells a new user to. Each has one failing
test and the language's standard test command:

| Fixture | Test command | Needs before the check |
|---|---|---|
| `node-plain` | `npm test` (`node --test`) | nothing; also onboarding's first-run sample |
| `node-deps` | `npm test` (uvu) | `npm ci` (a dev dependency) |
| `python-pytest` | `pytest` | `pip install -e '.[test]'` |
| `rust` | `cargo test` | a Rust toolchain |
| `go` | `go test ./...` | a Go toolchain |

The gate's four fixtures (`node-deps`, `python-pytest`, `rust`, `go`) share
one bug: paging skips the last item when the total is a multiple of the page
size. That is the task the README's `axocoatl run fix` example names, so the
README command runs on each of them verbatim. Each fixture's README states
the bug, the expected fix and how many tests pass after it.

`node-plain` is onboarding's first-run sample, made by the onboarding work.
Its bug is another one (the last item is dropped when the count is not a
multiple of the page size), so the README's task would describe the wrong
bug on it. The gate runs the command from `node-plain/README.md` instead
(`"command": "fixture-readme"` in `fixtures.json`), the one onboarding runs
on it.

## Rules

- A fixture is a template: the gate copies it to a new directory, runs
  `git init` and commits it, and only then runs Axocoatl in the copy. Never
  run `cargo`, `npm` or `pip` inside this tree.
- Keep each fixture to one failing test that the expected fix makes pass,
  with no test changes.
- `fixtures.json` says what the gate expects of each run (the image and the
  setup the run must choose). `scripts/release-gate/stub-fixes.json` says how
  the gate's scripted model server makes the fix; every fixture here needs
  one entry there. `node --test scripts/release-gate/stub-model-server.test.mjs`
  checks both against these files.
- `scripts/release-gate/test-fixtures.sh` proves on the host that each
  fixture fails before the stub's fix and passes after it.
