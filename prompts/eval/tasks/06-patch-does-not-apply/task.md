A patch was written against an older version of `src/greet.py` and it
does not apply. Re-anchor it against the file as it is now and apply the
intended change: `greet` should also accept an optional `greeting` argument
that defaults to `"Hello"`.

Do not overwrite the whole file to make the patch fit.
