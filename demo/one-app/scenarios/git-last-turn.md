# Git ownership and Last turn after Keep

This short follow-up starts at the end of the rehearsed Northstar Storefront
Ways path in [`PROMPTS.md`](../PROMPTS.md). It proves the repository boundary
after an operator keeps one checked result.

## Claim

**Keep this one** returns the selected Way's changes to the primary Session
checkout without committing them. Source Control → **Last turn** scopes review
to the paths and hunks recorded for that kept result; staging, discarding, and
committing remain explicit Git actions.

## Do not claim

- Keep is not a merge, commit, push, pull request, deploy, or release.
- Last turn is not a replacement for the complete working-tree view.
- Judge does not choose or apply the result. The operator chooses Keep.
- This scenario does not prove that unrelated pre-existing working-tree changes
  are absent. The fresh Northstar fixture and preflight establish that
  condition.

## Start or reset

Prepare a clean Northstar root with `./demo/one-app/prepare.sh`, start it with
`./demo/one-app/start.sh`, and run the diagnosis and **Explore several ways**
prompts from [`PROMPTS.md`](../PROMPTS.md). Run both Ways through the common
`npm run check`, inspect Outcome and Route, and Keep one passing, non-empty
result that changes only `lib/orders.js`.

Do not send a new Agent turn after Keep. The Last turn path scope follows the
most recently accepted canonical turn; even a read-only follow-up can replace
the kept turn's changed-path scope with an empty one. A Terminal command does
not create a Session turn, but Git review should still happen first.

## Browser actions

1. After the completed Keep transaction, open History and hold on the completed
   canonical Turn produced by the kept checked Way. Preserve the matching Keep
   receipt as durable evidence; the active attempt set is intentionally cleared
   after Keep and must not be fabricated for the film.
2. Without sending another message, open **Source Control**.
3. Select **Last turn**.
4. Open every path attributed to the kept result and inspect the meaningful
   hunks. For this Northstar take the only path must be `lib/orders.js`; do not
   accept a take that changed tests against the task contract.
5. Leave the change uncommitted. If the recording includes staging, stage only
   after the diff has been inspected and show the transition explicitly.
6. After the Last turn capture, open Terminal and run:

   ```bash
   npm run check
   ```

   Return to Source Control to show that the passing patch remains a normal Git
   change in the primary checkout.
7. Reload the page—or stop and restart the exact same daemon and data root—then
   reopen the Session and select **Last turn** again. The same kept path and hunk
   scope must rehydrate from durable Turn/Keep evidence.

## Visible proof

- History restores the completed canonical Turn created from the kept checked Way.
- The matching Keep receipt identifies the selected attempt even though the active
  attempt set is cleared after Keep.
- **Last turn** shows exact file paths and hunks, not only an attempt summary.
- The Last turn path scope survives reload or daemon restart against the same
  root.
- The patch is present in the primary checkout and remains uncommitted.
- Staging is available as a separate operator action; if used, its state change
  is visible.

## Durable evidence

Use the same environment values captured for the Several Ways run:

```bash
export AXO_DEMO_ROOT='/private/tmp/axocoatl-one-app-showcase-ways'
export AXO_SESSION_ID='ses-paste-the-id-here'
```

Before optional staging:

```bash
git -C "$AXO_DEMO_ROOT/workspace" status --short
git -C "$AXO_DEMO_ROOT/workspace" diff --stat
git -C "$AXO_DEMO_ROOT/workspace" diff
npm --prefix "$AXO_DEMO_ROOT/workspace" run check
find "$AXO_DEMO_ROOT/workspace/.axo-variants" \
  -path '*/receipts/keep-*.json' -type f -print
curl -sS "http://127.0.0.1:18080/api/sessions/$AXO_SESSION_ID/turns"
```

If the film includes staging, capture the boundary separately:

```bash
git -C "$AXO_DEMO_ROOT/workspace" diff --cached --stat
git -C "$AXO_DEMO_ROOT/workspace" diff --cached
```

The keep receipt and canonical Turn establish which result was selected. Git
status and diff establish that its patch returned to the primary checkout
without a commit.

## Recording beats

1. Start on the completed canonical kept Turn in History.
2. Open Source Control and show the one unstaged kept path.
3. Open Last turn and inspect the exact changed path and meaningful hunk.
4. Reload or restart the same root, then reopen Last turn on the same path and hunk.
5. End on All changes with the patch visibly uncommitted, or with one inspected hunk staged by
   an explicit operator action.

Target 20–30 seconds after editing. This is a Git-ownership coda to the Ways
film, not a second explanation of attempt execution.

## Cleanup

Capture the keep receipt, Turn list, Git status, and diff before resetting. Then
close the Session, stop its daemon, and prepare a fresh Northstar root with the
same `AXOCOATL_DEMO_ROOT` value. Do not commit solely to simplify cleanup.

## Known constraints

- Last turn scopes Source Control to paths recorded for the most recent
  canonical turn. Review it before another Agent turn changes that scope.
- Keep requires a passing, non-empty, terminal Way and is resumable once its
  apply transaction begins.
- The full Source Control view remains necessary when the checkout already had
  unrelated changes; this deterministic scenario begins clean so the Last turn
  proof is unambiguous.
