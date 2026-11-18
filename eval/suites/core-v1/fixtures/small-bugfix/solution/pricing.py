"""Order pricing."""


def total(items, discount_percent=0):
    """Sum of price * qty, minus discount_percent percent, rounded to 2 decimals.

    items: list of (price, qty) tuples.
    """
    subtotal = sum(price * qty for price, qty in items)
    return round(subtotal * (100 - discount_percent) / 100, 2)
