# go

A Go module tested with `go test ./...`. Its tests use a module from
proxy.golang.org (`github.com/google/go-cmp`, pinned by `go.sum`), so the
run's setup downloads it (`go mod download`) before the check, which then
builds offline. It needs a Go toolchain in the run's image; a run in an image
without `go` cannot check it.

- Test command: `go test ./...` (after `go mod download`)
- Before the fix: 1 of 4 tests fails
  (`TestTheLastPageIsFullWhenTheTotalIsAMultipleOfThePageSize`).

## The bug

Paging skips the last item when the total is a multiple of the page size.
`Paginate(items, size)` in `pages.go` sizes the last page as `len(items) %
size`, which is 0 when the total divides evenly, so the last page comes back
empty and its items are lost.

## The expected fix

Size the last page by what is left after its start:

```diff
-			length = len(items) % size
+			length = len(items) - start
```

No test changes. All 4 tests pass after the fix.
