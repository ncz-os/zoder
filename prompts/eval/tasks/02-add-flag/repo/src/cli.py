"""A tiny CLI."""


def build_parser():
    import argparse

    parser = argparse.ArgumentParser(prog="tiny")
    parser.add_argument("--verbose", action="store_true")
    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    print("hello")
    return 0
