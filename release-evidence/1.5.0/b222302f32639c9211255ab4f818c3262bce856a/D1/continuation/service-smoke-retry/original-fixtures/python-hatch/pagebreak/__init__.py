"""Split a list of elements into pages."""


def paginate(elements, size):
    """Return `elements` split into pages of `size` elements.

    Every page but the last is full; the last page holds whatever is left.
    """
    if size <= 0:
        raise ValueError("size must be positive")
    elements = list(elements)
    count = -(-len(elements) // size)
    pages = []
    for page in range(count):
        start = page * size
        length = size if page < count - 1 else len(elements) % size
        pages.append(elements[start:start + length])
    return pages
