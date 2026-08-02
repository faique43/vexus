package worker

import "fmt"

// StockLine is one warehouse shelf entry for a product SKU.
type StockLine struct {
	Sku   string
	Count int
}

// lowWaterMark is the count at which a SKU is considered running out.
const lowWaterMark = 5

// needsRestock reports whether a stock line has fallen below the low
// water mark and should be queued for the next replenishment run.
func needsRestock(line StockLine) bool {
	return line.Count < lowWaterMark
}

// RestockReport lists every SKU below the low water mark, formatted one
// per line for the operations channel.
func RestockReport(lines []StockLine) string {
	out := ""
	for _, line := range lines {
		if needsRestock(line) {
			out += fmt.Sprintf("restock %s (have %d)\n", line.Sku, line.Count)
		}
	}
	return out
}
