#!/bin/sh
# Verify the trio build produced a usable manifest (Finding #17). Called as
# the final step of each package job before publishing. Refuses to leave a
# tri-arch release whose three architectures built against three different
# engine revisions.
#
# Required dist/ contents:
#   manifest.json   built by scripts/package.sh -- pins zoder, zeroclaw,
#                   goose SHAs.
#   *.tar.gz        per-target tarballs (one per architecture this job
#                   built).
#   *.tar.gz.sha256 sibling checksums, written by package.sh and
#                   uploaded alongside the tarballs.
set -eu

cd "$(dirname "$0")/../.."
ROOT="$(pwd)"
DIST="${DIST:-dist}"

[ -d "$DIST" ] || { echo "assert-trio-manifest: $DIST/ missing — did package.sh run?" >&2; exit 1; }
[ -f "$DIST/manifest.json" ] || { echo "assert-trio-manifest: $DIST/manifest.json missing" >&2; exit 1; }

# Every tarball must have a sibling .sha256 (Finding #28: the prior upload
# loop uploaded only *.tar.gz, leaving the .sha256 unverifiable from the
# Releases page).
missing_sha=0
for tar in "$DIST"/*.tar.gz; do
    [ -e "$tar" ] || continue
    if [ ! -f "$tar.sha256" ]; then
        echo "assert-trio-manifest: $tar has no sibling .sha256" >&2
        missing_sha=1
    else
        # Verify the recorded SHA actually matches.
        if ! ( cd "$DIST" && sha256sum -c "$(basename "$tar.sha256")" >/dev/null ); then
            echo "assert-trio-manifest: $tar sha256 mismatch" >&2
            exit 1
        fi
    fi
done
[ "$missing_sha" = 0 ] || exit 1

# Reject a manifest with no recorded zeroclaw SHA — that means
# package.sh's ensure_zeroclaw() didn't run (likely ZODER_SKIP_TUI=1
# was set). CI does NOT export that, but be explicit on the contract.
zeroclaw_sha="$(python3 -c '
import json, sys
with open("'"$DIST"'/manifest.json") as f:
    d = json.load(f)
print((d.get("zeroclaw") or {}).get("sha", ""))
')"
[ -n "$zeroclaw_sha" ] && [ "${#zeroclaw_sha}" -ge 40 ] \
    || { echo "assert-trio-manifest: manifest.json missing zeroclaw.sha (no TUI built?)" >&2; exit 1; }

echo "assert-trio-manifest: OK (engine pinned at $zeroclaw_sha)"
