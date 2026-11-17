#!/usr/bin/env python3
"""Mirror verified legacy releases from the public GitLab project to GitHub."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any

PROJECT = "ncz-os/zoder"
API = "https://gitlab.com/api/v4"
PREFIX = "legacy-x86_64-v2-"
EXPECTED = {"zoder-x86_64-v2-linux", "SHA256SUMS"}


def http_request(url: str) -> tuple[int, bytes]:
    """The sole HTTP seam; tests replace this function."""
    try:
        with urllib.request.urlopen(url, timeout=60) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read()


def checksum_matches(binary: bytes, sums: bytes) -> bool:
    try:
        lines = sums.decode("utf-8").splitlines()
    except UnicodeDecodeError:
        return False
    for line in lines:
        match = re.fullmatch(r"([0-9a-fA-F]{64})\s+\*?zoder-x86_64-v2-linux", line.strip())
        if match:
            return hashlib.sha256(binary).hexdigest().lower() == match.group(1).lower()
    return False


def asset_urls(release: dict[str, Any]) -> dict[str, str]:
    links = release.get("assets", {}).get("links", [])
    return {str(item.get("name")): str(item.get("direct_asset_url") or item.get("url")) for item in links if item.get("name") in EXPECTED}


def list_releases() -> list[dict[str, Any]]:
    releases: list[dict[str, Any]] = []
    page = 1
    while True:
        url = f"{API}/projects/{urllib.parse.quote(PROJECT, safe='')}/releases?per_page=100&page={page}"
        status, body = http_request(url)
        if status < 200 or status >= 300:
            raise RuntimeError(f"release listing returned HTTP {status}")
        batch = json.loads(body)
        releases.extend(batch)
        if len(batch) < 100:
            return releases
        page += 1


def mirror(releases: list[dict[str, Any]], existing_tags: set[str], repo: str, gh: Any = subprocess.run, temp_root: str | None = None) -> list[str]:
    published: list[str] = []
    for release in releases:
        tag = str(release.get("tag_name", ""))
        if not tag.startswith(PREFIX) or tag in existing_tags:
            continue
        urls = asset_urls(release)
        if set(urls) != EXPECTED:
            raise RuntimeError(f"release {tag} is missing required assets")
        payloads: dict[str, bytes] = {}
        for name, url in urls.items():
            status, body = http_request(url)
            if status < 200 or status >= 300:
                raise RuntimeError(f"asset download returned HTTP {status}")
            payloads[name] = body
        if not checksum_matches(payloads["zoder-x86_64-v2-linux"], payloads["SHA256SUMS"]):
            raise RuntimeError(f"checksum verification failed for {tag}")
        with tempfile.TemporaryDirectory(prefix="legacy-mirror-", dir=temp_root) as directory:
            binary = Path(directory) / "zoder-x86_64-v2-linux"
            sums = Path(directory) / "SHA256SUMS"
            binary.write_bytes(payloads[binary.name])
            sums.write_bytes(payloads[sums.name])
            notes = str(release.get("description", ""))
            gh(["gh", "release", "create", tag, str(binary), str(sums), "--repo", repo, "--title", tag, "--notes", notes], check=True)
        published.append(tag)
    return published


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", ""))
    args = parser.parse_args()
    if not args.repo:
        print("legacy-release-mirror: GITHUB_REPOSITORY is required", file=sys.stderr)
        return 1
    try:
        releases = list_releases()
        result = subprocess.run(["gh", "release", "list", "--repo", args.repo, "--limit", "1000", "--json", "tagName"], check=True, capture_output=True, text=True)
        existing = {item["tagName"] for item in json.loads(result.stdout)}
        made = mirror(releases, existing, args.repo, temp_root=os.environ.get("RUNNER_TEMP"))
        print("mirrored: " + (", ".join(made) if made else "none"))
        return 0
    except Exception as exc:
        print(f"legacy-release-mirror: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
