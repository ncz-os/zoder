from src.tokenize import tokens


def test_tokens():
    assert tokens("a b") == ["a", "b"]
