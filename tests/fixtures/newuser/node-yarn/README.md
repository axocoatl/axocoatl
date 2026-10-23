# node-yarn

A Node project managed with Yarn 1: `npm test` runs the
[uvu](https://github.com/lukeed/uvu) test runner, a dev dependency that `yarn
install --frozen-lockfile` installs from `yarn.lock`. `packageManager` in
`package.json` names Yarn 1.22.22, which corepack downloads. Without the
install the check fails with `uvu: not found`.

- Test command: `npm test` (after `yarn install --frozen-lockfile`)
- Before the fix: 1 of 4 tests fails
  (`the last page is full when the total is a multiple of the page size`).

## The bug

Paging skips the last item when the total is a multiple of the page size.
`paginate(entries, size)` in `src/pages.js` sizes the last page as
`entries.length % size`, which is 0 when the total divides evenly, so the
last page comes back empty and its items are lost.

## The expected fix

Size the last page by what is left after its start:

```diff
-    const length = page < count - 1 ? size : entries.length % size;
+    const length = page < count - 1 ? size : entries.length - start;
```

No test changes. All 4 tests pass after the fix.
