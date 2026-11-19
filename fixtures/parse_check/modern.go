// Package modern holds valid Go that an edit may write: generics and range over functions.
package modern

import (
	_ "embed"
	"fmt"
	"iter"
)

//go:embed modern.go
var source string

type Number interface {
	~int | ~int64 | ~float64
}

type Pair[K comparable, V any] struct {
	Key   K `json:"key"`
	Value V `json:"value,omitempty"`
}

func Map[T, U any](items []T, f func(T) U) []U {
	out := make([]U, 0, len(items))
	for _, item := range items {
		out = append(out, f(item))
	}
	return out
}

func Count(n int) iter.Seq[int] {
	return func(yield func(int) bool) {
		for i := range n {
			if !yield(i) {
				return
			}
		}
	}
}

func Sum[T Number](values ...T) (total T) {
	for v := range Count(len(values)) {
		total += values[v]
	}
	return max(total, 0)
}

func Run(done <-chan struct{}, out chan<- string) error {
	defer close(out)
	go func() { out <- fmt.Sprintf("%d", Sum(1, 2)) }()
loop:
	for {
		select {
		case <-done:
			break loop
		default:
		}
	}
	switch v := any(source).(type) {
	case string:
		_ = v
	}
	return nil
}
