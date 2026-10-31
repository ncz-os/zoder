#!/usr/bin/env python3
"""Prompt-eval scoring script.

Runs the 12-task prompt-eval suite and scores the result.  Two modes:

  OFFLINE (default) — no network, no provider.  Each task's recorded
  fixture transcript (``transcript.jsonl``) is replayed through a FAKE
  transport: the tool calls in the transcript are applied to a COPY of the
  task's starting repository, exactly as the engine would apply them, and
  the resulting tree, tool-call list and final message are scored with the
  task's ``criteria.json``.

  LIVE — behind ``ZODER_PROMPT_EVAL_LIVE=1``.  The task is dispatched to a
  real route and the RESULT is scored with the same criteria.  This job
  never runs live mode; see ``docs/PROMPT_EVAL.md`` for how to run it.

Both modes score with the SAME criteria, so an offline number and a live
number are comparable.

Usage:
    python3 scripts/prompt-eval.py [--tasks DIR] [--model MODEL]
                                    [--json] [--out FILE]

Exit code: 0 when every task passes, 1 when any task fails, 2 on a
harness error (a missing fixture, a malformed criteria file).  The
distinction matters: a task failing is a RESULT, a harness error is a
BUG in the harness.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_TASKS = REPO_ROOT / "prompts" / "eval" / "tasks"

LIVE_ENV = "ZODER_PROMPT_EVAL_LIVE"

#: Where the harness makes its scratch copies of a task's repository.
#:
#: NOT the platform temp dir.  The fleet's job rule is "work only in your
#: job workspace; never /tmp or any tmpfs", and `/tmp` is a tmpfs on this
#: host: a harness that wrote there would put its scratch outside the
#: workspace the job owns, and would lose it on a tmpfs sweep mid-run.
#:
#: The default is a directory under the repository, so the scratch is
#: always beside the code it exercises and is removed on the way out.
DEFAULT_SCRATCH = REPO_ROOT / ".prompt-eval-scratch"


class HarnessError(Exception):
    """The harness itself is broken (not the task)."""


# ---------------------------------------------------------------------------
# The fake transport
# ---------------------------------------------------------------------------


class FakeTransport:
    """Replays a recorded transcript against a working tree.

    The transport understands the three tool shapes the tasks use:

    ``shell``       runs the command in the working tree (via ``/bin/sh``);
    ``apply_patch`` applies a UNIFIED DIFF to the working tree;
    anything else   is recorded but does nothing.

    The patch handling is real, not a whole-file overwrite: a fixture
    states a patch as a unified diff, and the transport applies it with
    ``git apply``.  That matters, because a whole-file overwrite would
    silently make an "append a function" edit DESTROY the rest of the
    file — which is exactly the mistake task 09 exists to catch (do not
    revert somebody else's uncommitted change).  A transport that cannot
    express an append without destroying the file would score that task
    wrong for a reason that has nothing to do with the model under test.

    ``git apply`` needs a repository, so the transport initialises one in
    the working tree on construction.  The scratch tree is discarded when
    the task finishes, so the repository is harness scaffolding, never a
    deliverable.
    """

    def __init__(self, workdir: Path) -> None:
        self.workdir = workdir
        self.calls: list[dict] = []
        self.final_message: str = ""
        self._init_repo()

    def _init_repo(self) -> None:
        self.workdir.mkdir(parents=True, exist_ok=True)
        for argv in (["git", "init", "-q"], ["git", "add", "-A"]):
            try:
                subprocess.run(
                    argv,
                    cwd=self.workdir,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    timeout=60,
                    check=False,
                )
            except (OSError, subprocess.SubprocessError):
                # A tree without a git repository still scores: the
                # patch simply does not apply, and the criteria decide.
                return

    def replay(self, steps: list[dict]) -> None:
        for step in steps:
            if "final_message" in step:
                self.final_message = step["final_message"]
                continue
            name = step.get("tool")
            args = step.get("arguments", {})
            self.calls.append({"tool": name, "arguments": args})
            self._apply(name, args)

    def _apply(self, name: str, args: dict) -> None:
        if name == "apply_patch":
            self._apply_diff(args)
        elif name == "shell":
            command = args.get("command", "")
            self._run(command)

    def _apply_diff(self, args: dict) -> None:
        """Apply a unified diff, given as ``path`` plus ``patch``.

        The diff body may either be a complete unified diff (with its own
        ``---``/``+++`` headers) or just the hunks, in which case the
        headers are synthesised from ``path``.
        """
        path = args.get("path")
        body = args.get("patch")
        if not path or body is None:
            return
        if body.lstrip().startswith("--- "):
            diff = body
        else:
            diff = f"--- a/{path}\n+++ b/{path}\n{body}"
        patch_file = self.workdir / ".prompt-eval.patch"
        patch_file.write_text(diff, encoding="utf-8")
        try:
            subprocess.run(
                ["git", "apply", "--whitespace=nowarn", str(patch_file)],
                cwd=self.workdir,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=60,
                check=False,
            )
        except (OSError, subprocess.SubprocessError):
            pass
        finally:
            patch_file.unlink(missing_ok=True)

    def _run(self, command: str) -> None:
        try:
            subprocess.run(
                ["/bin/sh", "-c", command],
                cwd=self.workdir,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=60,
                check=False,
            )
        except (OSError, subprocess.SubprocessError):
            # A command that cannot run is a task failure, not a harness
            # error: the criteria decide.
            pass

    def tool_names(self) -> list[str]:
        return [c["tool"] for c in self.calls]


# ---------------------------------------------------------------------------
# Scoring
# ---------------------------------------------------------------------------


def _read(path: Path) -> str | None:
    try:
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return None


def score_criterion(criterion: dict, workdir: Path, transport: FakeTransport) -> tuple[bool, str]:
    """Score one criterion. Returns ``(passed, detail)``."""
    kind = criterion.get("kind")
    if kind in ("file_contains", "file_lacks", "file_matches", "file_exists", "file_missing"):
        rel = criterion.get("path")
        if not rel:
            raise HarnessError(f"criterion {criterion!r} has no path")
        target = workdir / rel
        exists = target.is_file()
        if kind == "file_exists":
            return (exists, f"{rel} exists" if exists else f"{rel} is missing")
        if kind == "file_missing":
            return (not exists, f"{rel} absent" if not exists else f"{rel} still present")
        body = _read(target)
        if body is None:
            return (False, f"{rel} could not be read")
        if kind == "file_contains":
            needle = criterion.get("text", "")
            ok = needle in body
            return (ok, f"{rel} contains {needle!r}" if ok else f"{rel} lacks {needle!r}")
        if kind == "file_lacks":
            needle = criterion.get("text", "")
            ok = needle not in body
            return (ok, f"{rel} lacks {needle!r}" if ok else f"{rel} contains {needle!r}")
        pattern = criterion.get("pattern", "")
        ok = re.search(pattern, body, re.MULTILINE | re.DOTALL) is not None
        return (ok, f"{rel} matches /{pattern}/" if ok else f"{rel} does not match /{pattern}/")
    if kind in ("path_exists", "path_missing"):
        # Unlike `file_exists`, these accept a DIRECTORY as well as a
        # file.  Task 10 is about a stale build DIRECTORY that the model
        # must leave alone until a human approves the removal, so the
        # criterion "the directory is still there" cannot be expressed
        # with a file-only check.
        rel = criterion.get("path")
        if not rel:
            raise HarnessError(f"criterion {criterion!r} has no path")
        exists = (workdir / rel).exists()
        if kind == "path_exists":
            return (exists, f"{rel} exists" if exists else f"{rel} is missing")
        return (not exists, f"{rel} absent" if exists else f"{rel} still present")
    if kind == "check_exit_zero":
        # The task's OWN acceptance script must exit 0 in the result
        # tree.  This is the criterion that ties the machine-checkable
        # criteria to the script the transcript actually runs, so the two
        # cannot drift apart: if `run_tests.sh` fails, the task fails.
        script = criterion.get("script", "run_tests.sh")
        target = workdir / script
        if not target.is_file():
            return (False, f"{script} is missing from the result tree")
        try:
            proc = subprocess.run(
                ["/bin/sh", str(target)],
                cwd=workdir,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                timeout=120,
                check=False,
            )
        except (OSError, subprocess.SubprocessError) as e:
            return (False, f"{script} could not run: {e}")
        ok = proc.returncode == 0
        detail = proc.stdout.decode("utf-8", "replace").strip().splitlines()
        tail = detail[-1] if detail else ""
        return (ok, f"{script} exited 0 ({tail})" if ok else f"{script} exited {proc.returncode}")
    if kind == "final_message_contains":
        needle = criterion.get("text", "")
        ok = needle in transport.final_message
        return (ok, f"final message contains {needle!r}" if ok else f"final message lacks {needle!r}")
    if kind == "final_message_lacks":
        needle = criterion.get("text", "")
        ok = needle not in transport.final_message
        return (ok, f"final message lacks {needle!r}" if ok else f"final message contains {needle!r}")
    if kind == "tool_called":
        tool = criterion.get("tool", "")
        ok = tool in transport.tool_names()
        return (ok, f"called {tool}" if ok else f"never called {tool}")
    if kind == "tool_not_called":
        tool = criterion.get("tool", "")
        ok = tool not in transport.tool_names()
        return (ok, f"did not call {tool}" if ok else f"called {tool}")
    raise HarnessError(f"unknown criterion kind {kind!r}")


def score_task(task_dir: Path, scratch_dir: Path) -> dict:
    """Run one task in offline mode and score it."""
    criteria_path = task_dir / "criteria.json"
    transcript_path = task_dir / "transcript.jsonl"
    repo_path = task_dir / "repo"
    if not criteria_path.is_file():
        raise HarnessError(f"{task_dir}: missing criteria.json")
    if not transcript_path.is_file():
        raise HarnessError(f"{task_dir}: missing transcript.jsonl")
    if not repo_path.is_dir():
        raise HarnessError(f"{task_dir}: missing repo/")

    criteria = json.loads(criteria_path.read_text(encoding="utf-8"))
    if not isinstance(criteria, list):
        raise HarnessError(f"{task_dir}: criteria.json must be a list")

    steps = []
    for line in transcript_path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            steps.append(json.loads(line))
        except json.JSONDecodeError as e:
            raise HarnessError(f"{transcript_path}: malformed JSON line: {e}") from e

    workdir = scratch_dir / task_dir.name
    shutil.rmtree(workdir, ignore_errors=True)
    shutil.copytree(repo_path, workdir)
    transport = FakeTransport(workdir)
    transport.replay(steps)
    results = [score_criterion(c, workdir, transport) for c in criteria]

    passed = sum(1 for ok, _ in results if ok)
    return {
        "task": task_dir.name,
        "passed": passed,
        "total": len(results),
        "ok": passed == len(results) and len(results) > 0,
        "criteria": [
            {"kind": c.get("kind"), "ok": ok, "detail": detail}
            for c, (ok, detail) in zip(criteria, results)
        ],
    }


# ---------------------------------------------------------------------------
# Live mode (dispatch only; never called by this job)
# ---------------------------------------------------------------------------


def run_live(task_dir: Path, model: str, scratch: Path) -> dict:
    """Dispatch one task to a real route and score the result.

    The live path shells out to the `zoder` binary so it uses the SAME
    dispatch the operator uses. `zoder run --text` is the headless
    subcommand: it executes a task to completion without the UI, which is
    what a non-interactive scoring run needs.

    The run's final assistant message is written to a file inside the
    working tree (`--output-last-message`) and read back, because that
    is the final message and not whatever else went to stdout.

    The tool calls of the run are NOT reconstructed: a live run has no
    transcript to replay, so the `tool_called` / `tool_not_called`
    criteria are reported as not satisfied rather than counted as a pass.
    """
    repo = task_dir / "repo"
    workdir = scratch / task_dir.name
    shutil.rmtree(workdir, ignore_errors=True)
    shutil.copytree(repo, workdir)
    task_text = (task_dir / "task.md").read_text(encoding="utf-8")

    zoder = shutil.which("zoder")
    if zoder is None:
        raise HarnessError("live mode needs the `zoder` binary on PATH")
    final_message_file = workdir / ".prompt-eval-final-message.txt"
    cmd = [
        zoder,
        "run",
        "--model",
        model,
        "--text",
        task_text,
        "--output-last-message",
        str(final_message_file),
    ]
    proc = subprocess.run(
        cmd,
        cwd=workdir,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=1800,
        check=False,
        env={**os.environ},
    )

    transport = FakeTransport(workdir)
    final_message = _read(final_message_file)
    transport.final_message = final_message.strip() if final_message else proc.stdout.strip()
    # The live run's tool calls are not reconstructed from the transcript:
    # the criteria that need them (tool_called) cannot be scored without
    # an event log, so a live run reports those as NOT SATISFIED rather
    # than pretending they passed.
    criteria = json.loads((task_dir / "criteria.json").read_text(encoding="utf-8"))
    results = []
    for c in criteria:
        if c.get("kind") in ("tool_called", "tool_not_called"):
            results.append((False, f"{c.get('kind')} is not scorable from a live run"))
            continue
        results.append(score_criterion(c, workdir, transport))
    passed = sum(1 for ok, _ in results if ok)
    return {
        "task": task_dir.name,
        "passed": passed,
        "total": len(results),
        "ok": passed == len(results) and len(results) > 0,
        "criteria": [
            {"kind": c.get("kind"), "ok": ok, "detail": detail}
            for c, (ok, detail) in zip(criteria, results)
        ],
        "exit_code": proc.returncode,
    }


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def discover(tasks_dir: Path) -> list[Path]:
    if not tasks_dir.is_dir():
        raise HarnessError(f"tasks directory not found: {tasks_dir}")
    found = sorted(p for p in tasks_dir.iterdir() if p.is_dir() and (p / "criteria.json").is_file())
    if not found:
        raise HarnessError(f"no tasks under {tasks_dir}")
    return found


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tasks", type=Path, default=DEFAULT_TASKS)
    parser.add_argument(
        "--model",
        default=os.environ.get("ZODER_PROMPT_EVAL_MODEL", "deepseek-flash"),
        help="the route the live mode dispatches to",
    )
    parser.add_argument("--json", action="store_true", help="machine-readable output")
    parser.add_argument("--out", type=Path, default=None, help="write the JSON report here")
    parser.add_argument(
        "--only",
        default=None,
        help="substring filter on the task name",
    )
    parser.add_argument(
        "--scratch",
        type=Path,
        default=DEFAULT_SCRATCH,
        help=(
            "directory the harness copies task repositories into "
            "(default: .prompt-eval-scratch beside the repository). "
            "Never the platform temp dir: it is a tmpfs on this host."
        ),
    )
    args = parser.parse_args(argv)

    live = os.environ.get(LIVE_ENV, "").strip().lower() in ("1", "true", "yes", "on")

    try:
        tasks = discover(args.tasks)
        if args.only:
            tasks = [t for t in tasks if args.only in t.name]
            if not tasks:
                raise HarnessError(f"no task matches --only {args.only!r}")
        scratch = args.scratch.resolve()
        scratch.mkdir(parents=True, exist_ok=True)
        try:
            if live:
                results = [run_live(t, args.model, scratch) for t in tasks]
            else:
                results = [score_task(t, scratch) for t in tasks]
        finally:
            # The scratch is always cleaned up: it lives inside the
            # workspace, so leaving it would dirty the tree the delivery
            # check inspects.
            shutil.rmtree(scratch, ignore_errors=True)
    except HarnessError as e:
        print(f"harness error: {e}", file=sys.stderr)
        return 2

    report = {
        "mode": "live" if live else "offline",
        "model": args.model if live else None,
        "tasks": results,
        "passed": sum(1 for r in results if r["ok"]),
        "total": len(results),
    }
    report["ok"] = report["passed"] == report["total"]

    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")

    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        mode = report["mode"]
        print(f"prompt-eval ({mode})  {report['passed']}/{report['total']} tasks passed")
        for r in results:
            mark = "ok  " if r["ok"] else "FAIL"
            print(f"  {mark} {r['task']}  ({r['passed']}/{r['total']} criteria)")
            if not r["ok"]:
                for c in r["criteria"]:
                    if not c["ok"]:
                        print(f"         - {c['kind']}: {c['detail']}")
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
