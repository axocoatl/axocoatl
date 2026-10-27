//! Split a slice of items into pages.

/// `items` split into pages of `size` items. Every page but the last is
/// full; the last page holds whatever is left.
///
/// # Panics
///
/// When `size` is zero.
pub fn paginate<T: Clone>(items: &[T], size: usize) -> Vec<Vec<T>> {
    assert!(size > 0, "size must be positive");
    let count = items.len().div_ceil(size);
    let mut pages = Vec::with_capacity(count);
    for page in 0..count {
        let start = page * size;
        let length = if page + 1 < count {
            size
        } else {
            // If remainder is 0, it means we have a complete last page
            if items.len() % size == 0 {
                size
            } else {
                items.len() % size
            }
        };
        pages.push(items[start..start + length].to_vec());
    }
    pages
}

#[cfg(test)]
mod tests {
    use super::paginate;
    // A diff of the two values when an assertion fails.
    use pretty_assertions::assert_eq;

    fn numbers(count: u32) -> Vec<u32> {
        (1..=count).collect()
    }

    #[test]
    fn a_partial_last_page_keeps_the_items_left_over() {
        assert_eq!(
            paginate(&numbers(11), 5),
            vec![vec![1, 2, 3, 4, 5], vec![6, 7, 8, 9, 10], vec![11]]
        );
    }

    #[test]
    fn the_last_page_is_full_when_the_total_is_a_multiple_of_the_page_size() {
        assert_eq!(
            paginate(&numbers(10), 5),
            vec![vec![1, 2, 3, 4, 5], vec![6, 7, 8, 9, 10]]
        );
    }

    #[test]
    fn no_items_make_no_pages() {
        assert!(paginate::<u32>(&[], 5).is_empty());
    }

    #[test]
    #[should_panic(expected = "size must be positive")]
    fn a_page_size_of_zero_is_refused() {
        paginate(&numbers(3), 0);
    }
}
