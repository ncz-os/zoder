#!/usr/bin/sh
# Behavioral tests for scripts/ci/assert-trio-manifest.sh (Finding #17).
# Each test fails on the OLD code (which let an unsigned, single-arch
# release publish) and passes on the NEW one.
set -eu

PASSES=0
FAILS=0
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
SCRIPT="$ROOT/scripts/ci/assert-trio-manifest.sh"

ok()   { echo "ok    $*";  PASSES=$((PASSES + 1)); }
bad()  { echo "FAIL  $*";  FAILS=$((FAILS + 1)); }

run_assert() {
    # $1 = a temp dir with dist/ populated; $2 = expected exit code.
    workdir="$1"
    want="$2"
    DIST="$workdir/dist" "$SCRIPT" > /dev/null 2>&1
    rc=$?
    if [ "$rc" = "$want" ]; then
        return 0
    fi
    return 1
}

# Test 1: clean tri-arch build with manifest + .sha256 files must exit 0.
test_clean_pass() {
    workdir=$(mktemp -d)
    mkdir -p "$workdir/dist"
    cat > "$workdir/dist/manifest.json" <<EOF
{"zoder": {"head": "deadbeef", "version": "0.2.1"}, "zeroclaw": {"sha": "0123456789abcdef0123456789abcdef01234567"}, "goose": {"sha": "abcdef0123456789abcdef0123456789abcdef01", "ref": "v1.39.0"}}
EOF
    # Create tarballs with matching .sha256
    for t in x86_64-unknown-linux-gnu aarch64-apple-darwin aarch64-unknown-linux-gnu; do
        file="$workdir/dist/zoder-0.2.1-$t.tar.gz"
        echo "fake binary $t" > "$file"
        (cd "$workdir/dist" && sha256sum "$(basename "$file")" > "$(basename "$file").sha256")
    done
    if run_assert "$workdir" 0; then
        ok "clean tri-arch build with manifest + sha256 -> exit 0"
    else
        bad "clean tri-arch build REJECTED (false alarm)"
    fi
    rm -rf "$workdir"
}

# Test 2: missing manifest.json must fail (Finding #17: trio metadata
# is part of the release contract).
test_missing_manifest() {
    workdir=$(mktemp -d)
    mkdir -p "$workdir/dist"
    file="$workdir/dist/zoder-0.2.1-x86_64-unknown-linux-gnu.tar.gz"
    echo "fake" > "$file"
    if run_assert "$workdir" 1; then
        ok "missing manifest.json -> nonzero exit (release contract enforced)"
    else
        bad "missing manifest.json SLIPPED THROUGH (OLD bug)"
    fi
    rm -rf "$workdir"
}

# Test 3: tarball without sibling .sha256 must fail (Finding #28).
test_missing_sha256() {
    workdir=$(mktemp -d)
    mkdir -p "$workdir/dist"
    cat > "$workdir/dist/manifest.json" <<EOF
{"zoder": {"head": "deadbeef", "version": "0.2.1"}, "zeroclaw": {"sha": "0123456789abcdef0123456789abcdef01234567"}}
EOF
    file="$workdir/dist/zoder-0.2.1-x86_64-unknown-linux-gnu.tar.gz"
    echo "fake" > "$file"
    # DO NOT create the sibling .sha256
    if run_assert "$workdir" 1; then
        ok "tarball without sibling .sha256 -> nonzero exit (Finding #28 reproduced)"
    else
        bad "tarball without .sha256 SLIPPED THROUGH (OLD bug: missing checksums uploaded)"
    fi
    rm -rf "$workdir"
}

# Test 4: tarball with mismatched .sha256 must fail.
test_sha256_mismatch() {
    workdir=$(mktemp -d)
    mkdir -p "$workdir/dist"
    cat > "$workdir/dist/manifest.json" <<EOF
{"zoder": {"head": "deadbeef", "version": "0.2.1"}, "zeroclaw": {"sha": "0123456789abcdef0123456789abcdef01234567"}}
EOF
    file="$workdir/dist/zoder-0.2.1-x86_64-unknown-linux-gnu.tar.gz"
    echo "fake content" > "$file"
    # Wrong sha256 — points at a phantom file
    echo "0000000000000000000000000000000000000000000000000000000000000000  $(basename "$file")" > "$file.sha256"
    if run_assert "$workdir" 1; then
        ok "tarball with mismatched .sha256 -> nonzero exit (repudiation blocked)"
    else
        bad "tarball with MISMATCHED .sha256 SLIPPED THROUGH (OLD bug)"
    fi
    rm -rf "$workdir"
}

# Test 5: manifest with no zeroclaw SHA must fail (CI requires TUI build).
test_manifest_no_zeroclaw_sha() {
    workdir=$(mktemp -d)
    mkdir -p "$workdir/dist"
    cat > "$workdir/dist/manifest.json" <<EOF
{"zoder": {"head": "deadbeef", "version": "0.2.1"}, "zeroclaw": {"sha": ""}}
EOF
    file="$workdir/dist/zoder-0.2.1-x86_64-unknown-linux-gnu.tar.gz"
    echo "fake" > "$file"
    (cd "$workdir/dist" && sha256sum "$(basename "$file")" > "$(basename "$file").sha256")
    if run_assert "$workdir" 1; then
        ok "manifest with empty zeroclaw.sha -> nonzero exit (engine pin required)"
    else
        bad "manifest with EMPTY zeroclaw.sha SLIPPED THROUGH"
    fi
    rm -rf "$workdir"
}

test_clean_pass
test_missing_manifest
test_missing_sha256
test_sha256_mismatch
test_manifest_no_zeroclaw_sha

echo
echo "$PASSES passed; $FAILS failed"
[ "$FAILS" = 0 ]
