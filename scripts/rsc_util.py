"""Shared helpers for ingesting Next.js RSC-streamed leaderboards.

Several coding leaderboards (Terminal-Bench, arena.ai) are Next.js apps that
stream their data as React Server Component chunks: a series of
`self.__next_f.push([N, "<json-escaped chunk>"])` calls whose concatenated,
unescaped payload contains the leaderboard JSON. These helpers reconstruct that
payload with a single HTTP GET (no headless browser) and pull a named array out
of it.
"""
from __future__ import annotations
import json, re, urllib.request

_PUSH = re.compile(r'self\.__next_f\.push\(\[\d+,"((?:[^"\\]|\\.)*)"\]\)', re.S)


def fetch(url: str, timeout: int = 40) -> str:
    req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0 (zoder-ingest)"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.read().decode("utf-8", "replace")


def rsc_text(html: str) -> str:
    """Concatenate and unescape every RSC chunk into one decoded string."""
    return "".join(json.loads('"' + p + '"') for p in _PUSH.findall(html))


def extract_array(text: str, key: str):
    """Return the JSON array assigned to `"<key>":[ ... ]` via bracket matching
    (string-aware), or None. Works on the decoded RSC text."""
    i = text.find(f'"{key}":[')
    if i < 0:
        return None
    start = text.index("[", i)
    depth = 0
    instr = False
    esc = False
    for j in range(start, len(text)):
        c = text[j]
        if esc:
            esc = False
            continue
        if c == "\\":
            esc = True
            continue
        if c == '"':
            instr = not instr
            continue
        if instr:
            continue
        if c == "[":
            depth += 1
        elif c == "]":
            depth -= 1
            if depth == 0:
                try:
                    return json.loads(text[start:j + 1])
                except json.JSONDecodeError:
                    return None
    return None
