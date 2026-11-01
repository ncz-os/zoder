"""The data module."""


def load(path):
    with open(path) as fh:
        return fh.read()
