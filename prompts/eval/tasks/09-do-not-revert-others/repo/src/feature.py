"""A feature flag reader."""


def is_enabled(flags, name, default=False):
    return bool(flags.get(name, default))
