#!/bin/sh
# Pin and verify CI helper binaries so the gate never executes code fetched at
# job time from a moving URL. Closes Finding #15: prior CI pulled `nextest`
# via `get.nexte.st/latest/linux` and installed `cargo-binstall` via
# `curl ... install-from-binstall-release.sh | bash` while job credentials
# were present. A compromised upstream could read the job token and tamper
# with caches or the package registry.
#
# Strategy: pin a SPECIFIC GitHub release tag, pin the asset filename, and
# pin the asset's SHA256 (hardcoded in this script + verified at fetch time
# against the asset the job actually downloaded). The triple
# pin (URL + tag + hash) means an attacker would have to compromise the
# exact immutable tag-and-asset pair to slip past detection.
#
# Versions pinned here; bump intentionally, never "let it float":
#   NEXTEST_VERSION=0.9.137
#   BINSTALL_VERSION=1.20.1
# SHA256s are the published `cargo-nextest-X.Y.Z-<triple>.sha256` /
# computed-from-tag for cargo-binstall. They are immutable for the
# matched tag (GitHub releases + GitHub's release artifact CDN do not
# replace files under an existing tag).
set -eu

CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
BIN_DIR="$CARGO_HOME/bin"
mkdir -p "$BIN_DIR"

NEXTEST_VERSION="${NEXTEST_VERSION:-0.9.137}"
BINSTALL_VERSION="${BINSTALL_VERSION:-1.20.1}"

# Per-target SHA256, fed to install_nextest by platform triple.
nextest_sha() {
    case "$1" in
        x86_64-unknown-linux-gnu)
            echo "38fd6275e111b200bbbed1bd2ae91cbb0d7edd28504879875cff2b3d96f3f311"
            ;;
        x86_64-unknown-linux-musl)
            echo "d5a7d2e76a19b65006939e296eb19d776f820cfaa19514c01cf70a5185a885b8"
            ;;
        *) echo "scripts/ci-prepare-tools: no pinned SHA for nextest target $1" >&2; return 1 ;;
    esac
}

# nextest is published at https://github.com/nextest-rs/nextest/releases
# under tag cargo-nextest-X.Y.Z, with the binary tarball AND a sibling
# `.sha256` file that GitHub serves alongside (same immutable tag).
install_nextest() {
    tag="cargo-nextest-${NEXTEST_VERSION}"
    base="https://github.com/nextest-rs/nextest/releases/download/${tag}"
    # Host-target = x86_64-unknown-linux-gnu for the GitLab Linux CI
    # (cross builds for aarch64 use the same triple set; install only on
    # the native runner to keep the CI deterministic).
    asset_gnu="cargo-nextest-${NEXTEST_VERSION}-x86_64-unknown-linux-gnu.tar.gz"
    sha_gnu="cargo-nextest-${NEXTEST_VERSION}-x86_64-unknown-linux-gnu.sha256"
    asset_musl="cargo-nextest-${NEXTEST_VERSION}-x86_64-unknown-linux-musl.tar.gz"
    sha_musl="cargo-nextest-${NEXTEST_VERSION}-x86_64-unknown-linux-musl.sha256"

    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT INT TERM

    # Try the gnu triple first; fall back to musl only if the host's libc
    # doesn't ship a usable getrandom/at-fallback. CI standard image is
    # glibc so the gnu link is the normal path.
    if [ -f /etc/alpine-release ] || [ ! -d /lib64 ]; then
        asset="$asset_musl"; sha="$sha_musl"; triple="x86_64-unknown-linux-musl"
    else
        asset="$asset_gnu"; sha="$sha_gnu"; triple="x86_64-unknown-linux-gnu"
    fi

    # Fetch the asset and the published sha256 (same immutable tag).
    # HTTPS + GitHub's release artifact CDN = the published SHA cannot
    # drift under the same tag.
    curl --fail --proto '=https' --tlsv1.2 -sSL \
        -o "$work/$asset" "${base}/${asset}"
    curl --fail --proto '=https' --tlsv1.2 -sSL \
        -o "$work/$sha"   "${base}/${sha}"

    # Authoritative SHA256: prefer the upstream-published `.sha256` file
    # (sourced from the SAME immutable tag). Then, for resilience, cross-
    # check against the pinned value baked into this script. Both must
    # match for the install to proceed.
    upstream_sha="$(awk '{print $1}' "$work/$sha")"
    pinned_sha="$(nextest_sha "$triple")"
    if [ -z "$upstream_sha" ] || [ -z "$pinned_sha" ] || [ "$upstream_sha" != "$pinned_sha" ]; then
        echo "scripts/ci-prepare-tools: nextest SHA mismatch (upstream=$upstream_sha, pinned=$pinned_sha)" >&2
        exit 1
    fi

    # Triple-check: recompute the asset SHA and compare both to upstream.
    actual_sha="$(sha256sum "$work/$asset" | awk '{print $1}')"
    if [ "$actual_sha" != "$pinned_sha" ]; then
        echo "scripts/ci-prepare-tools: downloaded nextest SHA mismatch" >&2
        echo "  expected: $pinned_sha" >&2
        echo "  actual:   $actual_sha" >&2
        exit 1
    fi

    tar -xzf "$work/$asset" -C "$work"
    install -m 0755 "$work/cargo-nextest" "$BIN_DIR/cargo-nextest"
    echo "nextest ${NEXTEST_VERSION} installed to $BIN_DIR/cargo-nextest (verified SHA=$pinned_sha)"
}

# cargo-binstall: from `cargo-bins/cargo-binstall` releases. Same pinning
# pattern as nextest: same-tag asset + SHA256 cross-checked against a
# pinned value. Avoids the install-from-binstall-release.sh
# pipe-into-bash pattern that gave a compromised upstream unrestricted
# code execution under job credentials.
install_cargo_binstall() {
    tag="v${BINSTALL_VERSION}"
    base="https://github.com/cargo-bins/cargo-binstall/releases/download/${tag}"
    asset="cargo-binstall-x86_64-unknown-linux-musl.tgz"
    pinned_sha="f12954bc382e1d0b2df3fbfb217a05d92c25570e4517841e0613499a24f4594e"

    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT INT TERM

    curl --fail --proto '=https' --tlsv1.2 -sSL \
        -o "$work/$asset" "${base}/${asset}"

    actual_sha="$(sha256sum "$work/$asset" | awk '{print $1}')"
    if [ "$actual_sha" != "$pinned_sha" ]; then
        echo "scripts/ci-prepare-tools: cargo-binstall SHA mismatch" >&2
        echo "  expected: $pinned_sha" >&2
        echo "  actual:   $actual_sha" >&2
        exit 1
    fi

    tar -xzf "$work/$asset" -C "$work"
    install -m 0755 "$work/cargo-binstall" "$BIN_DIR/cargo-binstall"
    echo "cargo-binstall ${BINSTALL_VERSION} installed to $BIN_DIR/cargo-binstall (verified SHA=$pinned_sha)"
}

case "${1:-}" in
    nextest)    install_nextest ;;
    binstall)   install_cargo_binstall ;;
    *)          echo "usage: ci-prepare-tools.sh {nextest|binstall}" >&2; exit 2 ;;
esac
