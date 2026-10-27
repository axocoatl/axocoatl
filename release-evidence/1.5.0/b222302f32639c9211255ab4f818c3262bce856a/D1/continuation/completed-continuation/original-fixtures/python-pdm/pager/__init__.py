"""Split a list of records into pages."""


def paginate(records, size):
    """Return `records` split into pages of `size` records.

    Every page but the last is full; the last page holds whatever is left.
    """
    if size <= 0:
        raise ValueError("size must be positive")
    records = list(records)
    count = -(-len(records) // size)
    pages = []
    for page in range(count):
        start = page * size
        length = size if page < count - 1 else len(records) % size
        pages.append(records[start:start + length])
    return pages
