# Legacy Linux builds

The legacy build is for Linux computers whose processors do not support
AVX2. It is built for the x86-64-v2 instruction level and smoke-tested on
older physical hardware before publication.

Download the `zoder-x86_64-v2-linux` asset and its `SHA256SUMS` file from the
[GitLab releases](https://gitlab.com/ncz-os/zoder/-/releases) or the
[GitHub releases](https://github.com/ncz-os/zoder/releases). In the directory
containing both files, verify the binary with:

```sh
sha256sum --check SHA256SUMS
```

A new legacy release is cut when source files change outside the corpus,
pricing, benchmark, and documentation data. Data-only updates do not create a
new binary release. Each published build is compiled and smoke-tested on real
older hardware.
