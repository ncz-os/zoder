from src.prices import total


def test_total():
    assert total([{"price": 2, "qty": 3}, {"price": 5, "qty": 1}]) == 11
