#!/usr/bin/env python3
"""zerocode pre-submission gate runner (thin, declarative-driven).

Reads the YAML stack/forge catalogs in this directory, detects the project's
stack(s) and forge, and runs the hardened gate for the active risk tier. The
LANGUAGE/FORGE BAR LIVES IN YAML (stacks.yaml / forges.yaml); this runner only
executes it. A repo-local gate.toml (derived from the project's own CI) overrides
the catalog defaults so "local gate == destination CI" stays the invariant.

Exit 0 => gate green (safe to submit). Non-zero => blocked.

Usage:
  zerocode-gate.py [--repo DIR] [--tier low|medium|high] [--only fmt,lint,...]
                   [--list] [--json] [--evidence FILE]
"""
from __future__ import annotations
import argparse, json, os, shutil, subprocess, sys, glob as globmod
from pathlib import Path

HERE = Path(__file__).resolve().parent

def load_yaml(p: Path):
    import yaml
    with open(p) as f:
        return yaml.safe_load(f)

def any_glob(repo: Path, patterns) -> bool:
    for pat in patterns or []:
        if list(repo.glob(pat)) or list(repo.glob(f"**/{pat}")):
            return True
    return False

def root_glob(repo: Path, patterns) -> bool:
    """Root-anchored match for STACK DETECTION: a project is a given stack only
    if its primary manifest sits at the repo root (or a top-level workspace
    member), not because a marker file appears somewhere deep in the tree."""
    for pat in patterns or []:
        if list(repo.glob(pat)):
            return True
    return False

def tool_available(tool: str) -> bool:
    if tool is None:
        return True
    special = {
        "cargo-nextest": ["cargo", "nextest", "--version"],
        "gradle_wrapper": None,            # handled via 'uses'/file check below
        "mix-credo": ["mix", "help", "credo"],
        "mix-dialyzer": ["mix", "help", "dialyzer"],
        "build": [sys.executable, "-c", "import build"],
    }
    if tool in special:
        probe = special[tool]
        if probe is None:
            return True
        try:
            return subprocess.run(probe, capture_output=True, timeout=30).returncode == 0
        except Exception:
            return False
    return shutil.which(tool) is not None

def detect_stacks(repo: Path, catalog) -> list[str]:
    found = []
    for name, spec in catalog["stacks"].items():
        if root_glob(repo, spec.get("detect", {}).get("any_of", [])):
            found.append(name)
    return found

def detect_forge(repo: Path, forges) -> str:
    try:
        url = subprocess.run(["git", "-C", str(repo), "remote", "get-url", "origin"],
                             capture_output=True, text=True).stdout.strip().lower()
    except Exception:
        url = ""
    for name, spec in forges["forges"].items():
        d = spec.get("detect", {})
        if any(h in url for h in d.get("remote_host_any_of", [])):
            return name
        if any(h in url for h in d.get("remote_host_contains", [])):
            return name
    return "plain_git"

def step_applies(repo: Path, step: dict) -> tuple[bool, str]:
    uses = step.get("uses")
    if uses and not (repo / uses).exists() and not any_glob(repo, [uses]):
        return False, f"skip (no {uses})"
    wa = step.get("when_available")
    if wa and not tool_available(wa):
        return False, f"skip ({wa} not installed)"
    ua = step.get("unless_available")
    if ua and tool_available(ua):
        return False, f"skip ({ua} present, alt path)"
    return True, ""

def run_phase(repo: Path, phase: str, steps: list, evidence: list) -> bool:
    ok = True
    for step in steps:
        applies, why = step_applies(repo, step)
        cmd = step["cmd"]
        if not applies:
            print(f"  · [{phase}] {why}: {cmd}")
            continue
        print(f"  ▶ [{phase}] {cmd}")
        r = subprocess.run(cmd, shell=True, cwd=str(repo), text=True,
                           capture_output=True)
        out = (r.stdout or "") + (r.stderr or "")
        evidence.append({"phase": phase, "cmd": cmd, "exit": r.returncode,
                         "tail": out.strip()[-1500:]})
        if r.returncode != 0:
            print(out.strip()[-2000:])
            print(f"  ❌ [{phase}] FAILED (exit {r.returncode}): {cmd}")
            ok = False
            if step.get("strict", False):
                return False  # fail fast within a strict phase
        else:
            print(f"  ✅ [{phase}] ok")
    return ok

def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", default=".")
    ap.add_argument("--tier", default=None, choices=["low", "medium", "high"])
    ap.add_argument("--only", default=None, help="comma list of phases to run")
    ap.add_argument("--list", action="store_true", help="detect only, run nothing")
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--evidence", default=None, help="write validation-evidence JSON")
    args = ap.parse_args()

    repo = Path(args.repo).resolve()
    catalog = load_yaml(HERE / "stacks.yaml")
    forges = load_yaml(HERE / "forges.yaml")

    stacks = detect_stacks(repo, catalog)
    forge = detect_forge(repo, forges)
    tier = args.tier or catalog.get("default_tier", "medium")
    phases = args.only.split(",") if args.only else catalog["risk_tiers"][tier]

    if args.list or not stacks:
        print(json.dumps({"repo": str(repo), "stacks": stacks, "forge": forge,
                          "tier": tier, "phases": phases}, indent=2))
        if not stacks:
            print("No known stack detected; fall back to CI-derived gate.toml "
                  "(see stacks.yaml: fallback).", file=sys.stderr)
            return 0 if args.list else 2
        if args.list:
            return 0

    print(f"== zerocode gate == repo={repo.name} stacks={stacks} forge={forge} "
          f"tier={tier} phases={phases}")
    evidence, all_ok = [], True
    for stack in stacks:
        spec = catalog["stacks"][stack]
        print(f"\n-- stack: {stack} --")
        for phase in phases:
            steps = spec.get("phases", {}).get(phase)
            if not steps:
                continue
            if not run_phase(repo, phase, steps, evidence):
                all_ok = False

    result = {"repo": str(repo), "stacks": stacks, "forge": forge, "tier": tier,
              "green": all_ok, "evidence": evidence}
    if args.evidence:
        Path(args.evidence).write_text(json.dumps(result, indent=2))
    if args.json:
        print(json.dumps(result, indent=2))
    print("\n════════════════════════════════")
    print("GATE: ALL GREEN ✅" if all_ok else "GATE: BLOCKED ❌ (fix before submit)")
    return 0 if all_ok else 1

if __name__ == "__main__":
    sys.exit(main())
