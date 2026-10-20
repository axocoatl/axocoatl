package pages

import (
	"reflect"
	"testing"
)

func numbers(count int) []int {
	out := make([]int, count)
	for index := range out {
		out[index] = index + 1
	}
	return out
}

func TestAPartialLastPageKeepsTheItemsLeftOver(t *testing.T) {
	got := Paginate(numbers(11), 5)
	want := [][]int{{1, 2, 3, 4, 5}, {6, 7, 8, 9, 10}, {11}}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("Paginate(11 items, 5) = %v, want %v", got, want)
	}
}

func TestTheLastPageIsFullWhenTheTotalIsAMultipleOfThePageSize(t *testing.T) {
	got := Paginate(numbers(10), 5)
	want := [][]int{{1, 2, 3, 4, 5}, {6, 7, 8, 9, 10}}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("Paginate(10 items, 5) = %v, want %v", got, want)
	}
}

func TestNoItemsMakeNoPages(t *testing.T) {
	if got := Paginate([]int{}, 5); len(got) != 0 {
		t.Fatalf("Paginate(no items, 5) = %v, want no pages", got)
	}
}

func TestAPageSizeOfZeroIsRefused(t *testing.T) {
	defer func() {
		if recover() == nil {
			t.Fatal("Paginate with size 0 did not panic")
		}
	}()
	Paginate(numbers(3), 0)
}
