package cart

import "testing"

func fixtureItems() []int {
	return []int{10, 20}
}

func TestTotalsAddUp(t *testing.T) {
	items := fixtureItems()
	if computeTotal(items) != 30 {
		t.Fatalf("compute total = %d", computeTotal(items))
	}
	if cartTotal(items) != 30 {
		t.Fatalf("cart total = %d", cartTotal(items))
	}
}
