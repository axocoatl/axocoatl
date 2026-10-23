"""Split a list of entries into pages."""


def paginate(entries, size):
    """Return `entries` split into pages of `size` entries.

    Every page but the last is full; the last page holds whatever is left.
    """
    if size <= 0:
        raise ValueError("size must be positive")
    entries = list(entries)
    count = -(-len(entries) // size)
    pages = []
    for page in range(count):
        start = page * size
        length = size if page < count - 1 else len(entries) % size
        pages.append(entries[start:start + length])
    return pages
