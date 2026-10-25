# python-poetry

A Python project managed with Poetry. pytest is not part of Python: it is in
Poetry's `test` dependency group (`pytest = "^8.0"`), which pip cannot read,
so the run's setup gives it to pip as a requirement (`pip install -e .
'pytest>=8.0,<9'`); building the project needs poetry-core from PyPI. Without
the setup the check fails with `No module named pytest`.

- Test command: `pytest` (after the setup above)
- Before the fix: 1 of 4 tests fails
  (`test_the_last_page_is_full_when_the_total_is_a_multiple_of_the_page_size`).

## The bug

Paging skips the last item when the total is a multiple of the page size.
`paginate(entries, size)` in `paging/__init__.py` sizes the last page as
`len(entries) % size`, which is 0 when the total divides evenly, so the last
page comes back empty and its items are lost.

## The expected fix

Size the last page by what is left after its start:

```diff
-        length = size if page < count - 1 else len(entries) % size
+        length = size if page < count - 1 else len(entries) - start
```

No test changes. All 4 tests pass after the fix.
