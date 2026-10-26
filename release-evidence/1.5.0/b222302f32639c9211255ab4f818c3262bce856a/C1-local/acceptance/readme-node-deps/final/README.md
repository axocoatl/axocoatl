# node-deps

A Node project whose tests need its dev dependencies: `npm test` runs the
[uvu](https://github.com/lukeed/uvu) test runner, which `npm ci` installs from
`package-lock.json`. Without that setup step the check fails with
`uvu: not found`, the way a real project with mocha or jest does.

- Test command: `npm test` (after `npm ci`)
- Before the fix: 1 of 4 tests fails
  (`the last page is full when the total is a multiple of the page size`).

## The bug

Paging skips the last item when the total is a multiple of the page size.
`paginate(items, size)` in `src/paginate.js` sizes the last page as
`items.length % size`, which is 0 when the total divides evenly, so the last
page comes back empty and its items are lost.

## The expected fix

Size the last page by what is left after its start:

```diff
-    const length = page < count - 1 ? size : items.length % size;
+    const length = page < count - 1 ? size : items.length - start;
```

No test changes. All 4 tests pass after the fix.
