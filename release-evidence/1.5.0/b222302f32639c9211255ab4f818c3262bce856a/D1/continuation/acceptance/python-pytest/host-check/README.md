# python-pytest

A Python project tested with pytest. pytest is not part of Python: it comes
from the project's `test` extra, so the check needs a setup step such as
`pip install -e '.[test]'` first. Without it the check fails with
`pytest: not found` (or `No module named pytest`).

- Test command: `pytest` (after `pip install -e '.[test]'`)
- Before the fix: 1 of 4 tests fails
  (`test_the_last_page_is_full_when_the_total_is_a_multiple_of_the_page_size`).

## The bug

Paging skips the last item when the total is a multiple of the page size.
`paginate(items, size)` in `pages.py` sizes the last page as
`len(items) % size`, which is 0 when the total divides evenly, so the last
page comes back empty and its items are lost.

## The expected fix

Size the last page by what is left after its start:

```diff
-        length = size if page < count - 1 else len(items) % size
+        length = size if page < count - 1 else len(items) - start
```

No test changes. All 4 tests pass after the fix.
