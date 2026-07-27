#!/usr/bin/env bash
# publish-deb.sh — upload .deb files to the NCZ Buildkite Packages APT registry.
#
# The registry is https://packages.buildkite.com/ncz-os/ncz (Debian ecosystem,
# public). Publishing is a plain multipart POST; Buildkite regenerates the APT
# index itself, so there is no reprepro/aptly step and no signing key to manage
# on our side.
#
# IMPORTANT — two different Buildkite credential types exist and they are not
# interchangeable:
#   * bkua_… "API access token"  -> can WRITE (publish). This is what this
#     script needs, passed in as $BUILDKITE_PACKAGES_TOKEN.
#   * bkrt_… "registry token"    -> read/consume only (what an installed system
#     puts in /etc/apt/auth.conf.d). It cannot publish.
#
# Usage:
#   BUILDKITE_PACKAGES_TOKEN=… packaging/deb/publish-deb.sh dist/*.deb
#
# Optional overrides:
#   BUILDKITE_ORG       (default: ncz-os)
#   BUILDKITE_REGISTRY  (default: ncz)
set -euo pipefail

ORG="${BUILDKITE_ORG:-ncz-os}"
REGISTRY="${BUILDKITE_REGISTRY:-ncz}"
ENDPOINT="https://api.buildkite.com/v2/packages/organizations/${ORG}/registries/${REGISTRY}/packages"

if [ $# -eq 0 ]; then
  echo "publish-deb.sh: no .deb files given" >&2
  exit 2
fi

if [ -z "${BUILDKITE_PACKAGES_TOKEN:-}" ]; then
  echo "publish-deb.sh: BUILDKITE_PACKAGES_TOKEN is not set — refusing to publish." >&2
  echo "  Set it to the org's write-capable Buildkite API access token (bkua_…)." >&2
  exit 1
fi

rc=0
for deb in "$@"; do
  if [ ! -f "$deb" ]; then
    echo "publish-deb.sh: no such file: $deb" >&2
    rc=1
    continue
  fi

  echo "--- publishing $(basename "$deb") to ${ORG}/${REGISTRY}"
  body="$(mktemp)"
  # --fail-with-body keeps the API's error text on a 4xx/5xx instead of
  # discarding it, which is the difference between "publish failed" and a
  # diagnosable "version already exists" / "resource limit reached".
  if curl --silent --show-error --fail-with-body \
        --retry 3 --retry-connrefused --retry-delay 5 \
        -X POST "$ENDPOINT" \
        -H "Authorization: Bearer ${BUILDKITE_PACKAGES_TOKEN}" \
        -F "file=@${deb}" \
        -o "$body"; then
    # Echo only the non-sensitive identity fields of the created package.
    python3 - "$body" <<'PY' || cat "$body"
import json, sys
with open(sys.argv[1]) as fh:
    pkg = json.load(fh)
print("    published %s %s (%s)" % (
    pkg.get("name", "?"), pkg.get("version", "?"), pkg.get("id", "?")))
print("    %s" % pkg.get("web_url", ""))
PY
  else
    echo "publish-deb.sh: upload failed for $deb" >&2
    sed -e 's/[Bb]earer [A-Za-z0-9_]*/Bearer ***/g' "$body" >&2 || true
    rc=1
  fi
  rm -f "$body"
done

exit "$rc"
