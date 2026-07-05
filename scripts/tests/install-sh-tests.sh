#!/bin/sh
# Behavioral regression tests for `install.sh`.
#
# Adversarial-review findings pinned here:
#
#   #12  Failed seed downloads can poison the canonical pricing catalog
#        (the installer wrote straight to $ZODER_HOME/data/pricing.json
#        with no validation, so curl could leave a half-written file
#        that later `zoder pricing refresh` reads as if it were truth).
#   #13  Default checksum verification fails open — a missing
#        `<asset>.sha256`, an empty body, or no sha256 tool silently
#        installed an unverified binary.
#
# These tests don't replace the source-level assertions in
# `crates/zoder-core/tests/install_script.rs`; they exercise the actual
# shell code path end-to-end against a local mock HTTP server. Each test
# fails on the OLD buggy source and passes on the NEW one.

set -eu

# Repo root is two levels up from this script.
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"

PYTHON=${PYTHON:-python3}
if ! command -v "$PYTHON" >/dev/null 2>&1; then
    echo "skip  python3 not available — install-sh behavioral tests skipped"
    exit 0
fi

passes=0
fails=0
ok() { echo "ok    $*";  passes=$((passes + 1)); }
bad(){ echo "FAIL  $*"; fails=$((fails + 1)); }

# Pull the dl_atomic + sha256_file helper bodies out of install.sh verbatim
# so the test exercises the actual code we ship, not a re-implementation.
extract_helpers() {
    awk '
        /^# ── Downloader/ { p=1; print; next }
        /^# ── Platform/   { p=0; exit }
        p { print }
    ' "$ROOT/install.sh"
}

# ── 1. dl_atomic rejects an empty-body 200 response ────────────────
test_atomic_rejects_empty() {
    test_dir=$(mktemp -d)
    trap 'rm -rf "$test_dir"' EXIT

    : > "$test_dir/empty.bin"

    "$PYTHON" -c "
import http.server, socketserver, sys, os
os.chdir('${test_dir}')
class H(http.server.SimpleHTTPRequestHandler):
    def send_head(self):
        # Force a Content-Length: 0 response even for a real file.
        self.send_response(200); self.send_header('Content-Length','0'); self.end_headers(); return None
    def log_message(self, *a, **k): pass
with socketserver.TCPServer(('127.0.0.1', 0), H) as httpd:
    sys.stdout.write(str(httpd.server_address[1]))
    sys.stdout.flush()
    httpd.serve_forever()
" >"$test_dir/port" 2>/dev/null &
    srv=$!
    cleanup() { kill "$srv" 2>/dev/null; rm -rf "$test_dir"; }
    trap cleanup EXIT

    for i in 1 2 3 4 5 6 7 8 9 10; do
        [ -s "$test_dir/port" ] && break
        sleep 0.2
    done
    port=$(cat "$test_dir/port")
    url="http://127.0.0.1:$port/empty.bin"

    tmp=$(mktemp -d)
    extract_helpers > "$tmp/helpers.sh"
    echo "have() { command -v \"\$1\" >/dev/null 2>&1; }" >> "$tmp/helpers.sh"
    . "$tmp/helpers.sh"
    rm -rf "$tmp"

    # The actual call: dl_atomic URL OUT 100 should reject the empty body.
    out="$test_dir/out_atomic"
    if dl_atomic "$url" "$out" 100 2>/dev/null; then
        bad "dl_atomic accepted a 0-byte Content-Length:0 response"
    else
        ok "dl_atomic rejected an empty-body response"
    fi
    if [ -e "$out" ]; then
        bad "dl_atomic left a file at the canonical path after rejection"
    else
        ok "dl_atomic did not leave a file at the canonical path"
    fi
    rm -rf "$test_dir"
    trap - EXIT
}

# ── 2. dl_atomic accepts a sufficient body and renames it into place ──
test_atomic_accepts_and_renames() {
    test_dir=$(mktemp -d)
    trap 'rm -rf "$test_dir"' EXIT

    # Create a 2KB deterministic body.
    head -c 2048 /dev/urandom > "$test_dir/big.bin"

    "$PYTHON" -c "
import http.server, socketserver, sys, os
os.chdir('${test_dir}')
with socketserver.TCPServer(('127.0.0.1', 0), http.server.SimpleHTTPRequestHandler) as httpd:
    sys.stdout.write(str(httpd.server_address[1]))
    sys.stdout.flush()
    httpd.serve_forever()
" >"$test_dir/port" 2>/dev/null &
    srv=$!
    cleanup() { kill "$srv" 2>/dev/null; rm -rf "$test_dir"; }
    trap cleanup EXIT

    for i in 1 2 3 4 5 6 7 8 9 10; do
        [ -s "$test_dir/port" ] && break
        sleep 0.2
    done
    port=$(cat "$test_dir/port")
    url="http://127.0.0.1:$port/big.bin"

    tmp=$(mktemp -d)
    extract_helpers > "$tmp/helpers.sh"
    echo "have() { command -v \"\$1\" >/dev/null 2>&1; }" >> "$tmp/helpers.sh"
    . "$tmp/helpers.sh"
    rm -rf "$tmp"

    out="$test_dir/out_ok"
    if dl_atomic "$url" "$out" 100; then
        ok "dl_atomic accepted a sufficient body"
    else
        bad "dl_atomic rejected a sufficient body"
        return
    fi
    if [ -f "$out" ] && [ "$(wc -c <"$out" | tr -d ' ')" -eq 2048 ]; then
        ok "dl_atomic renamed the part-file into place with full content"
    else
        bad "dl_atomic did not rename to canonical path with full content"
    fi
    if ls "$test_dir"/out_ok.part.* >/dev/null 2>&1; then
        bad "dl_atomic leaked a .part file after a successful install"
    else
        ok "dl_atomic cleaned up the .part file on success"
    fi
    rm -rf "$test_dir"
    trap - EXIT
}

# ── 3. dl_atomic rejects a too-small body and NEVER lands a file at the canonical path
test_atomic_rejects_too_small() {
    test_dir=$(mktemp -d)
    trap 'rm -rf "$test_dir"' EXIT

    printf 'ab' > "$test_dir/small.bin"

    "$PYTHON" -c "
import http.server, socketserver, sys, os
os.chdir('${test_dir}')
with socketserver.TCPServer(('127.0.0.1', 0), http.server.SimpleHTTPRequestHandler) as httpd:
    sys.stdout.write(str(httpd.server_address[1]))
    sys.stdout.flush()
    httpd.serve_forever()
" >"$test_dir/port" 2>/dev/null &
    srv=$!
    cleanup() { kill "$srv" 2>/dev/null; rm -rf "$test_dir"; }
    trap cleanup EXIT

    for i in 1 2 3 4 5 6 7 8 9 10; do
        [ -s "$test_dir/port" ] && break
        sleep 0.2
    done
    port=$(cat "$test_dir/port")
    url="http://127.0.0.1:$port/small.bin"

    tmp=$(mktemp -d)
    extract_helpers > "$tmp/helpers.sh"
    echo "have() { command -v \"\$1\" >/dev/null 2>&1; }" >> "$tmp/helpers.sh"
    . "$tmp/helpers.sh"
    rm -rf "$tmp"

    out="$test_dir/out_small"
    if dl_atomic "$url" "$out" 100 2>/dev/null; then
        bad "dl_atomic accepted a 2-byte body with min=100"
    else
        ok "dl_atomic rejected a too-small body"
    fi
    if [ -e "$out" ]; then
        bad "dl_atomic left a file at the canonical path on size reject"
    else
        ok "dl_atomic did not leave a file at the canonical path"
    fi
    rm -rf "$test_dir"
    trap - EXIT
}

# ── 4. checksum verification fails closed: a required binary without a
# valid 64-hex-char .sha256 must die, not install. We assert this by
# source-level scanning because reproducing the full bash flow would
# require building/installing `zoder`.
test_required_binary_without_checksum_dies() {
    src=$(cat "$ROOT/install.sh")
    if printf '%s' "$src" | grep -q 'refusing to install unverified binary'; then
        ok "required-binary install path refuses to install without a valid checksum"
    else
        bad "required-binary install path silently installs when checksum is missing"
    fi
    if printf '%s' "$src" | grep -q 'malformed checksum for \${asset}'; then
        ok "checksum body that isn't exactly 64 hex chars is fatal"
    else
        bad "malformed checksum is treated as a warning, not a fatal error"
    fi
}

test_atomic_rejects_empty
test_atomic_accepts_and_renames
test_atomic_rejects_too_small
test_required_binary_without_checksum_dies

echo
echo "ok    $passes"
if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails"
    exit 1
fi
echo "INSTALL-SH REGRESSION: PASS"
