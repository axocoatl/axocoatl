# Browser app and server instructions

These rules extend the repository-root instructions for `axocoatl-server/`.

## One app

`/` is the only interactive product page. Do not restore `/app`, a page-level
`/variants`, Studio, or another feature destination. API routes may retain internal
names such as `variants`; browser navigation may not expose them as separate apps.

The session chat is always the spine. Place modules by how they are used:

- Persistent left rail: workspaces and their sessions, including live attempt state.
- Right dock: run state a user watches while work executes, such as attempts, cost,
  and plan.
- Bottom dock: terminal only.
- Over the chat: focused review surfaces such as files, editor, diff, comparison,
  and trajectories.
- Settings: Agents, Skills, MCP servers, and Automations.

Do not turn the old eight destinations into eight new modules with equal visual
weight. A module opens around a session; it is not a place the user navigates away
to.

## Frontend architecture

The app is intentionally buildless: native ES modules, custom elements, and shadow
DOM. UI components live in `static/ui/*.js`, shared component tokens in
`static/ui/tokens.css`, and sheet helpers in `static/ui/sheets.js`. They are embedded
and served from `/ui/*`. Do not add Node, a bundler, a virtual DOM framework, or a
second token system without an explicit architectural decision.

The shell in `static/index.html` owns layout, shared state, routing, and the bridge to
legacy inline code. A component owns its rendering and behavior. Move behavior out
of the shell incrementally, preserving it before simplifying it. Do not duplicate a
singleton such as Monaco or xterm while a component is detached or moved.

## Preserve the complete attempt loop

Before deleting or replacing an attempt-related surface, trace both its UI callers
and server endpoints. The complete product loop includes:

- heterogeneous agent/model selection and model preflight;
- plan and live roster;
- blocked-on-human supervision and resume;
- outcome, trajectory, diff, and counterfactual cost;
- repository checks, verdicts, and judging with the real task and selected provider;
- keep/adopt and discard/cleanup;
- explicit git detail on demand.

Importing `<ax-compare>` is not proof that this loop exists. Add or update a journey
test that starts from the visible session UI. Never hardcode a judge task, provider,
or model. Never delete a legacy surface until its capability inventory is either
reachable in the one app or explicitly removed by product decision.

## State and WebSocket invariants

- Open the WebSocket before unrelated awaited fetches. One slow fetch must not leave
  the app silently disconnected.
- Reconnect must restore authoritative session, attempt, approval, and run state, not
  only future frames.
- Key live state by durable run/session identity. Do not infer ownership from whichever
  session happens to be visible.
- Treat error, empty, loading, blocked, cancelled, and clean as distinct states.
- Never report a clean git tree when git itself failed.

## Known browser traps

- A CSS custom property with a missing sheet invalidates the whole declaration without
  a useful error. Keep `tokens.css` adopted and verify computed style.
- Setting an observed attribute before insertion can invoke both
  `attributeChangedCallback` and `connectedCallback`; make reloads idempotent.
- Git porcelain has two status columns. Preserve leading spaces in fixtures and parsing.
- `getElementById` cannot find a component while it is detached. Keep stable object
  references for moved editor/terminal elements.
- Monaco can measure zero immediately after a move. Relayout on a later animation frame.
- `requestAnimationFrame` may not run in a hidden automated tab. Capture a screenshot or
  foreground the page before treating zero layout as a product failure.
- Mutating a copied file snapshot does not update editor state. Route save/revert through
  the owning component.

## Verification

Run the root Rust gate, then rebuild the binary with:

```bash
cargo build -p axocoatl-cli
```

Restart the daemon and exercise the visible one-app journey. At minimum verify session
restore, one normal turn, several attempts, failed/blocked state, comparison, checks,
keep, git review, terminal placement, settings, reconnect, and console cleanliness for
the areas changed.
