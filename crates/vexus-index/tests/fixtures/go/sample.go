package inventory

import "fmt"

const MaxItems = 100

// Item is one stocked product.
type Item struct {
	Name  string
	Count int
}

type Stocker interface {
	Restock(n int) error
}

type Quantity int

// Restock adds n units.
func (it *Item) Restock(n int) error {
	if n <= 0 {
		return fmt.Errorf("bad restock %d", n)
	}
	it.Count += n
	return nil
}

func Describe(it Item, verbose bool) string {
	if verbose {
		return fmt.Sprintf("%s x%d", it.Name, it.Count)
	}
	return it.Name
}
