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

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] If the dashboard changed: rebuilt, restarted daemon, walked through
  every visible tab, watched the browser console.
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
