from __future__ import annotations

import os
import shutil
import tempfile
import textwrap
import unittest
from pathlib import Path

from tools.decompile_gate import (
    DEFAULT_DECOMPILER,
    DEFAULT_RECOMPILER,
    GateResult,
    _expand_captures,
    _first_nonblank_line,
    run_gate_one,
)

WORKSPACE = Path(__file__).resolve().parents[2]


def _write_fake_binary(directory: Path, name: str, body: str) -> Path:
    path = directory / name
    path.write_text(textwrap.dedent(body), encoding="utf-8")
    return path


class FirstNonblankLineTests(unittest.TestCase):
    def test_returns_first_line_with_content(self) -> None:
        self.assertEqual(
            _first_nonblank_line("\n\n  hello world  \nmore\n"), "hello world"
        )

    def test_returns_none_for_blank_text(self) -> None:
        self.assertIsNone(_first_nonblank_line("\n\n   \n"))

    def test_returns_none_for_empty_text(self) -> None:
        self.assertIsNone(_first_nonblank_line(""))


class RunGateOneTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = Path(tempfile.mkdtemp(prefix="decompile_gate_test_"))
        self.addCleanup(shutil.rmtree, self.tempdir, ignore_errors=True)
        self.capture = self.tempdir / "input.luac"
        self.capture.write_bytes(b"\x06fakebytecode")

    def _fake_decompiler(self, body: str) -> Path:
        return _write_fake_binary(self.tempdir, "fake_decompiler.py", body)

    def _fake_recompiler(self, body: str) -> Path:
        return _write_fake_binary(self.tempdir, "fake_recompiler.py", body)

    def test_success_round_trip(self) -> None:
        decompiler = self._fake_decompiler(
            """
            import sys
            sys.stdout.write("local x = 1\\nreturn x\\n")
            sys.exit(0)
            """
        )
        recompiler = self._fake_recompiler(
            """
            import sys
            sys.stdout.write("Compiled 1 KLOC\\n")
            sys.exit(0)
            """
        )

        result = run_gate_one(self.capture, decompiler, recompiler, WORKSPACE)

        self.assertEqual(result.decompile_exit, 0)
        self.assertEqual(result.recompile_exit, 0)
        self.assertTrue(result.recompiles)
        self.assertEqual(result.output_lines, 2)
        self.assertEqual(result.bytecode_version, 6)
        self.assertEqual(result.input_bytes, len(b"\x06fakebytecode"))

    def test_decompile_failure_skips_recompile_step(self) -> None:
        decompiler = self._fake_decompiler(
            """
            import sys
            sys.stderr.write("decompiler error: [deserialize] bad bytecode\\n")
            sys.exit(1)
            """
        )
        # A recompiler that would fail the test loudly if it were ever run.
        recompiler = self._fake_recompiler(
            """
            import sys
            sys.exit(99)
            """
        )

        result = run_gate_one(self.capture, decompiler, recompiler, WORKSPACE)

        self.assertEqual(result.decompile_exit, 1)
        self.assertIsNone(result.recompile_exit)
        self.assertFalse(result.recompiles)
        self.assertIn("deserialize", result.decompile_error or "")

    def test_recompile_failure_is_reported_with_first_error(self) -> None:
        decompiler = self._fake_decompiler(
            """
            import sys
            sys.stdout.write("local a = 1\\n" * 3)
            sys.exit(0)
            """
        )
        recompiler = self._fake_recompiler(
            """
            import sys
            sys.stdout.write("Compiled 1 KLOC\\n")
            sys.stderr.write(
                "out.lua(1,1): CompileError: Out of local registers "
                "when trying to allocate v212: exceeded limit 200\\n"
            )
            sys.exit(1)
            """
        )

        result = run_gate_one(self.capture, decompiler, recompiler, WORKSPACE)

        self.assertEqual(result.decompile_exit, 0)
        self.assertEqual(result.recompile_exit, 1)
        self.assertFalse(result.recompiles)
        self.assertIsNotNone(result.first_compile_error)
        self.assertIn("exceeded limit 200", result.first_compile_error or "")

    def test_stdout_and_stderr_stay_separate(self) -> None:
        # Mirrors a profiling build: valid Lua on stdout, a JSON phase
        # report on stderr. The JSON must never leak into the Lua that
        # gets fed to the recompiler.
        decompiler = self._fake_decompiler(
            """
            import sys
            sys.stdout.write("local ok = true\\nreturn ok\\n")
            sys.stderr.write('{"phases": [], "peak_live_mb": 12.0}\\n')
            sys.exit(0)
            """
        )
        recompiler = self._fake_recompiler(
            """
            import sys
            source = open(sys.argv[-1], encoding="utf-8").read()
            if "phases" in source or "{" in source:
                sys.stderr.write("contaminated with stderr content\\n")
                sys.exit(1)
            sys.exit(0)
            """
        )

        result = run_gate_one(self.capture, decompiler, recompiler, WORKSPACE)

        self.assertTrue(result.recompiles)
        self.assertEqual(result.output_lines, 2)

    def test_output_path_is_written_even_on_recompile_failure(self) -> None:
        decompiler = self._fake_decompiler(
            """
            import sys
            sys.stdout.write("broken\\n")
            sys.exit(0)
            """
        )
        recompiler = self._fake_recompiler(
            """
            import sys
            sys.stderr.write("out.lua(1,1): CompileError: bad token\\n")
            sys.exit(1)
            """
        )
        output_path = self.tempdir / "saved.lua"

        result = run_gate_one(
            self.capture, decompiler, recompiler, WORKSPACE, output_path
        )

        self.assertFalse(result.recompiles)
        self.assertTrue(output_path.exists())
        self.assertEqual(output_path.read_text(encoding="utf-8").strip(), "broken")


class ExpandCapturesTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = Path(tempfile.mkdtemp(prefix="expand_captures_test_"))
        self.addCleanup(shutil.rmtree, self.tempdir, ignore_errors=True)

    def test_literal_path_must_exist(self) -> None:
        with self.assertRaises(SystemExit):
            _expand_captures([str(self.tempdir / "missing.luac")])

    def test_literal_existing_path_is_returned(self) -> None:
        target = self.tempdir / "one.luac"
        target.write_bytes(b"x")

        result = _expand_captures([str(target)])

        self.assertEqual(result, [target])

    def test_glob_pattern_expands_to_matches(self) -> None:
        (self.tempdir / "a.luac").write_bytes(b"x")
        (self.tempdir / "b.luac").write_bytes(b"x")
        (self.tempdir / "c.txt").write_bytes(b"x")

        result = _expand_captures([str(self.tempdir / "*.luac")])

        self.assertEqual(
            sorted(path.name for path in result), ["a.luac", "b.luac"]
        )

    def test_glob_with_no_matches_is_an_error(self) -> None:
        with self.assertRaises(SystemExit):
            _expand_captures([str(self.tempdir / "*.nope")])


@unittest.skipUnless(
    (WORKSPACE / DEFAULT_DECOMPILER).exists()
    and (WORKSPACE / DEFAULT_RECOMPILER).exists(),
    "release decompiler and/or luau-compile binary not built",
)
class RealBinaryIntegrationTests(unittest.TestCase):
    """A small end-to-end smoke test against the real toolchain, mirroring
    the corpus runner's style but through this module's own entry point."""

    def test_tiny_corpus_case_round_trips(self) -> None:
        case = WORKSPACE / "tests/luau_corpus/cases/01_literals_locals.luau"
        if not case.exists():
            self.skipTest("corpus fixture not present")

        tempdir = Path(tempfile.mkdtemp(prefix="gate_integration_"))
        self.addCleanup(shutil.rmtree, tempdir, ignore_errors=True)

        import subprocess

        compiled = subprocess.run(
            (
                str(WORKSPACE / DEFAULT_RECOMPILER),
                "--binary",
                "-O1",
                "-g1",
                str(case),
            ),
            cwd=WORKSPACE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(compiled.returncode, 0, compiled.stderr)
        bytecode_path = tempdir / "case.luac"
        bytecode_path.write_bytes(compiled.stdout)

        result = run_gate_one(
            bytecode_path,
            WORKSPACE / DEFAULT_DECOMPILER,
            WORKSPACE / DEFAULT_RECOMPILER,
            WORKSPACE,
        )

        self.assertEqual(result.decompile_exit, 0)
        self.assertEqual(result.recompile_exit, 0)
        self.assertTrue(result.recompiles)
        self.assertGreater(result.output_lines, 0)


@unittest.skipUnless(
    os.environ.get("MEDAL_LARGE_CAPTURE"),
    "set MEDAL_LARGE_CAPTURE to a large obfuscated capture to run this check",
)
class LargeCaptureRecompilesTest(unittest.TestCase):
    """Checks a capture large enough to strain Luau's 200-local ceiling.

    Such a capture declares far more locals than the ceiling allows unless
    scope narrowing groups them, so it is the shape most likely to regress.
    Not part of the default suite because the capture lives outside the
    repository."""

    def test_a_large_capture_recompiles(self) -> None:
        fixture = Path(os.environ["MEDAL_LARGE_CAPTURE"])
        result = run_gate_one(
            fixture,
            WORKSPACE / DEFAULT_DECOMPILER,
            WORKSPACE / DEFAULT_RECOMPILER,
            WORKSPACE,
        )

        self.assertEqual(result.decompile_exit, 0)
        self.assertEqual(
            result.recompile_exit, 0, result.first_compile_error or "recompile failed"
        )


if __name__ == "__main__":
    unittest.main()
