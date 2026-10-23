# First-run sample

A small Node repository with one failing test: paging drops the last item when
the item count is not a multiple of the page size. `npm test` (or
`node --test`) fails until `src/paging.js` is fixed.

`axocoatl onboard` copies this repository to a temporary directory and runs a
first fix on it:

```sh
axocoatl run fix --task "Fix the bug: paging skips the last item"
```
