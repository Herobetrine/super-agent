"""Inventory data layer."""


class Store:
    def __init__(self, items=None):
        self.items = dict(items or {})

    def add(self, name, qty):
        self.items[name] = self.items.get(name, 0) + qty

    def quantity(self, name):
        return self.items.get(name, 0)
