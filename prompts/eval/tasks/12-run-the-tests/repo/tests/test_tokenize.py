from src.tokenize import tokens


def test_tokens():
    assert tokens("a,b") == ["a", "b"]


def test_tokens_drops_empty_pieces():
    assert tokens("a,,b") == ["a", "b"]
