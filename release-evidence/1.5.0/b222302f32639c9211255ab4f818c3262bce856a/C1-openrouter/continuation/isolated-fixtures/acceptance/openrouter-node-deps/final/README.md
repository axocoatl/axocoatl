# node-deps

A Node project whose tests need its dev dependencies: `npm test` runs the
[uvu](https://github.com/lukeed/uvu) test runner, which `npm ci` installs from
`package-lock.json`. Without that setup step the check fails with
`uvu: not found`, the way a real project with mocha or jest does.

- Test command: `npm test` (after `npm ci`)
- Before the fix: 1 of 4 tests fails
  (`the last page is full when the total is a multiple of the page size`).

## The bug

Paging skips the last item when the total is a multiple of the page size

# Problem

The pagination function had a bug that caused the last page to be empty when the total number of items was exactly divisible by the page size.

## Example

When calling `paginate([1,2,3,4,5,6,7,8,9,10], 5)`:
- **Before fix**: `[[1,2,3,4,5], []]` (last page empty)
- **After fix**: `[[1,2,3,4,5], [6,7,8,9,10]]` (last page properly filled)

## Solution

Fixed the logic in `src/paginate.js` to correctly calculate the length of the last page by using `items.length - start` instead of `items.length % size`.

This ensures that when the total number of items is a multiple of the page size, the last page contains the correct number of items rather than being empty..
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
