// Bundled go fixture corpus (E5 disposition): the fixed body of real-shape
// code the LangRegistry seeds its kind table from at first use. Parsed only.

package widgets

import (
	"errors"
	"fmt"
	"strings"
)

// Widget is a documented unit exercising structs, methods and generics.
type Widget[T comparable] struct {
	ID    int
	Inner T
	tags  []string
}

type Kind int

const (
	Alpha Kind = iota
	Beta
	Gamma
)

type Inspector interface {
	Inspect() string
	Classify() (Kind, error)
}

func (w *Widget[T]) Inspect() string {
	return fmt.Sprintf("widget %d holding %v", w.ID, w.Inner)
}

func (w *Widget[T]) Classify() (Kind, error) {
	switch {
	case w.ID == 0:
		return Alpha, nil
	case w.ID%2 == 0:
		return Beta, nil
	default:
		return Gamma, errors.New("odd id")
	}
}

func Tally(rows map[int]int) (total int) {
	for key, value := range rows {
		total += value * (key + 1)
	}
	return total
}

func Stream(ch chan int, done chan struct{}) {
	defer close(done)
	for i := 0; i < 10; i++ {
		select {
		case ch <- i:
		case <-done:
			return
		}
	}
}

func main() {
	w := &Widget[int]{ID: 7, Inner: 42, tags: []string{"a", "b"}}
	go Stream(make(chan int, 1), make(chan struct{}))
	var numbers = []int{1, 2, 3}
	if len(numbers) > 0 && w.ID > 0 {
		fmt.Println(strings.Join(w.tags, ","), Tally(map[int]int{1: 2}))
	}
}
