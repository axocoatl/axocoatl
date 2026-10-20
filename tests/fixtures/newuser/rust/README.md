# rust

A Rust library crate with no dependencies, tested with `cargo test`. It needs
a Rust toolchain in the run's image; a run in an image without `cargo`
cannot check it.

- Test command: `cargo test`
- Before the fix: 1 of 4 tests fails
  (`the_last_page_is_full_when_the_total_is_a_multiple_of_the_page_size`).

## The bug

Paging skips the last item when the total is a multiple of the page size.
`paginate(items, size)` in `src/lib.rs` sizes the last page as
`items.len() % size`, which is 0 when the total divides evenly, so the last
page comes back empty and its items are lost.

## The expected fix

Size the last page by what is left after its start:

```diff
-            items.len() % size
+            items.len() - start
```

No test changes. All 4 tests pass after the fix.
