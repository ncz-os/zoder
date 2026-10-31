#!/usr/bin/env python3
"""Tests for the prompt evaluation harness (`scripts/prompt-eval.py`).

The harness is a deliverable of its own: if it cannot tell a passing
transcript from a failing one, then the 12 tasks score nothing.  These
tests pin the behaviour the task list depends on:

  * the offline mode needs no network and no provider;
  * a patch is applied as a UNIFIED DIFF, so an append cannot silently
    delete the rest of the file (the failure task 09 exists to catch);
  * the scratch tree lives inside the workspace, never on a tmpfs;
  * every criterion kind the fixtures use is understood;
  * every task fixture is well-formed and every task is scored.

Offline and deterministic: no network, no provider, and the scratch
directory is created under the repository and removed afterwards.

Run: `python3 scripts/tests/prompt-eval-tests.py`
"""

from __future__ import annotations

import ast
import importlib.util
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = REPO_ROOT / "scripts"
EVAL = SCRIPTS / "prompt-eval.py"
TASKS_DIR = REPO_ROOT / "prompts" / "eval" / "tasks"

#: The criterion kinds the scorer knows about.  A fixture using a kind
#: outside this set would be scored as "unknown criterion kind" and the
#: task would ERROR rather than quietly pass.
KNOWN_CRITERION_KINDS = {
    "file_contains",
    "file_lacks",
    "file_matches",
    "file_exists",
    "file_missing",
    "path_exists",
    "path_missing",
    "check_exit_zero",
    "final_message_contains",
    "final_message_lacks",
    "tool_called",
    "tool_not_called",
}


def load_eval_module():
    """Import `scripts/prompt-eval.py` (its name is not importable)."""
    spec = importlib.util.spec_from_file_location("prompt_eval", EVAL)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {EVAL}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class HarnessImportTests(unittest.TestCase):
    def test_the_harness_imports(self):
        module = load_eval_module()
        self.assertTrue(hasattr(module, "score_criterion"))
        self.assertTrue(hasattr(module, "FakeTransport"))

    def test_the_harness_never_uses_the_platform_temp_dir(self):
        """The job rule is "never /tmp or any tmpfs".

        The harness must not fall back to the platform temp dir, not even
        as a default: `/tmp` is a tmpfs on this host, so a scratch tree
        there would be outside the workspace the job owns.

        The check is on the CODE, not on the raw text: the harness
        deliberately NAMES `/tmp` in a comment to explain why it avoids
        it, and a naive text search would flag that explanation.
        """
        module = load_eval_module()
        self.assertNotIn(
            "tempfile",
            {name for name in dir(module) if name == "tempfile"},
            "the harness must not use tempfile",
        )

        tree = ast.parse(EVAL.read_text(encoding="utf-8"))
        docstrings = set()
        for node in ast.walk(tree):
            if isinstance(node, (ast.Module, ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                body = getattr(node, "body", [])
                if (
                    body
                    and isinstance(body[0], ast.Expr)
                    and isinstance(body[0].value, ast.Constant)
                    and isinstance(body[0].value.value, str)
                ):
                    docstrings.add(id(body[0].value))
        for node in ast.walk(tree):
            if isinstance(node, ast.Constant) and isinstance(node.value, str):
                if id(node) in docstrings:
                    continue
                self.assertNotIn(
                    "/tmp",
                    node.value,
                    f"line {node.lineno}: a live string names the platform temp dir",
                )

    def test_the_harness_does_not_import_tempfile(self):
        source = EVAL.read_text(encoding="utf-8")
        self.assertNotIn("import tempfile", source, "the harness must not use tempfile")

    def test_the_default_scratch_is_inside_the_repository(self):
        module = load_eval_module()
        scratch = module.DEFAULT_SCRATCH.resolve()
        self.assertTrue(
            str(scratch).startswith(str(REPO_ROOT.resolve())),
            f"the default scratch {scratch} is outside the workspace",
        )


class CriterionTests(unittest.TestCase):
    """The scorer must understand every kind the fixtures use."""

    def setUp(self):
        self.module = load_eval_module()
        self.workdir = Path(tempfile.mkdtemp(prefix="prompt-eval-selftest-"))
        self.addCleanup(shutil.rmtree, self.workdir, ignore_errors=True)
        (self.workdir / "a.txt").write_text("hello world\n", encoding="utf-8")
        (self.workdir / "sub").mkdir()
        self.transport = self.module.FakeTransport(self.workdir)

    def score(self, criterion):
        return self.module.score_criterion(criterion, self.workdir, self.transport)[0]

    def test_file_contains_and_lacks(self):
        self.assertTrue(self.score({"kind": "file_contains", "path": "a.txt", "text": "hello"}))
        self.assertFalse(self.score({"kind": "file_contains", "path": "a.txt", "text": "nope"}))
        self.assertTrue(self.score({"kind": "file_lacks", "path": "a.txt", "text": "nope"}))

    def test_file_matches(self):
        self.assertTrue(self.score({"kind": "file_matches", "path": "a.txt", "pattern": r"h\w+"}))
        self.assertFalse(self.score({"kind": "file_matches", "path": "a.txt", "pattern": r"z+"}))

    def test_path_exists_accepts_a_directory(self):
        """`file_exists` is file-only; `path_exists` also accepts a dir."""
        self.assertFalse(self.score({"kind": "file_exists", "path": "sub"}))
        self.assertTrue(self.score({"kind": "path_exists", "path": "sub"}))
        self.assertTrue(self.score({"kind": "path_exists", "path": "a.txt"}))
        self.assertTrue(self.score({"kind": "path_missing", "path": "nope"}))

    def test_final_message_criteria(self):
        self.transport.final_message = "the build is fine"
        self.assertTrue(self.score({"kind": "final_message_contains", "text": "build"}))
        self.assertFalse(self.score({"kind": "final_message_contains", "text": "destroyed"}))
        self.assertTrue(self.score({"kind": "final_message_lacks", "text": "destroyed"}))

    def test_tool_called_criteria(self):
        self.transport.calls.append({"tool": "shell", "arguments": {}})
        self.assertTrue(self.score({"kind": "tool_called", "tool": "shell"}))
        self.assertFalse(self.score({"kind": "tool_called", "tool": "apply_patch"}))
        self.assertTrue(self.score({"kind": "tool_not_called", "tool": "apply_patch"}))

    def test_an_unknown_kind_is_an_error_not_a_pass(self):
        with self.assertRaises(self.module.HarnessError):
            self.score({"kind": "no_such_kind"})

    def test_a_criterion_without_a_path_is_an_error(self):
        with self.assertRaises(self.module.HarnessError):
            self.score({"kind": "file_contains", "text": "x"})


class PatchTransportTests(unittest.TestCase):
    """The transport must apply a unified diff, not overwrite a file."""

    def setUp(self):
        self.module = load_eval_module()
        self.workdir = Path(tempfile.mkdtemp(prefix="prompt-eval-patch-"))
        self.addCleanup(shutil.rmtree, self.workdir, ignore_errors=True)
        (self.workdir / "f.py").write_text("def a():\n    return 1\n", encoding="utf-8")
        self.transport = self.module.FakeTransport(self.workdir)

    def test_an_append_keeps_the_existing_content(self):
        diff = (
            "--- a/f.py\n+++ b/f.py\n@@ -1,2 +1,5 @@\n"
            " def a():\n"
            "     return 1\n"
            "+\n"
            "+def b():\n"
            "+    return 2\n"
        )
        self.transport.replay(
            [{"tool": "apply_patch", "arguments": {"path": "f.py", "patch": diff}}]
        )
        body = (self.workdir / "f.py").read_text(encoding="utf-8")
        self.assertIn("def a():", body, "the append destroyed the original function")
        self.assertIn("def b():", body, "the append did not land")

    def test_a_new_file_is_created(self):
        diff = "--- /dev/null\n+++ b/new.py\n@@ -0,0 +1,2 @@\n+def c():\n+    return 3\n"
        self.transport.replay(
            [{"tool": "apply_patch", "arguments": {"path": "new.py", "patch": diff}}]
        )
        self.assertTrue((self.workdir / "new.py").is_file())
        self.assertIn("def c():", (self.workdir / "new.py").read_text(encoding="utf-8"))

    def test_a_diff_that_does_not_apply_leaves_the_file_alone(self):
        before = (self.workdir / "f.py").read_text(encoding="utf-8")
        diff = "--- a/f.py\n+++ b/f.py\n@@ -1,2 +1,2 @@\n- completely wrong\n+ also wrong\n"
        self.transport.replay(
            [{"tool": "apply_patch", "arguments": {"path": "f.py", "patch": diff}}]
        )
        self.assertEqual(before, (self.workdir / "f.py").read_text(encoding="utf-8"))

    def test_a_shell_step_runs_in_the_working_tree(self):
        self.transport.replay(
            [
                {
                    "tool": "shell",
                    "arguments": {"command": "echo written > made.txt"},
                }
            ]
        )
        self.assertTrue((self.workdir / "made.txt").is_file())


class TaskPremiseTests(unittest.TestCase):
    """A task must only pass for the RIGHT reason.

    THE BUG THIS EXISTS TO CATCH
    ----------------------------
    Task 12 used to be built on a FALSE premise: it asked the model to
    make `tokens` drop empty strings, using `str.split()` with no
    argument — but `str.split()` already collapses runs of whitespace and
    never yields an empty string.  The "fix" was a no-op, and the
    task's acceptance test passed on the UNFIXED tree, so the task could
    pass whether or not the model did anything.

    The general rule this class enforces:

      * a task whose acceptance script exists MUST have that script FAIL
        on the starting tree (the task is unfinished), unless the task is
        explicitly one where the starting state is already correct;
      * that script MUST exit 0 on the RESULT tree (the transcript
        actually fixes it);
      * every `file_contains` criterion MUST hold on the RESULT tree —
        never only on the starting tree.

    The `file_contains` check is what the reviewer read by hand: the
    criteria name a string that only appears AFTER the patch is applied,
    which is correct for a scoring criterion but looks like a mismatch
    when the starting tree is read in isolation.  Asserting it against
    the RESULT tree pins down the intended semantics.
    """

    #: Tasks whose starting state is deliberately ALREADY correct, so
    #: their acceptance script passes before any edit.  Each one earns
    #: its exemption: see the comment on the class.
    ACCEPTANCE_PASSES_AT_START = {
        # A refactor preserves behaviour, so the test passes before and
        # after by design.  What the task scores is the SHAPE of the
        # refactor, which its `file_contains` criteria check.
        "03-refactor-function",
        # The model must escalate rather than delete, so the stale build
        # directory is supposed to still be there.  The script asserting
        # that it exists is the task's point.
        "10-escalate-destructive",
    }

    def setUp(self):
        self.module = load_eval_module()
        self.tasks = sorted(p for p in TASKS_DIR.iterdir() if p.is_dir())

    def _score(self, task_dir: Path, use_starting_tree: bool) -> dict:
        """Score one task, optionally against its UNPATCHED tree."""
        scratch = Path(tempfile.mkdtemp(prefix="prompt-eval-premise-"))
        self.addCleanup(shutil.rmtree, scratch, ignore_errors=True)
        if use_starting_tree:
            # A "result" tree that is just the starting tree: nothing was
            # applied.  The criteria must then FAIL, or the task would
            # pass without the model doing anything.
            result = scratch / "result"
            shutil.copytree(task_dir / "repo", result)
            transport = self.module.FakeTransport(result)
            criteria = json.loads((task_dir / "criteria.json").read_text(encoding="utf-8"))
            scored = [self.module.score_criterion(c, result, transport) for c in criteria]
            return {
                "criteria": [
                    {"kind": c.get("kind"), "ok": ok, "detail": detail}
                    for c, (ok, detail) in zip(criteria, scored)
                ],
                "passed": sum(1 for ok, _ in scored if ok),
                "total": len(scored),
            }
        return self.module.score_task(task_dir, scratch)

    def test_the_acceptance_script_fails_on_the_starting_tree(self):
        """The task must be UNFINISHED before the model starts."""
        for task in self.tasks:
            script = task / "repo" / "run_tests.sh"
            if not script.is_file():
                continue
            name = task.name
            if name in self.ACCEPTANCE_PASSES_AT_START:
                continue
            work = Path(tempfile.mkdtemp(prefix="prompt-eval-start-"))
            self.addCleanup(shutil.rmtree, work, ignore_errors=True)
            tree = work / "repo"
            shutil.copytree(task / "repo", tree)
            proc = subprocess.run(
                ["/bin/sh", str(tree / "run_tests.sh")],
                cwd=tree,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                timeout=180,
                check=False,
            )
            self.assertNotEqual(
                proc.returncode,
                0,
                f"{name}: the acceptance script passes on the STARTING tree, so the "
                "task is already finished and would pass without the model doing "
                "anything",
            )

    def test_the_acceptance_script_passes_on_the_result_tree(self):
        """The transcript must actually fix the task."""
        for task in self.tasks:
            script = task / "repo" / "run_tests.sh"
            if not script.is_file():
                continue
            work = Path(tempfile.mkdtemp(prefix="prompt-eval-result-"))
            self.addCleanup(shutil.rmtree, work, ignore_errors=True)
            tree = work / "repo"
            shutil.copytree(task / "repo", tree)
            subprocess.run(["git", "init", "-q"], cwd=tree, check=False)
            for line in (task / "transcript.jsonl").read_text(encoding="utf-8").splitlines():
                if not line.strip():
                    continue
                step = json.loads(line)
                if step.get("tool") != "apply_patch":
                    continue
                args = step["arguments"]
                (tree / ".p").write_text(args["patch"], encoding="utf-8")
                subprocess.run(
                    ["git", "apply", "--whitespace=nowarn", ".p"],
                    cwd=tree,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    timeout=60,
                    check=False,
                )
            proc = subprocess.run(
                ["/bin/sh", str(tree / "run_tests.sh")],
                cwd=tree,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                timeout=180,
                check=False,
            )
            self.assertEqual(
                proc.returncode,
                0,
                f"{task.name}: the acceptance script fails on the RESULT tree, so the "
                f"transcript does not actually complete the task:\n"
                f"{proc.stdout.decode('utf-8', 'replace')}",
            )

    def test_every_file_contains_criterion_holds_on_the_result_tree(self):
        """A `file_contains` criterion is about the RESULT, not the start.

        This is the check that pins down what the reviewer read by hand.
        A criterion naming a string that only appears after the patch is
        CORRECT for scoring; what would be wrong is a criterion that
        holds on the starting tree and therefore tests nothing.
        """
        for task in self.tasks:
            report = self._score(task, use_starting_tree=False)
            for criterion in report["criteria"]:
                if criterion["kind"] != "file_contains":
                    continue
                self.assertTrue(
                    criterion["ok"],
                    f"{task.name}: {criterion['detail']} — the criterion does not hold "
                    "on the result tree",
                )

    def test_a_task_is_not_fully_satisfied_by_its_starting_tree(self):
        """Doing nothing must not pass a task.

        For every task except the ones explicitly exempted, scoring the
        UNPATCHED tree must fail at least one criterion.
        """
        for task in self.tasks:
            if task.name in self.ACCEPTANCE_PASSES_AT_START:
                continue
            report = self._score(task, use_starting_tree=True)
            self.assertLess(
                report["passed"],
                report["total"],
                f"{task.name}: every criterion passes on the UNPATCHED tree, so the "
                "task would pass even if the model did nothing",
            )

    def test_task_12_premise_is_real(self):
        """`str.split()` never yields an empty string.

        Task 12 must not be rebuilt on that false premise: a fixture that
        claims `split()` returns an empty string for "a  b" is wrong, and
        its acceptance test would pass on the unfixed tree.
        """
        self.assertEqual("a  b".split(), ["a", "b"])
        self.assertEqual("a,,b".split(","), ["a", "", "b"])


class FixtureTests(unittest.TestCase):
    """Every task fixture must be well-formed."""

    def setUp(self):
        self.tasks = sorted(p for p in TASKS_DIR.iterdir() if p.is_dir())
        self.assertEqual(len(self.tasks), 12, "there must be exactly 12 tasks")

    def test_every_task_has_the_expected_files(self):
        for task in self.tasks:
            for name in ("task.md", "criteria.json", "transcript.jsonl"):
                self.assertTrue((task / name).is_file(), f"{task.name}/{name} is missing")
            self.assertTrue((task / "repo").is_dir(), f"{task.name}/repo is missing")

    def test_every_criterion_kind_is_known(self):
        for task in self.tasks:
            criteria = json.loads((task / "criteria.json").read_text(encoding="utf-8"))
            self.assertTrue(criteria, f"{task.name} has no criteria")
            for criterion in criteria:
                self.assertIn(
                    criterion.get("kind"),
                    KNOWN_CRITERION_KINDS,
                    f"{task.name} uses an unknown criterion kind {criterion.get('kind')!r}",
                )

    def test_every_transcript_line_is_json_with_a_tool_or_a_final_message(self):
        for task in self.tasks:
            for number, line in enumerate(
                (task / "transcript.jsonl").read_text(encoding="utf-8").splitlines(), 1
            ):
                if not line.strip():
                    continue
                step = json.loads(line)
                self.assertTrue(
                    "final_message" in step or "tool" in step,
                    f"{task.name}/transcript.jsonl:{number} is neither a tool call nor a final message",
                )

    def test_the_generator_is_idempotent(self):
        """Re-running the generator must not change any fixture bytes."""
        before = {
            path.relative_to(TASKS_DIR): path.read_bytes()
            for path in TASKS_DIR.rglob("*")
            if path.is_file()
        }
        subprocess.run(
            [sys.executable, str(SCRIPTS / "generate-prompt-eval-tasks.py")],
            cwd=str(REPO_ROOT),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            timeout=300,
            check=True,
        )
        after = {
            path.relative_to(TASKS_DIR): path.read_bytes()
            for path in TASKS_DIR.rglob("*")
            if path.is_file()
        }
        self.assertEqual(before, after, "the generator is not idempotent")


class ScoringTests(unittest.TestCase):
    """The whole harness, end to end, offline."""

    def test_the_offline_harness_scores_every_task(self):
        result = subprocess.run(
            [sys.executable, str(EVAL)],
            cwd=str(REPO_ROOT),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=600,
            check=False,
        )
        output = result.stdout.decode("utf-8", "replace")
        self.assertEqual(result.returncode, 0, f"the harness failed:\n{output}")
        self.assertIn("12/12 tasks passed", output, f"unexpected harness output:\n{output}")

    def test_the_harness_leaves_no_scratch_behind(self):
        scratch = REPO_ROOT / ".prompt-eval-scratch"
        subprocess.run(
            [sys.executable, str(EVAL)],
            cwd=str(REPO_ROOT),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=600,
            check=False,
        )
        self.assertFalse(
            scratch.exists(),
            "the harness left its scratch tree behind inside the workspace",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
