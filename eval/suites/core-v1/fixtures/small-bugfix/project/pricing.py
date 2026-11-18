"""Order pricing."""


def total(items, discount_percent=0):
    """Sum of price * qty, minus discount_percent percent, rounded to 2 decimals.

    items: list of (price, qty) tuples.
    """
    subtotal = 0
    for price, qty in items:
        subtotal += price * qty
    return round(subtotal - discount_percent, 2)
