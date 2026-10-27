import pytest

from paging import paginate


def numbers(count):
    return list(range(1, count + 1))


def test_a_partial_last_page_keeps_the_items_left_over():
    assert paginate(numbers(11), 5) == [[1, 2, 3, 4, 5], [6, 7, 8, 9, 10], [11]]


def test_the_last_page_is_full_when_the_total_is_a_multiple_of_the_page_size():
    assert paginate(numbers(10), 5) == [[1, 2, 3, 4, 5], [6, 7, 8, 9, 10]]


def test_no_items_make_no_pages():
    assert paginate([], 5) == []


def test_a_page_size_of_zero_is_refused():
    with pytest.raises(ValueError):
        paginate(numbers(3), 0)
