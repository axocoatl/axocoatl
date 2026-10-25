"""Split a list of rows into pages."""


def paginate(rows, size):
    """Return `rows` split into pages of `size` rows.

    Every page but the last is full; the last page holds whatever is left.
    """
    if size <= 0:
        raise ValueError("size must be positive")
    rows = list(rows)
    count = -(-len(rows) // size)
    pages = []
    for page in range(count):
        start = page * size
        length = size if page < count - 1 else len(rows) % size
        pages.append(rows[start:start + length])
    return pages
