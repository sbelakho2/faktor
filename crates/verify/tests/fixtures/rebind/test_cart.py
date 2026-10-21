from cart import compute_total, cart_total


def test_totals_add_up():
    items = [10, 20]
    assert compute_total(items) == 30
    assert cart_total(items) == 30
