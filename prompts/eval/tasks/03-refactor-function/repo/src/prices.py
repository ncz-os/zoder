"""Price helpers."""


def total(items):
    t = 0
    for i in items:
        t = t + i["price"] * i["qty"]
    return t
