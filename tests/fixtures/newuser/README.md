# New-user fixtures

Tiny repository templates that the clean-machine new-user gate
([docs/RELEASE_GATE.md](../../../docs/RELEASE_GATE.md)) runs `axocoatl run fix`
on, exactly as the README tells a new user to. Each one is the same bug in a
different language, with one failing test and the language's standard test
command:

| Fixture | Test command | Needs before the check |
|---|---|---|
| `node-plain` | `npm test` (`node --test`) | nothing; also onboarding's first-run sample |
| `node-deps` | `npm test` (uvu) | `npm ci` (a dev dependency) |
| `python-pytest` | `pytest` | `pip install -e '.[test]'` |
| `rust` | `cargo test` | a Rust toolchain |
| `go` | `go test ./...` | a Go toolchain |

The bug, in every fixture: paging skips the last item when the total is a
multiple of the page size. That is the task the README's `axocoatl run fix`
example names, so the README command runs on each fixture verbatim. Each
fixture's README states the bug, the expected fix and how many tests pass
after it.

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
