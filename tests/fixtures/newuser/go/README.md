# go

A Go module with no dependencies, tested with `go test ./...`. It needs a Go
toolchain in the run's image; a run in an image without `go` cannot check
it.

- Test command: `go test ./...`
- Before the fix: 1 of 4 tests fails
  (`TestTheLastPageIsFullWhenTheTotalIsAMultipleOfThePageSize`).

## The bug

Paging skips the last item when the total is a multiple of the page size.
`Paginate(items, size)` in `pages.go` sizes the last page as
`len(items) % size`, which is 0 when the total divides evenly, so the last
page comes back empty and its items are lost.

## The expected fix

Size the last page by what is left after its start:

```diff
-			length = len(items) % size
+			length = len(items) - start
```

No test changes. All 4 tests pass after the fix.
