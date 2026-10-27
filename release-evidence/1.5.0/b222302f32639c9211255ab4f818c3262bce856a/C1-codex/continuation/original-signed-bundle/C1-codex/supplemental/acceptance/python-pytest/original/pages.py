"""Split a list of items into pages."""


def paginate(items, size):
    """Return `items` split into pages of `size` items.

    Every page but the last is full; the last page holds whatever is left.
    """
    if size <= 0:
        raise ValueError("size must be positive")
    items = list(items)
    count = -(-len(items) // size)
    pages = []
    for page in range(count):
        start = page * size
        length = size if page < count - 1 else len(items) % size
        pages.append(items[start:start + length])
    return pages
