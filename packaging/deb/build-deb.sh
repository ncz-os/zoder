#!/usr/bin/env bash
# build-deb.sh — turn an already-built `zoder` binary into a binary .deb.
#
# zoder is a single statically-configured Rust binary (rustls everywhere, no
# OpenSSL — see the workspace Cargo.toml), so the package is deliberately thin:
# one executable plus the copyright file Debian policy asks for. There is no
# debian/ source package and no dpkg-buildpackage step; we build the binary in
# CI on a native runner for the target arch and wrap it here.
#
# Usage:
#   packaging/deb/build-deb.sh --binary <path> --arch <amd64|arm64> \
#                              --version <deb-version> [--outdir <dir>]
#
# Emits: <outdir>/zoder_<version>_<arch>.deb  (path echoed on stdout)
set -euo pipefail

BINARY=""
ARCH=""
VERSION=""
OUTDIR="dist"

while [ $# -gt 0 ]; do
  case "$1" in
    --binary)  BINARY="$2";  shift 2 ;;
    --arch)    ARCH="$2";    shift 2 ;;
    --version) VERSION="$2"; shift 2 ;;
    --outdir)  OUTDIR="$2";  shift 2 ;;
    -h|--help) sed -n '2,18p' "$0"; exit 0 ;;
    *) echo "build-deb.sh: unknown argument: $1" >&2; exit 2 ;;
  esac
done

[ -n "$BINARY" ]  || { echo "build-deb.sh: --binary is required"  >&2; exit 2; }
[ -n "$ARCH" ]    || { echo "build-deb.sh: --arch is required"    >&2; exit 2; }
[ -n "$VERSION" ] || { echo "build-deb.sh: --version is required" >&2; exit 2; }
[ -f "$BINARY" ]  || { echo "build-deb.sh: no such binary: $BINARY" >&2; exit 1; }

case "$ARCH" in
  amd64|arm64) ;;
  *) echo "build-deb.sh: --arch must be amd64 or arm64 (got '$ARCH')" >&2; exit 2 ;;
esac

command -v dpkg-deb >/dev/null 2>&1 || {
  echo "build-deb.sh: dpkg-deb not found (run this on a Debian/Ubuntu host)" >&2
  exit 1
}

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"

# Minimum glibc the binary actually needs, read from its versioned symbol
# references, so the dependency is honest rather than a guess. Falls back to
# the oldest glibc any current NCZ / Debian trixie target ships if objdump
# isn't available.
min_glibc() {
  local out
  if command -v objdump >/dev/null 2>&1; then
    out="$(objdump -T "$BINARY" 2>/dev/null \
            | sed -n 's/.*GLIBC_\([0-9]\+\.[0-9]\+\).*/\1/p' \
            | sort -t. -k1,1n -k2,2n \
            | tail -1)"
  fi
  echo "${out:-2.34}"
}

GLIBC_MIN="$(min_glibc)"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

# Every directory is created explicitly: `mktemp -d` gives 0700 and `install -d`
# only applies the mode to the LAST path component, so implicit parents would
# otherwise inherit the builder's umask and ship as 0700/0775 inside the .deb.
for d in DEBIAN usr usr/bin usr/share usr/share/doc usr/share/doc/zoder; do
  install -d -m 0755 "$STAGE/$d"
done
chmod 0755 "$STAGE"
install -m 0755 "$BINARY" "$STAGE/usr/bin/zoder"
# CI builds are not stripped by cargo; do it here so the .deb stays small (the
# ncz registry is storage-capped).
if command -v strip >/dev/null 2>&1; then strip "$STAGE/usr/bin/zoder" || true; fi

install -m 0644 "$REPO_ROOT/LICENSE" "$STAGE/usr/share/doc/zoder/copyright"

INSTALLED_SIZE="$(du -sk "$STAGE" | cut -f1)"

cat > "$STAGE/DEBIAN/control" <<CONTROL
Package: zoder
Version: $VERSION
Architecture: $ARCH
Maintainer: Jason Perlow <jperlow@gmail.com>
Installed-Size: $INSTALLED_SIZE
Depends: libc6 (>= $GLIBC_MIN), libgcc-s1
Section: devel
Priority: optional
Homepage: https://gitlab.com/ncz-os/zoder
Description: Model-routing coding agent for the NCZ stack
 zoder routes coding and review work across local and hosted language-model
 providers, picking a model per task from a cost-and-capability catalog rather
 than pinning one provider. It ships as a single self-contained binary and is
 the designated coding and review agent for the NCZ toolchain.
CONTROL

install -d -m 0755 "$OUTDIR"
DEB="$OUTDIR/zoder_${VERSION}_${ARCH}.deb"
dpkg-deb --build --root-owner-group "$STAGE" "$DEB" >/dev/null

echo "$DEB"
