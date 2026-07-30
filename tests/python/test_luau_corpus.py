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
    count_trivial_aliases,
    decompiler_command,
    readability_metrics,
    run_corpus,
)
from tools.luau_corpus.report import write_json_summary, write_markdown_summary
from tools.luau_corpus.semantic import (
    TRUSTED_SEMANTIC_PROBES,
    run_semantic_probe,
    runtime_command,
)
from tools.run_luau_corpus import run_failed, select_profiles


class ProfileTests(unittest.TestCase):
    # Luau's require rejects a module returning more than one value, so a
    # case with a multi-value return cannot be loaded by runner.luau.
    UNPROBEABLE_CASES = frozenset({"18_recursion"})

    def test_semantic_probe_manifest_covers_every_probeable_case(self) -> None:
        workspace = Path(__file__).resolve().parents[2]
        cases = {
            path.stem
            for path in (workspace / "tests" / "luau_corpus" / "cases").glob(
                "*.luau"
            )
        }

        self.assertEqual(
            sorted(cases - self.UNPROBEABLE_CASES),
            sorted(TRUSTED_SEMANTIC_PROBES),
        )
        self.assertEqual(
            tuple(TRUSTED_SEMANTIC_PROBES),
            tuple(sorted(TRUSTED_SEMANTIC_PROBES)),
        )

        for name, probe in TRUSTED_SEMANTIC_PROBES.items():
            with self.subTest(case=name):
                self.assertTrue((workspace / probe.probe_path).exists())

        self.assertNotIn(
            "27_orchestration_engine",
            TRUSTED_SEMANTIC_PROBES,
        )

    def test_runtime_command_passes_subject_and_probe_after_program_args(
        self,
    ) -> None:
        command = runtime_command(
            Path("luau"),
            Path("probes/runner.luau"),
            "../cases/04_calls_multireturn",
            "./04_calls_multireturn",
        )

        self.assertEqual(
            command,
            (
                "luau",
                "probes/runner.luau",
                "-a",
                "../cases/04_calls_multireturn",
                "./04_calls_multireturn",
            ),
        )

    def test_real_semantic_runner_preserves_nil_and_sorts_table_keys(
        self,
    ) -> None:
        workspace = Path(__file__).resolve().parents[2]
        runtime = workspace / ".tools" / "luau-windows" / "luau.exe"
        if not runtime.exists():
            self.skipTest("bundled Luau runtime is absent")

        probe_root = workspace / "tests" / "luau_corpus" / "probes"
        probe_root.mkdir(exist_ok=True)

        with tempfile.TemporaryDirectory(dir=probe_root) as temporary:
            modules = Path(temporary)
            (modules / "subject.luau").write_text(
                """\
return function()
    return "x", nil, { b = 2, a = 1 }
end
""",
                encoding="utf-8",
            )
            (modules / "probe.luau").write_text(
                """\
return function(subject)
    return subject()
end
""",
                encoding="utf-8",
            )
            relative_root = f"./{modules.name}"
            command = runtime_command(
                runtime,
                probe_root / "runner.luau",
                f"{relative_root}/subject",
                f"{relative_root}/probe",
            )

            outputs = [
                subprocess.run(
                    command,
                    cwd=workspace,
                    check=False,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
                for _ in range(2)
            ]

        for completed in outputs:
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(
                completed.stdout.strip(),
                "SEMANTIC_RESULT p3[s1:x;n;t2{s1:a;d1;s1:b;d2;}]",
            )

    def test_semantic_runner_times_out_instead_of_blocking_matrix(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            runtime = root / "runtime.py"
            runtime.write_text(
                "import time\ntime.sleep(1)\n",
                encoding="utf-8",
            )

            result = run_semantic_probe(
                runtime,
                root / "runner.luau",
                root / "subject.luau",
                root / "probe.luau",
                root,
                timeout_seconds=0.01,
            )

        self.assertEqual(result.exit_code, -1)
        self.assertIsNone(result.normalized_result)
        self.assertIn("timed out", result.stderr)

    def test_trusted_probes_produce_literal_source_results(self) -> None:
        workspace = Path(__file__).resolve().parents[2]
        runtime = workspace / ".tools" / "luau-windows" / "luau.exe"
        if not runtime.exists():
            self.skipTest("bundled Luau runtime is absent")

        runner = workspace / "tests" / "luau_corpus" / "probes" / "runner.luau"
        expected = {
            "01_literals_locals": (
                "p1[t4{s5:count;d42;s5:ratio;d3.5;"
                "s7:enabled;b0;s7:message;s5:medal;}]"
            ),
            "02_expression_precedence": (
                "p15[d14.199999999999999;d-1;s9:value:5:4;b1;b1;d11;d24;s9:value:4:2;b0;"
                "b1;d0;d0;s9:value:0:7;b0;b0;]"
            ),
            "03_parallel_assignment": "p6[d1;n;d3;d3;d2;d1;]",
            "04_calls_multireturn": "p2[s5:q=5:4;d12;]",
            "05_varargs": (
                "p11[s1:p;d4;d1;d2;d3;"
                "t3{d1;d2;d2;d3;d3;d4;}"
                "d4;d1;d2;d3;t3{d1;d2;d2;d3;d3;d4;}]"
            ),
            "06_method_chains": "p1[d32;]",
            "07_table_literals": (
                "p1[t4{s5:alpha;d99;s5:array;"
                "t5{d1;d1;d2;d2;d3;d3;d4;d5;d5;d8;}"
                "s5:mixed;t6{d1;d10;d2;d20;s17:not an identifier;b1;"
                "s4:name;s5:alpha;s5:child;t3{d1;t2{d1;s4:deep;d2;b0;}"
                "s1:x;d4;s1:y;d8;}s5:score;d99;}"
                "s6:record;t2{s4:left;s1:L;s5:right;s1:R;}}]"
            ),
            "08_table_incremental": (
                "p1[t6{d1;s5:first;d2;s6:second;d3;s4:tail;"
                "s4:name;s11:incremental;"
                "s5:child;t2{d1;d42;s7:enabled;b1;}s5:extra;d42;}]"
            ),
            "09_if_elseif_else": (
                "p6[s6:inside;s7:outside;s7:outside;s8:negative;s4:zero;s6:almost;]"
            ),
            "10_short_circuit": (
                "p12[b0;b1;d7;t5{d1;s1:a;d2;s1:b;d3;s1:x;d4;s4:left;d5;s8:fallback;}d9;b"
                "1;b1;t5{d1;s1:a;d2;s1:b;d3;s1:c;d4;s1:x;d5;s4:left;}b0;d3;d3;t5{d1;s1:a"
                ";d2;s1:x;d3;s1:y;d4;s1:z;d5;s8:fallback;}]"
            ),
            "11_conditional_expression": (
                "p5[t2{s3:tag;s3:low;s5:value;d1;}t2{s3:tag;s6:inside;s5:value;d7;}t2{s3"
                ":tag;s4:high;s5:value;d12;}t2{s3:tag;s3:low;s5:value;d-4;}t2{s3:tag;s4:"
                "high;s5:value;d18;}]"
            ),
            "12_while_break_continue": "p4[d20;d7;d4;d6;]",
            "13_repeat_until": "p2[d10;d6;]",
            "14_numeric_for": "p2[d81;d72;]",
            "15_generic_for": (
                "p1[t8{d1;d4;d2;d8;d3;d20;d4;d12;"
                "d5;d6;d6;d2;d7;d0;s4:name;s4:kept;}]"
            ),
            "16_closure_capture": (
                "p6[s9:transform;d8;s9:transform;d13;s9:transform;d53;]"
            ),
            "17_mutable_upvalue": "p6[d5;d3;d3;d8;d8;d7;]",
            "19_callback_factory": "p5[s4:id-a;d16;d3;s3:id-;d16;]",
            "20_pcall_style_flow": (
                "p3[p2[d7;d8;]p2[d9;s9:recovered;]"
                "p2[n;s13:still missing;]]"
            ),
            "21_state_machine": "p3[s4:done;d1;d3;]",
            "22_nested_early_exits": "p4[d4;d1;s6:target;n;]",
            "23_register_pressure_aliases": (
                "p12[d94;d194;t14{d10;d29;d11;d30;d12;d31;d13;d32;d14;d33;d1;d20;d2;d21;"
                "d3;d22;d4;d23;d5;d24;d6;d25;d7;d26;d8;d27;d9;d28;}d6;d14;d22;d118;d218;"
                "t14{d10;d29;d11;d30;d12;d31;d13;d32;d14;d33;d1;d20;d2;d21;d3;d22;d4;d23"
                ";d5;d24;d6;d25;d7;d26;d8;d27;d9;d28;}d6;b0;d22;]"
            ),
            "24_wonky_integration": (
                "p6[s4:done;d15;t6{d1;t3{s5:label;s5:start;s5:state;s7:running;s5:value;"
                "d1;}d2;t3{s5:label;s3:add;s5:state;s7:running;s5:value;d9;}d3;t3{s5:lab"
                "el;s5:pause;s5:state;s6:paused;s5:value;d9;}d4;t3{s5:label;s6:resume;s5"
                ":state;s7:running;s5:value;d9;}d5;t3{s5:label;s3:add;s5:state;s7:runnin"
                "g;s5:value;d15;}d6;t3{s5:label;s4:stop;s5:state;s4:done;s5:value;d15;}}"
                "s7:running;d2;t5{d1;t3{s5:label;s5:start;s5:state;s7:running;s5:value;d"
                "1;}d2;t3{s5:label;s7:unknown;s5:state;s7:running;s5:value;s5:weird;}d3;"
                "t2{s5:label;s7:unknown;s5:state;s7:running;}d4;t3{s5:label;s5:error;s5:"
                "state;s7:running;s5:value;d3;}d5;t3{s5:label;s7:unknown;s5:state;s7:run"
                "ning;s5:value;b0;}}]"
            ),
            "25_product_controller": (
                "p5[b1;s6:pong:1;b1;s16:fallback:missing;d2;]"
            ),
            "26_adversarial_dataflow": (
                "p14[b1;s8:accepted;d1;d12;"
                "b1;s8:accepted;d2;d20;"
                "b0;s7:missing;s6:absent;"
                "d20;d2;t2{d1;d5;d2;d8;}]"
            ),
        }

        for case_name, normalized in expected.items():
            with self.subTest(case=case_name):
                command = runtime_command(
                    runtime,
                    runner,
                    f"../cases/{case_name}",
                    f"./{case_name}",
                )
                completed = subprocess.run(
                    command,
                    cwd=workspace,
                    check=False,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )

                self.assertEqual(completed.returncode, 0, completed.stderr)
                self.assertEqual(
                    completed.stdout.strip(),
                    f"SEMANTIC_RESULT {normalized}",
                )

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

    def test_readability_metrics_count_spacing_names_and_slots(self) -> None:
        source = textwrap.dedent(
            """\
            local v1 = {}

            v1[1] = "a"
            v1[2] = "b"
            local named = v1

            local wide = "0123456789"
            """
        )

        metrics = readability_metrics(source)

        self.assertEqual(metrics.blank_lines, 2)
        self.assertEqual(metrics.generated_placeholder_locals, 1)
        self.assertEqual(metrics.slot_assignments, 2)
        self.assertEqual(metrics.long_lines, 0)

    def test_readability_metrics_flag_only_lines_past_the_column_budget(
        self,
    ) -> None:
        short = "local a = " + '"' + "x" * 100 + '"'
        long = "local b = " + '"' + "y" * 130 + '"'

        metrics = readability_metrics(f"{short}\n{long}\n")

        self.assertEqual(metrics.long_lines, 1)

    def test_placeholder_local_metric_ignores_meaningful_names(self) -> None:
        source = "local v1, v2 = 1, 2\nlocal stack = {}\nlocal p3 = 4\n"

        metrics = readability_metrics(source)

        self.assertEqual(metrics.generated_placeholder_locals, 3)


class CorpusRunnerTests(unittest.TestCase):
    def test_trivial_alias_metric_counts_identifier_copies(self) -> None:
        source = """\
local copy = original
local value = call()
table.slot = original
return copy
"""

        self.assertEqual(count_trivial_aliases(source), 1)

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
            print("local alias = value")
            print("return alias")
            """,
        )

    def tearDown(self) -> None:
        self.temporary_directory.cleanup()

    def _write_executable(self, path: Path, body: str) -> None:
        path.write_text(
            textwrap.dedent(body).lstrip(),
            encoding="utf-8",
        )

    def _add_case(self, name: str) -> None:
        (self.cases / f"{name}.luau").write_text(
            "return 1\n",
            encoding="utf-8",
        )

    def _write_runtime(self, body: str) -> Path:
        runtime = self.tools / "luau_runtime_stub.py"
        self._write_executable(runtime, body)
        return runtime

    def test_semantic_mode_executes_only_allowlisted_cases(self) -> None:
        self._add_case("04_calls_multireturn")
        self._add_case("99_untrusted")
        invocation_log = self.root / "runtime-invocations.txt"
        runtime = self._write_runtime(
            f"""
            import pathlib
            import sys

            subject = sys.argv[-2]
            with pathlib.Path({str(invocation_log)!r}).open(
                "a",
                encoding="utf-8",
            ) as output:
                output.write(subject + "\\n")

            if "/cases/" in subject:
                print("SEMANTIC_RESULT p1[d1;]")
            else:
                print("SEMANTIC_RESULT p1[d2;]")
            """
        )

        result = run_corpus(
            workspace=self.root,
            output_root=self.root / "semantic",
            profiles=(CompileProfile("test", 1, 1),),
            compiler=self.compiler,
            decompiler=self.decompiler,
            semantic=True,
            runtime=runtime,
        )

        by_name = {case.case_name: case for case in result.cases}
        trusted = by_name["04_calls_multireturn"]
        untrusted = by_name["99_untrusted"]
        runtime_invocations = invocation_log.read_text(
            encoding="utf-8",
        ).splitlines()

        self.assertEqual(len(runtime_invocations), 2)
        self.assertTrue(
            all(
                "04_calls_multireturn" in invocation
                for invocation in runtime_invocations
            )
        )
        self.assertIsNone(untrusted.source_runtime)
        self.assertIsNone(untrusted.generated_runtime)
        self.assertIsNone(untrusted.semantic_match)
        self.assertEqual(trusted.source_runtime.exit_code, 0)
        self.assertEqual(trusted.generated_runtime.exit_code, 0)
        self.assertFalse(trusted.semantic_match)
        self.assertIn(
            "[source-runtime]\nexit=0\nresult=p1[d1;]",
            trusted.diagnostic_path.read_text(encoding="utf-8"),
        )
        self.assertIn(
            "[generated-runtime]\nexit=0\nresult=p1[d2;]",
            trusted.diagnostic_path.read_text(encoding="utf-8"),
        )

        json_path = write_json_summary(result)
        markdown_path = write_markdown_summary(result)
        payload = json.loads(json_path.read_text(encoding="utf-8"))
        markdown = markdown_path.read_text(encoding="utf-8")
        trusted_payload = next(
            case
            for case in payload["cases"]
            if case["case_name"] == "04_calls_multireturn"
        )

        self.assertEqual(payload["totals"]["semantic_checked"], 1)
        self.assertEqual(payload["totals"]["semantic_mismatched"], 1)
        self.assertEqual(trusted_payload["source_runtime_exit"], 0)
        self.assertEqual(
            trusted_payload["generated_runtime_result"],
            "p1[d2;]",
        )
        self.assertFalse(trusted_payload["semantic_match"])
        self.assertIn("| source run | generated run | semantic |", markdown)

    def test_generated_runtime_failure_does_not_stop_later_probe(self) -> None:
        self._add_case("04_calls_multireturn")
        self._add_case("05_varargs")
        runtime = self._write_runtime(
            """
            import sys

            subject = sys.argv[-2]
            if "04_calls_multireturn" in subject and "/cases/" not in subject:
                print("generated runtime rejected", file=sys.stderr)
                raise SystemExit(7)

            case = (
                "04_calls_multireturn"
                if "04_calls_multireturn" in subject
                else "05_varargs"
            )
            print(f"SEMANTIC_RESULT {case}")
            """
        )

        result = run_corpus(
            workspace=self.root,
            output_root=self.root / "runtime-failure",
            profiles=(CompileProfile("test", 1, 1),),
            compiler=self.compiler,
            decompiler=self.decompiler,
            semantic=True,
            runtime=runtime,
        )

        by_name = {case.case_name: case for case in result.cases}

        self.assertEqual(
            by_name["04_calls_multireturn"].generated_runtime.exit_code,
            7,
        )
        self.assertFalse(by_name["04_calls_multireturn"].semantic_match)
        self.assertTrue(by_name["05_varargs"].semantic_match)

    def test_semantic_exit_policy_only_blocks_checked_mismatches(self) -> None:
        self._add_case("04_calls_multireturn")
        runtime = self._write_runtime(
            """
            import sys

            subject = sys.argv[-2]
            result = "source" if "/cases/" in subject else "generated"
            print(f"SEMANTIC_RESULT {result}")
            """
        )
        mismatched = run_corpus(
            workspace=self.root,
            output_root=self.root / "exit-policy-mismatch",
            profiles=(CompileProfile("test", 1, 1),),
            case_filter="04_calls",
            compiler=self.compiler,
            decompiler=self.decompiler,
            semantic=True,
            runtime=runtime,
        )
        unchecked = run_corpus(
            workspace=self.root,
            output_root=self.root / "exit-policy-unchecked",
            profiles=(CompileProfile("test", 1, 1),),
            case_filter="success",
            compiler=self.compiler,
            decompiler=self.decompiler,
            semantic=True,
            runtime=runtime,
        )

        self.assertFalse(run_failed(mismatched, semantic=False))
        self.assertTrue(run_failed(mismatched, semantic=True))
        self.assertFalse(run_failed(unchecked, semantic=True))

    def test_exit_policy_rejects_empty_or_reduced_expected_matrix(self) -> None:
        empty = run_corpus(
            workspace=self.root,
            output_root=self.root / "empty-selection",
            profiles=(CompileProfile("test", 1, 1),),
            case_filter="definitely_missing",
            compiler=self.compiler,
            decompiler=self.decompiler,
            semantic=True,
        )
        expected = frozenset(
            {
                ("01_success", "test"),
                ("02_compile_fail", "test"),
            }
        )

        self.assertTrue(
            run_failed(
                empty,
                semantic=True,
                expected_cases=frozenset(),
                expected_semantic=frozenset(),
            )
        )
        self.assertTrue(
            run_failed(
                empty,
                semantic=True,
                expected_cases=expected,
                expected_semantic=frozenset(),
            )
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

        self.assertEqual(result.cases[0].generated_aliases, 1)
        self.assertEqual(payload["totals"]["cases"], 1)
        self.assertEqual(payload["totals"]["compile_failed"], 0)
        self.assertEqual(payload["cases"][0]["bytecode_version"], ord("B"))
        self.assertEqual(payload["cases"][0]["output_path"], "test/01_success.luau")
        self.assertEqual(payload["cases"][0]["generated_aliases"], 1)
        self.assertEqual(payload["totals"]["semantic_checked"], 0)
        self.assertIsNone(payload["cases"][0]["source_runtime_exit"])
        self.assertIsNone(payload["cases"][0]["semantic_match"])
        self.assertIn(
            "| profile | case | version | compile | decompile | recompile | source run | generated run | semantic | statements | locals | aliases | gotos |",
            markdown,
        )
        self.assertIn(
            "| test | 01_success | 66 | 0 | 0 | 0 | - | - | - | 3 | 2 | 1 | 0 |",
            markdown,
        )


if __name__ == "__main__":
    unittest.main()
