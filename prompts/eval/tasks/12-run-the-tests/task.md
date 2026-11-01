`tokens` in `src/tokenize.py` should drop empty pieces - right now
`tokens("a,,b")` returns an empty string in the middle. Fix it and make
sure the test suite still passes.
