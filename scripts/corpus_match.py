#!/usr/bin/env python3
"""Canonical model-key normalization shared by the corpus builder and the bench
overlay combiner.

The pricing corpus, the SWE leaderboards, and arena.ai each name the same model
differently:

  corpus id        anthropic.claude-opus-4-8 | azure_ai/claude-opus-4-8 |
                   bedrock/us-east-1/zai.glm-5 | baseten/moonshotai/Kimi-K2-Thinking
  vals.ai          anthropic/claude-opus-4-8 | anthropic/claude-opus-4-8-claude-code
  aider            gpt-5 (high) | o3-pro (high)
  arena webdev     glm-5.2 (max) | claude-opus-4-8-thinking
  arena agent      Claude Fable 5 (High)
  scale seal       Fable-5 (Claude Code) xHigh

`canon_key` reduces all of them to one comparable token (e.g. `claude-opus-4-8`,
`glm-5-2`, `kimi-k2`) by: taking the leaf after any provider/region path, dropping
a leading `vendor.` namespace, removing parentheticals + reasoning-effort/variant/
snapshot decorators, and normalizing version dots to dashes. Distinct base models
stay distinct (`gpt-5` vs `gpt-5-5`); harness/effort variants of one model merge
(`...-claude-code`, `(high)`, `-thinking`)."""
from __future__ import annotations
import re

# Pure-namespace vendor tokens that prefix an id as "vendor." (e.g. zai.glm-5).
# Only stripped when they appear as a dotted prefix, never mid-name.
_VENDORS = {
    "anthropic", "openai", "google", "meta", "mistral", "mistralai", "zai",
    "qwen", "alibaba", "minimax", "moonshot", "moonshotai", "nvidia", "deepseek",
    "xai", "cohere", "ai21", "amazon", "microsoft", "databricks", "perplexity",
    "reka", "nous", "together", "fireworks", "groq", "inflection", "stepfun",
}

# Multi-word decorators removed wholesale before tokenization.
_MULTI = ("claude code", "claude-code", "with reasoning", "w/ thinking")

# Trailing decorator tokens (reasoning effort, variant, quantization, snapshot)
# that are not part of model identity; stripped from the END only.
_TRAIL = {
    "thinking", "think", "high", "medium", "low", "xhigh", "minimal", "codex",
    "max", "mini", "nitro", "preview", "instruct", "latest", "reasoner", "fast",
    "turbo", "exp", "beta", "it", "chat", "base", "online", "pro", "fp8", "bf16",
    "32k", "16k", "8k", "128k", "1m",
}


def canon_key(name: str) -> str:
    if not name:
        return ""
    s = name.strip().lower()
    s = s.split("/")[-1]  # drop provider/region path (azure_ai/, bedrock/us-east-1/, …)
    if "." in s:
        head = s.split(".", 1)[0]
        if head in _VENDORS:  # drop a "vendor." namespace, keep version dots
            s = s.split(".", 1)[1]
    for m in _MULTI:
        s = s.replace(m, " ")
    s = re.sub(r"\(.*?\)", " ", s)        # drop parentheticals e.g. (high)
    s = s.replace(".", "-")               # version dots -> dash (5.2 -> 5-2)
    s = re.sub(r"[^a-z0-9]+", "-", s)     # any other separator -> dash
    s = re.sub(r"-+", "-", s).strip("-")
    toks = s.split("-")
    # strip trailing decorators + snapshot dates (YYYYMMDD / YYYY / MMDD)
    changed = True
    while toks and changed:
        changed = False
        last = toks[-1]
        if last in _TRAIL or re.fullmatch(r"\d{4}|\d{6}|\d{8}", last):
            toks.pop()
            changed = True
    return "-".join(toks)


if __name__ == "__main__":  # quick self-check
    cases = {
        "anthropic.claude-opus-4-8": "claude-opus-4-8",
        "azure_ai/claude-opus-4-8": "claude-opus-4-8",
        "anthropic/claude-opus-4-8-claude-code": "claude-opus-4-8",
        "claude-opus-4-8-thinking": "claude-opus-4-8",
        "Claude Fable 5 (High)": "claude-fable-5",
        "Fable-5 (Claude Code) xHigh": "fable-5",
        "bedrock/us-east-1/zai.glm-5": "glm-5",
        "cloudflare/@cf/zai-org/glm-5.2": "glm-5-2",
        "glm-5.2 (max)": "glm-5-2",
        "baseten/moonshotai/Kimi-K2-Instruct-0905": "kimi-k2",
        "qwen.qwen3-coder-next": "qwen3-coder-next",
        "gpt-5 (high)": "gpt-5",
        "gpt-5.5": "gpt-5-5",
        "deepseek-v3-0324": "deepseek-v3",
    }
    bad = {k: (canon_key(k), v) for k, v in cases.items() if canon_key(k) != v}
    if bad:
        print("MISMATCH:", bad)
        raise SystemExit(1)
    print("canon_key self-check OK")
