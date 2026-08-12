## Summary

One or two sentences on what this changes and why.

## Related issue

Closes #...

## Type of change

- [ ] Bug fix (no behavior change for existing happy paths)
- [ ] New feature
- [ ] Refactor (no functional change)
- [ ] Docs / examples / chore
- [ ] Breaking change (call out below)

## How this was tested

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `cargo test --doc --workspace`
- [ ] `cargo build -p axocoatl-cli`
- [ ] If the browser app changed: rebuilt the CLI, restarted the daemon,
  exercised the affected journey in the one app, and checked the browser
  console plus relevant light, dark, narrow, and reduced-motion states.
- [ ] If a new agent/skill/tool was added: ran an end-to-end demo against
  it locally.

## Breaking changes

If this changes a public API, CLI surface, or persisted file format,
describe the impact and the migration path here. Otherwise: "None."

## Checklist

- [ ] No secrets, API keys, or owner-personal paths in the diff.
- [ ] New code has tests where it makes sense.
- [ ] `CHANGELOG.md` updated under `[Unreleased]` for any user-visible change.
- [ ] Docs updated if behavior or setup changed.
- [ ] Any consolidated capability remains discoverable and usable end to end.
