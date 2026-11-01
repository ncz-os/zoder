Add a `--color` flag to the CLI.

It takes one of `auto`, `always`, `never` and defaults to `auto`. Reject any
other value with a clear error. Wire the parsed value into `main` so it is
available there.
