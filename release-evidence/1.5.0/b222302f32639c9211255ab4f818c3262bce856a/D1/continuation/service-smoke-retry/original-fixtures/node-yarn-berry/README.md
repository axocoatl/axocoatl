# node-yarn-berry

A Node project managed with Yarn 4: `npm test` runs the
[uvu](https://github.com/lukeed/uvu) test runner, a dev dependency that `yarn
install --immutable` installs from `yarn.lock`. `packageManager` in
`package.json` names Yarn 4.18.1, which corepack downloads. `.yarnrc.yml`
keeps Yarn's `node_modules` layout (`nodeLinker: node-modules`) rather than
Plug'n'Play, as projects whose tools need `node_modules` do. Without the
install the check fails with `uvu: not found`.

- Test command: `npm test` (after `yarn install --immutable`)
- Before the fix: 1 of 4 tests fails
  (`the last page is full when the total is a multiple of the page size`).

## The bug

Paging skips the last item when the total is a multiple of the page size.
`paginate(values, size)` in `lib/paginate.js` sizes the last page as
`values.length % size`, which is 0 when the total divides evenly, so the last
page comes back empty and its items are lost.

## The expected fix

Size the last page by what is left after its start:

```diff
-    const length = page < count - 1 ? size : values.length % size;
+    const length = page < count - 1 ? size : values.length - start;
```

No test changes. All 4 tests pass after the fix.
