#!/usr/bin/env bash
# deb-version.sh — map a release tag to the Debian package version.
#
# Tag policy (kernel-style): release tags are `vX.Y.Z`, release candidates
# are `vX.Y.0-rcN` (RCs only on .0). Anything else — nightly, stack-*,
# calendar tags, RCs on non-zero patch — is rejected with a non-zero exit
# so nothing is built or published from it.
#
# Mapping:
#   vX.Y.Z      -> X.Y.Z        (final release)
#   vX.Y.0-rcN  -> X.Y.0~rcN    (`~` sorts BELOW the final release in Debian
#                               version comparison, so an RC can never shadow
#                               the real release for `apt upgrade`; a `-`
#                               suffix would sort ABOVE it)
#
# Usage: packaging/deb/deb-version.sh <tag>
# Prints the deb version on stdout. Exit 0 on success, 2 on rejection.
set -euo pipefail

TAG="${1:-}"

if [[ "$TAG" =~ ^v([0-9]+)\.([0-9]+)\.0-rc([0-9]+)$ ]]; then
  echo "${TAG#v}" | sed 's/-rc/~rc/'
  exit 0
fi

if [[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "${TAG#v}"
  exit 0
fi

echo "deb-version.sh: rejected tag '$TAG': want vX.Y.Z or vX.Y.0-rcN" >&2
exit 2
