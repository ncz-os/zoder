#!/usr/bin/env python3
"""Publish a smoke-tested x86-64-v2 legacy build from GitLab CI."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

PREFIX = "legacy-x86_64-v2-"
FINGERPRINT_RE = re.compile(r"(?m)^Source fingerprint: ([0-9a-f]{64})$")


def http_request(method: str, url: str, headers: dict[str, str], data: bytes | None = None) -> tuple[int, bytes]:
    """The sole HTTP seam; tests replace this function."""
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=60) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read()


def source_fingerprint(commit: str, cwd: str | Path = ".") -> str:
    """Hash tracked git tree entries, excluding data and documentation trees."""
    raw = subprocess.check_output(["git", "ls-tree", "-rz", "--full-tree", "-r", commit], cwd=cwd)
    excluded = (b"corpus/", b"pricing/", b"bench/", b"docs/")
    entries = [entry for entry in raw.split(b"\0") if entry and not entry.split(b"\t", 1)[1].startswith(excluded)]
    return hashlib.sha256(b"\0".join(entries) + b"\0").hexdigest()


def sha256_file(path: str | Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def checksums(binary: str | Path) -> str:
    return f"{sha256_file(binary)}  zoder-x86_64-v2-linux\n"


def needs_release(fingerprint: str, releases: list[dict[str, Any]]) -> bool:
    candidates = [r for r in releases if str(r.get("tag_name", "")).startswith(PREFIX)]
    if not candidates:
        return True
    # GitLab returns newest releases first. Sort defensively by created_at/date.
    newest = max(candidates, key=lambda r: str(r.get("created_at") or r.get("released_at") or ""))
    match = FINGERPRINT_RE.search(str(newest.get("description", "")))
    return not match or match.group(1) != fingerprint


def _json_call(method: str, url: str, token: str, payload: dict[str, Any] | None = None) -> Any:
    headers = {"JOB-TOKEN": token, "Accept": "application/json"}
    data = None
    if payload is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(payload).encode()
    status, body = http_request(method, url, headers, data)
    if status < 200 or status >= 300:
        raise RuntimeError(f"GitLab API returned HTTP {status}")
    return json.loads(body) if body else None


def publish(binary: str | Path, env: dict[str, str]) -> str:
    required = ("CI_API_V4_URL", "CI_PROJECT_ID", "CI_COMMIT_SHA", "CI_JOB_TOKEN")
    missing = [key for key in required if not env.get(key)]
    if missing:
        raise ValueError("missing CI environment: " + ", ".join(missing))
    api = env["CI_API_V4_URL"].rstrip("/")
    project = urllib.parse.quote(env["CI_PROJECT_ID"], safe="")
    base = f"{api}/projects/{project}"
    fingerprint = source_fingerprint(env["CI_COMMIT_SHA"], env.get("CI_PROJECT_DIR", "."))
    releases = _json_call("GET", f"{base}/releases?per_page=100", env["CI_JOB_TOKEN"])
    if not needs_release(fingerprint, releases):
        print(f"legacy release unchanged: {fingerprint}")
        return ""
    binary = Path(binary)
    if not binary.is_file():
        raise FileNotFoundError(binary)
    checksum_text = checksums(binary)
    date = datetime.now(timezone.utc).strftime("%Y%m%d")
    tag = f"{PREFIX}{date}-{env['CI_COMMIT_SHA'][:8]}"
    package = f"{api}/projects/{project}/packages/generic/zoder-legacy/{date}-{env['CI_COMMIT_SHA'][:8]}"
    for name, content_type, content in (
        ("zoder-x86_64-v2-linux", "application/octet-stream", binary.read_bytes()),
        ("SHA256SUMS", "text/plain", checksum_text.encode()),
    ):
        status, _ = http_request("PUT", f"{package}/{name}", {"JOB-TOKEN": env["CI_JOB_TOKEN"], "Content-Type": content_type}, content)
        if status < 200 or status >= 300:
            raise RuntimeError(f"GitLab package upload returned HTTP {status}")
    description = f"Legacy x86-64-v2 build.\n\nSHA-256: {checksum_text.split()[0]}\nSource fingerprint: {fingerprint}\n"
    links = [
        {"name": name, "url": f"{package}/{name}", "link_type": "package"}
        for name in ("zoder-x86_64-v2-linux", "SHA256SUMS")
    ]
    _json_call("POST", f"{base}/releases", env["CI_JOB_TOKEN"], {"name": tag, "tag_name": tag, "ref": env["CI_COMMIT_SHA"], "description": description, "assets": {"links": links}})
    return tag


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", help="built target/release/zoder binary")
    args = parser.parse_args()
    try:
        tag = publish(args.binary, dict(os.environ))
        print(tag or "no release needed")
        return 0
    except Exception as exc:
        print(f"legacy-release: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
