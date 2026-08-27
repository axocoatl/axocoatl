# Axocoatl contributor instructions

These instructions apply to the whole repository. More specific `AGENTS.md` files
add rules for their directory.

Before editing a subtree, read its local instructions explicitly:

- `axocoatl-server/AGENTS.md` for server routes or the embedded app.
- `examples/AGENTS.md` for runnable examples.
- `sites/AGENTS.md` plus the closer site-specific file for public sites.

## Start with the product

Read `docs/PRODUCT.md` before changing product behavior or interface structure.
Read `docs/ARCHITECTURE.md` before changing runtime behavior. `BRAND.md` governs
voice and visual identity, not implementation facts.

Axocoatl is one local-first coding workbench backed by a durable multi-agent
runtime. The browser app at `/` is the product surface. A folder-anchored session
and its chat are the permanent spine; files, editor, terminal, browser, activity,
attempt state, comparison, git, and agent graph open around that session. Agents,
Skills, MCP servers, and Automations belong in Settings rather than becoming peer
destinations.

Installation and onboarding configure Axocoatl for the current OS user. Onboarding
must not create a project, repository, Workspace, or Session folder. A repository
becomes a Workspace only through **Open workspace…** in the app. Never select a cwd
`axocoatl.yaml` implicitly; project-local configuration requires an explicit path.

The signature loop is:

1. Open or resume a workspace session.
2. Ask for one solution or explore several heterogeneous attempts.
3. Watch each attempt, including blocked or failed states.
4. Run repository checks and compare both outcome and route.
5. Keep one attempt, review its git changes, and commit deliberately.

Do not create another app shell, alternate dashboard, or feature-specific route.
When consolidating an older surface into `/`, inventory every working capability
first and prove that each one remains reachable. A handler, endpoint, imported
component, or hidden DOM node does not make a feature shipped. Shipped means a user
can discover and complete the behavior end to end.

## Product language

Use `workspace`, `session`, `attempt`, and `ways` in user-facing copy. Prefer
`Explore several ways`, `Keep this one`, `Outcome`, `Route`, `Checks`, and `Judge`.
Keep `variant`, `lane`, `fan-out`, `fan-in`, `worktree`, `branch`, `adopt`, and
`discard` in code, API, or an explicit git reveal. The product may explain its git
mechanics, but plumbing is not the primary interface.

## Repository map

- `axocoatl-cli/`: the `axocoatl` binary and command entrypoints.
- `axocoatl-server/`: Axum routes, WebSocket protocol, and embedded browser app.
- `crates/axocoatl-daemon/`: composition root and runtime orchestration.
- `crates/axocoatl-actor/`: actor turn execution and conversation state.
- `crates/axocoatl-session/`: persistent folder-session model.
- `crates/axocoatl-isolation/`: local Podman and optional remote sandbox backends.
- `crates/axocoatl-coordination/`: lattice, HTN, and auction primitives.
- `crates/axocoatl-memory/`: durable memory tiers and recall.
- `packages/lattice/`: native web-component library for graph rendering.
- `sites/docs/` and `sites/marketing/`: public claims and onboarding.

The root package is a placeholder used by benchmarks. It does not build the CLI or
refresh the embedded UI. Use an explicit package command.

## Build and verification

Baseline Rust gate:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo test --doc --workspace
cargo build -p axocoatl-cli
```

Some sandbox tests require Podman and are ignored by default. Webhook tests bind a
local port. If an execution sandbox blocks that bind, rerun the same test outside
the sandbox rather than treating it as a code failure.

For a browser-app change, rebuild `axocoatl-cli`, restart the daemon, and verify the
actual embedded page at `http://localhost:8080`. Exercise the affected journey,
check the browser console, and inspect light, dark, narrow, and reduced-motion
states when relevant. Automated DOM checks do not replace a screenshot or visual
inspection.

Add tests at the product seam, not only inside a helper crate. A route plus a unit
test can still leave the visible workflow broken.

## Claims are part of the change

Code is the final source of truth. Before changing a public capability claim,
verify the full repository, including `crates/*`, CLI, server routes, UI callers,
examples, release workflow, and current branch ancestry. `Defined` is not `wired`;
`wired` is not `reachable`.

Update affected public surfaces in the same change: `README.md`, `CHANGELOG.md`,
`docs/`, `sites/docs/`, `sites/marketing/`, examples, and package metadata. Do not
publish test counts, binary sizes, platform support, privacy guarantees, or exact
UI-parity claims without rechecking them. Historical plans and internal ledgers are
caches, not evidence.

Keep an active plan's status table and the internal claims material current in
the same change as the implementation they describe. That update discipline does
not make those documents authoritative: verify the code before reporting status,
especially after several commits or parallel work streams.

Do not call every unbuilt idea a gap. It is a gap only when the product has
actually decided it belongs in Axocoatl. Otherwise evaluate it as a possible
non-goal before turning it into roadmap pressure or a public caveat.

## Working agreement

- Preserve user changes and inspect a dirty worktree before editing.
- Optimize for correctness, durability, and honest verification rather than a
  calendar estimate or the apparent size of the change. Do not rank engineering
  choices by how fast or inexpensive they seem when the design has a correctness
  answer.
- Use one branch per approved plan. Never merge branches unless explicitly asked.
- Do not commit, push, open a pull request, publish, or release unless explicitly
  asked.
- Keep commits and pull-request text technical. Do not add AI attribution.
- Product decisions belong to the maintainer. When the code exposes a real fork,
  make a recommendation and record the decision instead of silently choosing a new
  product direction.
- Finish the requested scope and verify it. Do not report a phase complete while
  known required behavior is inaccessible, stubbed, or only documented.
- Prefer precise, direct writing. Explain constraints and tradeoffs without hype.
