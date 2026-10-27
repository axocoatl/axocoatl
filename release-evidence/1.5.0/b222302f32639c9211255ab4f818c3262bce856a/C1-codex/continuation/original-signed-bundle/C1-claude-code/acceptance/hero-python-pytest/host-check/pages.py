"""Split a list of items into pages."""


def paginate(items, size):
    """Return `items` split into pages of `size` items.

    Every page but the last is full; the last page holds whatever is left.
    """
    if size <= 0:
        raise ValueError("size must be positive")
    items = list(items)
    return [items[start:start + size] for start in range(0, len(items), size)]
