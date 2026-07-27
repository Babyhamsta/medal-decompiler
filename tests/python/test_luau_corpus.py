from __future__ import annotations

import json
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

from tools.luau_corpus import (
    COMPATIBILITY_PROFILES,
    PRIMARY_PROFILES,
    SECONDARY_PROFILES,
    CompileProfile,
)
from tools.luau_corpus.process import (
    compiler_command,
    decompiler_command,
    run_corpus,
)
from tools.luau_corpus.report import write_json_summary, write_markdown_summary
from tools.run_luau_corpus import select_profiles


class ProfileTests(unittest.TestCase):
    def test_all_profile_selection_includes_compatibility_versions(self) -> None:
        self.assertEqual(
            select_profiles("all"),
            PRIMARY_PROFILES + SECONDARY_PROFILES + COMPATIBILITY_PROFILES,
        )

    def test_primary_profile_commands_preserve_required_levels(self) -> None:
        commands = [
            compiler_command(Path("compiler"), Path("case.luau"), profile, "binary")
            for profile in PRIMARY_PROFILES
        ]

        self.assertEqual(
            commands,
            [
                ("compiler", "--binary", "-O1", "-g1", "case.luau"),
                ("compiler", "--binary", "-O2", "-g1", "case.luau"),
                ("compiler", "--binary", "-O2", "-g0", "case.luau"),
            ],
        )

    def test_secondary_profile_commands_preserve_required_levels(self) -> None:
        commands = [
            compiler_command(Path("compiler"), Path("case.luau"), profile, "null")
            for profile in SECONDARY_PROFILES
        ]

        self.assertEqual(
            commands,
            [
                ("compiler", "--null", "-O0", "-g1", "case.luau"),
                ("compiler", "--null", "-O1", "-g0", "case.luau"),
                ("compiler", "--null", "-O1", "-g2", "case.luau"),
            ],
        )

    def test_decompiler_command_uses_binary_path_and_bytecode(self) -> None:
        self.assertEqual(
            decompiler_command(Path("decompiler"), Path("case.bc")),
            ("decompiler", "case.bc"),
        )

    def test_compatibility_profiles_emit_one_fast_flag_argument(self) -> None:
        commands = [
            compiler_command(Path("compiler"), Path("case.luau"), profile, "binary")
            for profile in COMPATIBILITY_PROFILES
        ]

        self.assertEqual(
            commands,
            [
                (
                    "compiler",
                    "--binary",
                    "-O1",
                    "-g1",
                    "--fflags=LuauBytecodeCostModel=false,LuauEmitCallFeedback=false,DebugLuauUserDefinedClasses=false",
                    "case.luau",
                ),
                (
                    "compiler",
                    "--binary",
                    "-O1",
                    "-g1",
                    "--fflags=LuauBytecodeCostModel=false,LuauEmitCallFeedback=false,DebugLuauUserDefinedClasses=true",
                    "case.luau",
                ),
                (
                    "compiler",
                    "--binary",
                    "-O1",
                    "-g1",
                    "--fflags=LuauBytecodeCostModel=false,LuauEmitCallFeedback=true,DebugLuauUserDefinedClasses=false",
                    "case.luau",
                ),
                (
                    "compiler",
                    "--binary",
                    "-O1",
                    "-g1",
                    "--fflags=LuauBytecodeCostModel=true",
                    "case.luau",
                ),
            ],
        )

    def test_bundled_compiler_emits_requested_compatibility_versions(self) -> None:
        workspace = Path(__file__).resolve().parents[2]
        compiler = workspace / ".tools" / "luau-windows" / "luau-compile.exe"
        source = workspace / "tests" / "luau_corpus" / "cases" / "01_literals_locals.luau"
        if not compiler.exists():
            self.skipTest("bundled Luau compiler is absent")

        observed = []
        for profile in COMPATIBILITY_PROFILES:
            compiled = subprocess.run(
                compiler_command(compiler, source, profile, "binary"),
                cwd=workspace,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertEqual(compiled.returncode, 0, compiled.stderr.decode())
            observed.append(compiled.stdout[0])

        self.assertEqual(observed, [9, 10, 11, 12])


class CorpusRunnerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary_directory.name)
        self.cases = self.root / "tests" / "luau_corpus" / "cases"
        self.tools = self.root / ".tools" / "luau-windows"
        self.cases.mkdir(parents=True)
        self.tools.mkdir(parents=True)
        (self.cases / "01_success.luau").write_text("return 1\n", encoding="utf-8")
        (self.cases / "02_compile_fail.luau").write_text(
            "COMPILE_FAIL\n", encoding="utf-8"
        )

        self.compiler = self.tools / "luau_compile_stub.py"
        self.decompiler = self.root / "decompiler_stub.py"
        self._write_executable(
            self.compiler,
            """
            import pathlib
            import sys

            source = pathlib.Path(sys.argv[-1]).read_text(encoding="utf-8")
            if "COMPILE_FAIL" in source:
                print("compile rejected", file=sys.stderr)
                raise SystemExit(9)
            if "--binary" in sys.argv:
                sys.stdout.buffer.write(b"BYTECODE")
            """,
        )
        self._write_executable(
            self.decompiler,
            """
            print("local value = 1")
            print("return value")
            """,
        )

    def tearDown(self) -> None:
        self.temporary_directory.cleanup()

    def _write_executable(self, path: Path, body: str) -> None:
        path.write_text(
            textwrap.dedent(body).lstrip(),
            encoding="utf-8",
        )

    def test_compile_failure_is_recorded_without_stopping_next_case(self) -> None:
        output = self.root / "output"

        result = run_corpus(
            workspace=self.root,
            output_root=output,
            profiles=(CompileProfile("test", 1, 1),),
            compiler=self.compiler,
            decompiler=self.decompiler,
        )

        self.assertEqual(len(result.cases), 2)
        success, failure = result.cases
        self.assertEqual(
            (success.compile_exit, success.decompile_exit, success.recompile_exit),
            (0, 0, 0),
        )
        self.assertEqual(success.bytecode_version, ord("B"))
        self.assertEqual(
            (failure.compile_exit, failure.decompile_exit, failure.recompile_exit),
            (9, None, None),
        )
        self.assertIn(
            "[compile]\nexit=9\ncompile rejected",
            failure.diagnostic_path.read_text(encoding="utf-8"),
        )

    def test_case_filter_selects_matching_source(self) -> None:
        result = run_corpus(
            workspace=self.root,
            output_root=self.root / "filtered",
            profiles=(CompileProfile("test", 1, 1),),
            case_filter="success",
            compiler=self.compiler,
            decompiler=self.decompiler,
        )

        self.assertEqual([case.case_name for case in result.cases], ["01_success"])

    def test_reports_are_stable_and_use_relative_artifact_paths(self) -> None:
        result = run_corpus(
            workspace=self.root,
            output_root=self.root / "reported",
            profiles=(CompileProfile("test", 1, 1),),
            case_filter="success",
            compiler=self.compiler,
            decompiler=self.decompiler,
        )

        json_path = write_json_summary(result)
        markdown_path = write_markdown_summary(result)
        payload = json.loads(json_path.read_text(encoding="utf-8"))
        markdown = markdown_path.read_text(encoding="utf-8")

        self.assertEqual(payload["totals"]["cases"], 1)
        self.assertEqual(payload["totals"]["compile_failed"], 0)
        self.assertEqual(payload["cases"][0]["bytecode_version"], ord("B"))
        self.assertEqual(payload["cases"][0]["output_path"], "test/01_success.luau")
        self.assertIn(
            "| profile | case | version | compile | decompile | recompile | statements | locals | gotos |",
            markdown,
        )
        self.assertIn(
            "| test | 01_success | 66 | 0 | 0 | 0 | 2 | 1 | 0 |",
            markdown,
        )


if __name__ == "__main__":
    unittest.main()
