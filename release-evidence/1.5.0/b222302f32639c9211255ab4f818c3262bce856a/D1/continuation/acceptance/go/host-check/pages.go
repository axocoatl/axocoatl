// Package pages splits a list of items into pages.
package pages

// Paginate returns items split into pages of size items. Every page but the
// last is full; the last page holds whatever is left. It panics when size is
// not positive.
func Paginate[T any](items []T, size int) [][]T {
	if size <= 0 {
		panic("size must be positive")
	}
	count := (len(items) + size - 1) / size
	pages := make([][]T, 0, count)
	for page := 0; page < count; page++ {
		start := page * size
		length := size
		if page == count-1 {
			length = len(items) - start
		}
		pages = append(pages, items[start:start+length])
	}
	return pages
}
