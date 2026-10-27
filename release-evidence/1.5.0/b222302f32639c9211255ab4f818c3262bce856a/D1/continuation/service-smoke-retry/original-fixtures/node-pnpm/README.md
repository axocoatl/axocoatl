# node-pnpm

A Node project managed with pnpm: `npm test` runs the
[uvu](https://github.com/lukeed/uvu) test runner, a dev dependency that `pnpm
install --frozen-lockfile` installs from `pnpm-lock.yaml`. `packageManager`
in `package.json` names pnpm 10.34.7, which corepack downloads. Without the
install the check fails with `uvu: not found`.

- Test command: `npm test` (after `pnpm install --frozen-lockfile`)
- Before the fix: 1 of 4 tests fails
  (`the last page is full when the total is a multiple of the page size`).

## The bug

Paging skips the last item when the total is a multiple of the page size.
`paginate(list, size)` in `src/pager.js` sizes the last page as `list.length
% size`, which is 0 when the total divides evenly, so the last page comes
back empty and its items are lost.

## The expected fix

Size the last page by what is left after its start:

```diff
-    const length = page < count - 1 ? size : list.length % size;
+    const length = page < count - 1 ? size : list.length - start;
```

No test changes. All 4 tests pass after the fix.
