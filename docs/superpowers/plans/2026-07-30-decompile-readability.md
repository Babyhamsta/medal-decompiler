# Decompile Readability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make decompiled Luau readable at scale — meaningful local names instead of `v1..vN`, register-slot noise folded back into expressions, and vertical spacing — without changing program behaviour or regressing throughput.

**Architecture:** Two new AST passes (`slot_folding`, `table_construction`) join the existing per-function `AstRecovery` phase inside a bounded fixpoint loop. One new whole-program pass (`name_flow`) writes name hints before `name_locals` runs. `name_locals` and `formatter` are extended in place. `declare_locals` + `validate_bindings` stay downstream of everything as the correctness gate.

**Tech Stack:** Rust (nightly toolchain, edition 2024), Python 3 (stdlib only, `unittest`), Luau (bundled compiler and runtime under `.tools/luau-windows/`).

## Global Constraints

- Worktree: `C:/Users/Admin/Downloads/medal-decompiler-main/.worktrees/decompile-readability`, branch `feat/decompile-readability`. All work happens here.
- The crate requires nightly. Every cargo command uses `cargo +nightly`. Plain `cargo` fails with `error: could not compile ast` on `#![feature(...)]`.
- Spec: `docs/superpowers/specs/2026-07-30-decompile-readability-design.md`. Where this plan and the spec disagree, the spec controls.
- No feature flags. All new behaviour ships default-on.
- Python is stdlib only. Do not add `psutil`, `pytest`, or any dependency.
- Never write comments containing "fixed", "was broken", "previously", or similar historical framing.
- Performance ceilings, measured on the stage-27 fixture: wall clock **16.0 s**, peak RSS **2,000 MB**, `ast-recovery` phase **1.0 s**, `format` phase **1.5 s**. Baseline is 13.42–13.56 s / 1,818 MB / 0.060 s / 0.205 s.
- Correctness gate, unchanged by any task: 78 corpus runs with 0 compile/decompile/recompile failures, and every semantic probe's `SEMANTIC_RESULT` identical between source and decompiled output.

### Commands used throughout

```bash
# from the worktree root
cargo +nightly build --release -p luau-lifter
cargo +nightly test -p ast
python -m unittest tests.python.test_luau_corpus -v

# full corpus, no semantics
python tools/run_luau_corpus.py --profiles primary \
  --decompiler target/release/luau-lifter.exe --no-build \
  --output tests/luau_corpus/results/current

# full corpus with semantic probes
python tools/run_luau_corpus.py --profiles primary --semantic \
  --decompiler target/release/luau-lifter.exe --no-build \
  --output tests/luau_corpus/results/current
```

---

## File Structure

**Created:**

| Path | Responsibility |
| --- | --- |
| `ast/src/slot_folding.rs` | Fold table-slot writes into later reads. The only pass that can change semantics. |
| `ast/src/table_construction.rs` | Fold `t[k] = v` runs into table constructors. |
| `ast/src/name_flow.rs` | Write name hints onto locals from callee parameter names, before `name_locals`. |
| `tools/measure_decompiler.py` | Wall clock, peak RSS, and per-phase timing on a fixture. |
| `tests/luau_corpus/cases/27_..32_*.luau` | Six cases exercising slot-folding preconditions. |
| `tests/luau_corpus/probes/*.luau` | Twenty new probes plus six for the new cases. |

**Modified:**

| Path | Change |
| --- | --- |
| `ast/src/lib.rs` | Register the three new modules and re-export their entry points. |
| `ast/src/formatter.rs` | Blank lines between statements; shared `table_renders_multiline`; 120-column wrap guard. |
| `ast/src/name_locals.rs` | Scope stack replacing the flat `used_names`; shape-driven fallback names; library-return names. |
| `luau-lifter/src/lib.rs:1076-1079` | Fixpoint loop around the recovery passes; `propagate_parameter_names` call. |
| `tools/luau_corpus/model.py` | New `CaseResult` metric fields. |
| `tools/luau_corpus/process.py` | Compute the new metrics. |
| `tools/luau_corpus/report.py` | Report the new metrics. |
| `tools/luau_corpus/semantic.py` | `_PROBE_NAMES` grows from 6 to 32. |
| `tests/python/test_luau_corpus.py` | Update the two tests that pin the probe manifest. |

---

## Task 1: Readability Metrics In The Corpus Harness

Adds the numbers that answer "did this actually improve readability". Reported, never gated.

**Files:**
- Modify: `tools/luau_corpus/model.py`
- Modify: `tools/luau_corpus/process.py:69-73` (`_output_metrics`)
- Modify: `tools/luau_corpus/report.py`
- Test: `tests/python/test_luau_corpus.py`

**Interfaces:**
- Consumes: nothing.
- Produces: `readability_metrics(output: str) -> ReadabilityMetrics`, a frozen dataclass with fields `blank_lines: int`, `generated_placeholder_locals: int`, `slot_assignments: int`, `long_lines: int`. `CaseResult` gains those four fields with defaults of `0`.

- [ ] **Step 1: Write the failing test**

Append to `class ProfileTests` in `tests/python/test_luau_corpus.py`:

```python
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
```

Add to the imports at the top of that file:

```python
from tools.luau_corpus.process import (
    compiler_command,
    count_trivial_aliases,
    decompiler_command,
    readability_metrics,
    run_corpus,
)
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `python -m unittest tests.python.test_luau_corpus -v`

Expected: FAIL with `ImportError: cannot import name 'readability_metrics'`.

- [ ] **Step 3: Add the metrics dataclass**

In `tools/luau_corpus/model.py`, add after the `RuntimeResult` dataclass:

```python
@dataclass(frozen=True)
class ReadabilityMetrics:
    blank_lines: int
    generated_placeholder_locals: int
    slot_assignments: int
    long_lines: int
```

Add these four fields to `CaseResult`, after `semantic_match`:

```python
    blank_lines: int = 0
    generated_placeholder_locals: int = 0
    slot_assignments: int = 0
    long_lines: int = 0
```

Export `ReadabilityMetrics` from `tools/luau_corpus/__init__.py` by adding it to both the `from .model import (...)` list and `__all__`.

- [ ] **Step 4: Implement the metric computation**

In `tools/luau_corpus/process.py`, add `import re` at the top, extend the model import to include `ReadabilityMetrics`, and add:

```python
COLUMN_BUDGET = 120

_PLACEHOLDER_LOCAL = re.compile(r"^[vp]\d+$")
_SLOT_ASSIGNMENT = re.compile(
    r"^[A-Za-z_][A-Za-z0-9_]*\[(?:\d+|\"[^\"]*\")\]\s*="
)


def readability_metrics(output: str) -> ReadabilityMetrics:
    """Count the output properties this work is trying to move.

    These are diagnostics, not gates. A run is never failed for them; they
    exist so "more readable" can be answered with a number.
    """
    blank_lines = 0
    placeholder_locals = 0
    slot_assignments = 0
    long_lines = 0

    for line in output.splitlines():
        if not line.strip():
            blank_lines += 1
            continue
        if len(line) > COLUMN_BUDGET:
            long_lines += 1
        stripped = line.strip()
        if stripped.startswith("local "):
            declared = stripped.removeprefix("local ").split("=", maxsplit=1)[0]
            placeholder_locals += sum(
                bool(_PLACEHOLDER_LOCAL.fullmatch(name.strip()))
                for name in declared.split(",")
            )
        if _SLOT_ASSIGNMENT.match(stripped):
            slot_assignments += 1

    return ReadabilityMetrics(
        blank_lines=blank_lines,
        generated_placeholder_locals=placeholder_locals,
        slot_assignments=slot_assignments,
        long_lines=long_lines,
    )
```

- [ ] **Step 5: Run the new tests to verify they pass**

Run: `python -m unittest tests.python.test_luau_corpus -v`

Expected: PASS, 22 tests.

- [ ] **Step 6: Wire the metrics into every case result**

In `run_corpus` in `tools/luau_corpus/process.py`, immediately after the existing line `aliases = count_trivial_aliases(output_text)`, add:

```python
            readability = readability_metrics(output_text)
```

and add these four arguments to the `CaseResult(...)` construction, after `semantic_match=semantic_match,`:

```python
                    blank_lines=readability.blank_lines,
                    generated_placeholder_locals=(
                        readability.generated_placeholder_locals
                    ),
                    slot_assignments=readability.slot_assignments,
                    long_lines=readability.long_lines,
```

- [ ] **Step 7: Report the metrics**

Read `tools/luau_corpus/report.py` to find the markdown header row and the per-case row construction. Add four columns named `blank`, `vN`, `slots`, and `wide`, sourced from the matching `CaseResult` fields, positioned after the existing `gotos` column. Add the same four keys to the JSON summary's per-case object.

- [ ] **Step 8: Verify the full harness still runs and now reports the columns**

Run:

```bash
python tools/run_luau_corpus.py --profiles primary \
  --decompiler target/release/luau-lifter.exe --no-build \
  --output tests/luau_corpus/results/current
head -8 tests/luau_corpus/results/current/summary.md
```

Expected: `Cases: 78; compile failures: 0; decompile failures: 0; recompile failures: 0.` and a table whose header includes `blank | vN | slots | wide`. Every `blank` value is `0` — that is the current state and the number Task 4 moves.

- [ ] **Step 9: Run the whole test suite and commit**

```bash
python -m unittest tests.python.test_luau_corpus -v
git add tools/luau_corpus/ tests/python/test_luau_corpus.py
git commit -m "test: report readability metrics per corpus case"
```

---

## Task 2: Performance Measurement Tool

The perf ceiling is a hard gate. It needs one command that produces the numbers, so every later task can check itself.

**Files:**
- Create: `tools/measure_decompiler.py`
- Test: `tests/python/test_measure_decompiler.py`

**Interfaces:**
- Consumes: nothing.
- Produces: `parse_phase_report(stderr: str) -> dict` returning the profiling JSON object, raising `ValueError` when no JSON object is present. CLI: `python tools/measure_decompiler.py --fixture <path> [--runs 3]` printing a markdown table to stdout.

Peak RSS comes from the profiling build's own `peak_live_mb`, not from an external process monitor. That keeps the tool stdlib-only and platform-independent.

- [ ] **Step 1: Write the failing test**

Create `tests/python/test_measure_decompiler.py`:

```python
from __future__ import annotations

import unittest

from tools.measure_decompiler import parse_phase_report


class ParsePhaseReportTests(unittest.TestCase):
    def test_extracts_the_json_object_from_surrounding_output(self) -> None:
        stderr = (
            "warning: something\n"
            '{\n  "phases": [\n'
            '    {"phase": "format", "seconds": 0.205, "alloc_mb": 12.7}\n'
            '  ],\n  "peak_live_mb": 1622.5\n}\n'
            "trailing noise\n"
        )

        report = parse_phase_report(stderr)

        self.assertEqual(report["peak_live_mb"], 1622.5)
        self.assertEqual(report["phases"][0]["phase"], "format")

    def test_missing_report_is_an_error_not_an_empty_result(self) -> None:
        with self.assertRaises(ValueError):
            parse_phase_report("no json here")


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `python -m unittest tests.python.test_measure_decompiler -v`

Expected: FAIL with `ModuleNotFoundError: No module named 'tools.measure_decompiler'`.

- [ ] **Step 3: Implement the tool**

Create `tools/measure_decompiler.py`:

```python
"""Wall clock, peak heap, and per-phase cost for one decompiler fixture.

The release binary supplies wall clock. A second binary built with the
`profiling` feature supplies the phase table and peak live heap; that build
carries accounting overhead, so its total is not comparable to the release
timing and is not reported as such.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path

WORKSPACE = Path(__file__).resolve().parents[1]


def parse_phase_report(stderr: str) -> dict:
    start = stderr.find("{")
    end = stderr.rfind("}")
    if start == -1 or end == -1 or end < start:
        raise ValueError("no profiling report found in decompiler stderr")
    return json.loads(stderr[start : end + 1])


def _run(binary: Path, fixture: Path) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(
        (str(binary), str(fixture)),
        cwd=WORKSPACE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        check=False,
    )


def measure_wall_clock(binary: Path, fixture: Path, runs: int) -> list[float]:
    timings = []
    for _ in range(runs):
        started = time.perf_counter()
        completed = _run(binary, fixture)
        elapsed = time.perf_counter() - started
        if completed.returncode != 0:
            raise SystemExit(
                f"decompiler exited {completed.returncode}: "
                f"{completed.stderr.decode('utf-8', errors='replace')}"
            )
        timings.append(elapsed)
    return timings


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Measure decompiler wall clock and per-phase cost."
    )
    parser.add_argument(
        "--fixture",
        type=Path,
        default=(
            Path(os.environ["MEDAL_BIG_FIXTURE"])
            if os.environ.get("MEDAL_BIG_FIXTURE")
            else None
        ),
        help=(
            "Bytecode to measure. Defaults to $MEDAL_BIG_FIXTURE. The large "
            "capture lives outside the repository, so it is named by "
            "environment rather than committed."
        ),
    )
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument(
        "--release-binary",
        type=Path,
        default=Path("target/release/luau-lifter.exe"),
    )
    parser.add_argument(
        "--profiling-binary",
        type=Path,
        default=Path("target/release/luau-lifter.exe"),
    )
    arguments = parser.parse_args()

    if arguments.fixture is None:
        raise SystemExit(
            "no fixture: pass --fixture or set MEDAL_BIG_FIXTURE"
        )
    fixture = arguments.fixture.resolve()
    if not fixture.exists():
        raise SystemExit(f"fixture not found: {fixture}")

    timings = measure_wall_clock(
        arguments.release_binary, fixture, arguments.runs
    )
    print(f"# Decompiler measurement: {fixture.name}\n")
    print("| Run | Seconds |")
    print("| ---: | ---: |")
    for index, elapsed in enumerate(timings, start=1):
        print(f"| {index} | {elapsed:.2f} |")
    print(f"\nBest of {arguments.runs}: **{min(timings):.2f} s**\n")

    completed = _run(arguments.profiling_binary, fixture)
    try:
        report = parse_phase_report(
            completed.stderr.decode("utf-8", errors="replace")
        )
    except ValueError:
        print(
            "No phase report. Build with "
            "`cargo +nightly build --release -p luau-lifter "
            "--features profiling` and pass --profiling-binary."
        )
        return 0

    print(f"Peak live heap: **{report['peak_live_mb']} MB**\n")
    print("| Phase | Seconds | Allocated MB |")
    print("| --- | ---: | ---: |")
    for phase in sorted(
        report["phases"], key=lambda entry: entry["seconds"], reverse=True
    ):
        print(
            f"| {phase['phase']} | {phase['seconds']:.3f} "
            f"| {phase['alloc_mb']:.1f} |"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `python -m unittest tests.python.test_measure_decompiler -v`

Expected: PASS, 2 tests.

- [ ] **Step 5: Record the baseline**

Set the fixture once for the whole session. Every later task's measurement step assumes it:

```bash
export MEDAL_BIG_FIXTURE="C:/Users/Admin/Desktop/Script/captures/sUNCm0m3n7-d7140e4f7546-hardened/stage-27-reconstructed-luau-bytecode-v1/recovered.reconstructed.v9-g2.luac"
cargo +nightly build --release -p luau-lifter
python tools/measure_decompiler.py --runs 3
```

Later steps in this plan write `python tools/measure_decompiler.py --runs 3`; if `MEDAL_BIG_FIXTURE` is not set the tool exits with a message rather than measuring nothing.

Expected: best-of-three between 13.0 s and 14.0 s. If the machine is loaded and the number is higher, re-run — do not proceed with a baseline you cannot reproduce, because every later task compares against it.

- [ ] **Step 6: Commit**

```bash
git add tools/measure_decompiler.py tests/python/test_measure_decompiler.py
git commit -m "test: add decompiler wall clock and phase measurement tool"
```

---

## Task 3: Promote Six Semantic Probes To Twenty-Six

Six probes containing no register-array pattern cannot catch a bad slot fold. This lands against unmodified decompiler behaviour so the new probes establish expected output on known-good code.

**Files:**
- Create: 20 files under `tests/luau_corpus/probes/`
- Modify: `tools/luau_corpus/semantic.py:25-31` (`_PROBE_NAMES`)
- Modify: `tests/python/test_luau_corpus.py:32-47` and `:148-172`

**Interfaces:**
- Consumes: nothing.
- Produces: `TRUSTED_SEMANTIC_PROBES` covering 25 of the 26 corpus cases; `18_recursion` is exempt because Luau's `require` rejects its multi-value return.

A probe is a module returning `function(subject) ... end`. `runner.luau` calls it, `table.pack`s the results, and encodes them. **The encoder raises on function values**, so no probe may return a function or a table containing one.

- [ ] **Step 1: Write the twenty probe files**

Create each file under `tests/luau_corpus/probes/`.

`01_literals_locals.luau`:
```lua
return function(subject)
    return subject
end
```

`02_expression_precedence.luau`:
```lua
return function(subject)
    return subject(2, 3, 4), subject(-1, 5, 2), subject(0, 0, 7)
end
```

`03_parallel_assignment.luau`:
```lua
return function(subject)
    return subject(1, 2, 3)
end
```

`06_method_chains.luau`:
```lua
return function(subject)
    return subject(5, { key = "offset", offset = 3, factor = 4 })
end
```

`07_table_literals.luau`:
```lua
return function(subject)
    return subject
end
```

`08_table_incremental.luau`:
```lua
return function(subject)
    return subject("extra", 42)
end
```

`09_if_elseif_else.luau`:
```lua
return function(subject)
    return subject(1, 10), subject(10, 10), subject(50, 10), subject(-5, 10)
end
```

`10_short_circuit.luau`:
```lua
return function(subject)
    local all, any, mixed, log = subject(true, false, 7)
    return all, any, mixed, log
end
```

`11_conditional_expression.luau`:
```lua
return function(subject)
    return subject(1, 5, 9), subject(7, 5, 9), subject(12, 5, 9)
end
```

`12_while_break_continue.luau`:
```lua
return function(subject)
    return subject({ 1, -2, 3, 4, -5, 7, 9 }, 12)
end
```

`14_numeric_for.luau`:
```lua
return function(subject)
    return subject(6)
end
```

`16_closure_capture.luau`:
```lua
return function(subject)
    local transform = subject(3, 5)
    return transform(1), transform(2), transform(10)
end
```

`17_mutable_upvalue.luau`:
```lua
return function(subject)
    local counter = subject(4)
    return counter(), counter(), counter()
end
```

`18_recursion.luau` — the case ends `return factorial, isEven, isOdd`, but `require` of a multi-return module yields only the first value, so the probe receives `factorial` alone:
```lua
return function(factorial)
    return factorial(0), factorial(1), factorial(6)
end
```

`19_callback_factory.luau` — `makeCallbacks(prefix, initial)` returns `update, snapshot`; `update(label, ...)` returns `prefix .. label, total, count`:
```lua
return function(subject)
    local update, snapshot = subject("id-", 10)
    local label, runningTotal, count = update("a", 1, 2, 3)
    local prefix, current = snapshot()
    return label, runningTotal, count, prefix, current
end
```

`22_nested_early_exits.luau`:
```lua
return function(subject)
    local groups = {
        { items = { "a", "b" } },
        { disabled = true, items = { "target" } },
        { items = { "c", "target", "d" } },
    }
    return subject(groups, "target"), subject(groups, "absent")
end
```

`23_register_pressure_aliases.luau`:
```lua
return function(subject)
    local finish = subject(3, function(seed, a10, a20)
        return seed * 2, a10 + 1, a20 - 1
    end)
    return finish(0), finish(100)
end
```

`24_wonky_integration.luau`:
```lua
return function(subject)
    local built = subject(5, {
        { command = "start", value = 1 },
        { command = "add", value = 4 },
        { command = "pause", autoResume = true },
        { command = "add", value = 6 },
        { command = "stop" },
    })
    return built.state, built.total, built.history
end
```

`25_product_controller.luau`:
```lua
return function(subject)
    local controller = subject.new({
        fallback = function(context)
            return "fallback:" .. context.action
        end,
    }, { retries = 5 })

    controller:use(function(context, index)
        context.seen = index
        return context
    end)

    controller:on("ping", function(context)
        return "pong:" .. tostring(context.seen)
    end)

    local okPing, pingValue = controller:dispatch("ping", 1)
    local okMissing, missingValue = controller:dispatch("missing", 2)

    return okPing, pingValue, okMissing, missingValue, #controller.history
end
```

`26_adversarial_dataflow.luau` — `build(seed, source)` returns `api, snapshot("ready")`. Every snapshot table carries a `resume` function and `api.state` carries `callbacks`, so the probe reads scalar fields out rather than returning either table:
```lua
return function(subject)
    local api = subject(7, {
        seed = function()
            return 1, 2, 3
        end,
        transforms = {
            double = function(value)
                return value * 2
            end,
        },
    })

    local addedOk, added = api.invoke("add", 5)
    local doubledOk, doubled = api.invoke("double", 4)
    local missingOk, missingReason, missingName = api.invoke("absent")

    return addedOk,
        added.label,
        added.revision,
        added.total,
        doubledOk,
        doubled.label,
        doubled.revision,
        doubled.total,
        missingOk,
        missingReason,
        missingName,
        api.state.total,
        api.state.revision,
        api.state.values
end
```

- [ ] **Step 2: Verify every probe runs and produces a result**

```bash
for case in 01_literals_locals 02_expression_precedence 03_parallel_assignment \
  06_method_chains 07_table_literals 08_table_incremental 09_if_elseif_else \
  10_short_circuit 11_conditional_expression 12_while_break_continue \
  14_numeric_for 16_closure_capture 17_mutable_upvalue 18_recursion \
  19_callback_factory 22_nested_early_exits 23_register_pressure_aliases \
  24_wonky_integration 25_product_controller 26_adversarial_dataflow; do
  printf '%s -> ' "$case"
  ./.tools/luau-windows/luau.exe tests/luau_corpus/probes/runner.luau -a \
    "../cases/$case" "./$case" 2>&1 | tail -1
done
```

Expected: every line prints `SEMANTIC_RESULT <encoding>`. Any line showing a Luau error means the probe's assumptions about the case are wrong — read that case and correct the probe. Any line mentioning `unsupported semantic result type: function` means the probe returned a function; return plain data instead.

- [ ] **Step 3: Extend the probe manifest**

In `tools/luau_corpus/semantic.py`, replace `_PROBE_NAMES` with all 26 case names in sorted order:

```python
_PROBE_NAMES = (
    "01_literals_locals",
    "02_expression_precedence",
    "03_parallel_assignment",
    "04_calls_multireturn",
    "05_varargs",
    "06_method_chains",
    "07_table_literals",
    "08_table_incremental",
    "09_if_elseif_else",
    "10_short_circuit",
    "11_conditional_expression",
    "12_while_break_continue",
    "13_repeat_until",
    "14_numeric_for",
    "15_generic_for",
    "16_closure_capture",
    "17_mutable_upvalue",
    "18_recursion",
    "19_callback_factory",
    "20_pcall_style_flow",
    "21_state_machine",
    "22_nested_early_exits",
    "23_register_pressure_aliases",
    "24_wonky_integration",
    "25_product_controller",
    "26_adversarial_dataflow",
)
```

- [ ] **Step 4: Update the manifest test**

In `tests/python/test_luau_corpus.py`, replace the body of `test_semantic_probe_manifest_is_explicit_and_limited` with:

```python
    def test_semantic_probe_manifest_covers_every_case(self) -> None:
        workspace = Path(__file__).resolve().parents[2]
        cases = sorted(
            path.stem
            for path in (workspace / "tests" / "luau_corpus" / "cases").glob(
                "*.luau"
            )
        )

        self.assertEqual(tuple(cases), tuple(TRUSTED_SEMANTIC_PROBES))

        for name, probe in TRUSTED_SEMANTIC_PROBES.items():
            with self.subTest(case=name):
                self.assertTrue((workspace / probe.probe_path).exists())

        self.assertNotIn("27_orchestration_engine", TRUSTED_SEMANTIC_PROBES)
```

- [ ] **Step 5: Pin the literal results**

Run the loop from Step 2 again and copy each printed encoding into the `expected` dict of `test_trusted_probes_produce_literal_source_results`, keeping the six existing entries unchanged. The dict must end with 26 entries, keys in the same order as `_PROBE_NAMES`.

This pins what the *source* produces. It is the reference every later task's decompiled output is compared against, so the values must come from a clean run on unmodified code — not from a run after any decompiler change.

- [ ] **Step 6: Run the tests**

Run: `python -m unittest tests.python.test_luau_corpus -v`

Expected: PASS. `test_trusted_probes_produce_literal_source_results` runs 26 subtests.

- [ ] **Step 7: Run the full semantic corpus**

```bash
python tools/run_luau_corpus.py --profiles primary --semantic \
  --decompiler target/release/luau-lifter.exe --no-build \
  --output tests/luau_corpus/results/current
grep -c "| yes |" tests/luau_corpus/results/current/summary.md
head -4 tests/luau_corpus/results/current/summary.md
```

Expected: 0 failures, and 75 semantic matches (25 probed cases × 3 profiles).

`18_recursion` is not probed: it returns three values, and Luau's `require` rejects a module returning more than one, so `runner.luau` cannot load it. The case still compiles, decompiles, and recompiles in all 78 structural runs.

If any case reports a semantic mismatch here, **stop**. That is a pre-existing decompiler defect on unmodified `main`, not something this plan introduced. Record the case name and its diagnostic log path, remove that case from `_PROBE_NAMES` with a one-line comment naming the defect, and report it before continuing.

- [ ] **Step 8: Commit**

```bash
git add tests/luau_corpus/probes/ tools/luau_corpus/semantic.py \
  tests/python/test_luau_corpus.py
git commit -m "test: execute every corpus case as a semantic probe"
```

---

## Task 4: Formatter Blank Lines

Lowest-risk, highest-visibility change. Formatting only — it cannot alter behaviour.

**Files:**
- Modify: `ast/src/formatter.rs:115-178` (`format_block_no_indent`), `:252-258` (`format_table`)
- Test: `ast/src/formatter.rs` `mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces: `Formatter::table_renders_multiline(&Table) -> bool`, `Formatter::statement_renders_multiline(&Statement) -> bool`, both `pub(crate)`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `ast/src/formatter.rs`:

```rust
    #[test]
    fn blank_line_separates_a_multiline_statement_from_its_neighbours() {
        let counter = local("i");
        let block = Block(vec![
            Assign::new(vec![local("a").into()], vec![Literal::Number(1.0).into()]).into(),
            NumericFor::new(
                Literal::Number(1.0).into(),
                Literal::Number(2.0).into(),
                Literal::Number(1.0).into(),
                counter,
                Block(vec![
                    Assign::new(vec![local("b").into()], vec![Literal::Number(2.0).into()])
                        .into(),
                ]),
            )
            .into(),
            Assign::new(vec![local("c").into()], vec![Literal::Number(3.0).into()]).into(),
        ]);

        let formatted = block.to_string();

        assert_eq!(
            formatted,
            "a = 1\n\nfor i = 1, 2 do\n\tb = 2\nend\n\nc = 3"
        );
    }

    #[test]
    fn consecutive_single_line_statements_stay_tight() {
        let block = Block(vec![
            Assign::new(vec![local("a").into()], vec![Literal::Number(1.0).into()]).into(),
            Assign::new(vec![local("b").into()], vec![Literal::Number(2.0).into()]).into(),
            Assign::new(vec![local("c").into()], vec![Literal::Number(3.0).into()]).into(),
        ]);

        assert_eq!(block.to_string(), "a = 1\nb = 2\nc = 3");
    }

    #[test]
    fn blank_line_precedes_a_return_that_follows_other_work() {
        let block = Block(vec![
            Assign::new(vec![local("a").into()], vec![Literal::Number(1.0).into()]).into(),
            Return::new(vec![local("a").into()]).into(),
        ]);

        assert_eq!(block.to_string(), "a = 1\n\nreturn a");
    }

    #[test]
    fn a_lone_return_gains_no_leading_blank_line() {
        let block = Block(vec![Return::new(vec![local("a").into()]).into()]);

        assert_eq!(block.to_string(), "return a");
    }

    #[test]
    fn empty_closure_value_is_not_treated_as_multiline() {
        let closure = Closure {
            function: ByAddress(Arc::new(Mutex::new(Function::default()))),
            upvalues: Vec::new(),
        };
        let block = Block(vec![
            Assign::new(vec![local("f").into()], vec![closure.into()]).into(),
            Assign::new(vec![local("g").into()], vec![Literal::Number(1.0).into()]).into(),
        ]);

        assert_eq!(block.to_string(), "f = function() end\ng = 1");
    }
```

Adjust the constructor calls in these tests to match the real signatures of `NumericFor::new` and `Return::new` in `ast/src/for.rs` and `ast/src/return.rs`. Read those files first. Add any missing imports to `mod tests`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo +nightly test -p ast blank_line`

Expected: FAIL — the produced strings contain no `\n\n`.

- [ ] **Step 3: Extract the shared table predicate**

In `ast/src/formatter.rs`, add above `format_table`:

```rust
    /// Whether `format_table` will place this table's fields on their own
    /// lines.
    ///
    /// The blank-line predicate and the table writer must agree, so both read
    /// this one definition rather than each deciding for themselves.
    pub(crate) fn table_renders_multiline(table: &Table) -> bool {
        let compacted = table.without_shadowed_literal_fields();
        let sequential_keys = Self::are_table_keys_sequential(&compacted);
        !compacted.0.is_empty() && (!sequential_keys || compacted.0.len() > 3)
            || Self::contains_table(&compacted)
    }
```

Then change `format_table` to use it, replacing its `should_format` binding:

```rust
        let should_format = Self::table_renders_multiline(&compacted);
```

Leave `let compacted = table.without_shadowed_literal_fields();` and the `let table = &compacted;` line that follows it exactly as they are.

- [ ] **Step 4: Add the multi-line and blank-line predicates**

Add above `format_block_no_indent`:

```rust
    fn value_renders_multiline(value: &RValue) -> bool {
        match value {
            RValue::Closure(closure) => !closure.function.lock().body.is_empty(),
            RValue::Table(table) => Self::table_renders_multiline(table),
            _ => false,
        }
    }

    pub(crate) fn statement_renders_multiline(statement: &Statement) -> bool {
        match statement {
            Statement::If(_)
            | Statement::While(_)
            | Statement::Repeat(_)
            | Statement::NumericFor(_)
            | Statement::GenericFor(_) => true,
            Statement::Assign(assign) => {
                assign.right.iter().any(Self::value_renders_multiline)
            }
            _ => false,
        }
    }

    fn is_declaration(statement: &Statement) -> bool {
        matches!(statement, Statement::Assign(assign) if assign.prefix)
    }

    /// Whether a blank line belongs between two adjacent statements.
    ///
    /// `declaration_run` is the number of declarations immediately preceding
    /// `next`, so a block of locals can be separated from the work that uses
    /// them without splitting the block itself.
    fn needs_blank_between(
        previous: &Statement,
        next: &Statement,
        declaration_run: usize,
    ) -> bool {
        if matches!(previous, Statement::Comment(_) | Statement::Empty(_))
            || matches!(next, Statement::Comment(_) | Statement::Empty(_))
        {
            return false;
        }
        if Self::statement_renders_multiline(previous)
            || Self::statement_renders_multiline(next)
        {
            return true;
        }
        if matches!(next, Statement::Return(_)) {
            return true;
        }
        declaration_run >= 2 && !Self::is_declaration(next)
    }
```

- [ ] **Step 5: Emit the blank lines**

Change the head of the loop in `format_block_no_indent` from:

```rust
        for (i, statement) in block.iter().enumerate() {
            if i != 0 {
                writeln!(self.output)?;
            }
```

to:

```rust
        let mut declaration_run = 0usize;
        for (i, statement) in block.iter().enumerate() {
            if i != 0 {
                writeln!(self.output)?;
                if Self::needs_blank_between(
                    &block[i - 1],
                    statement,
                    declaration_run,
                ) {
                    writeln!(self.output)?;
                }
            }
            declaration_run = if Self::is_declaration(statement) {
                declaration_run + 1
            } else {
                0
            };
```

Leave the rest of the loop body, including the `disambiguate` logic that appends `;`, untouched. The `;` is written before the newline of the following iteration, so it stays attached to its own statement.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo +nightly test -p ast`

Expected: PASS. Several pre-existing formatter tests that assert exact output will now fail because their expected strings lack the new blank lines. Update each expected string to include them — the test's intent is the formatting shape, and the shape changed deliberately. Do not weaken an assertion to make it pass.

- [ ] **Step 7: Verify against the corpus**

```bash
cargo +nightly build --release -p luau-lifter
python tools/run_luau_corpus.py --profiles primary --semantic \
  --decompiler target/release/luau-lifter.exe --no-build \
  --output tests/luau_corpus/results/current
head -4 tests/luau_corpus/results/current/summary.md
sed -n '1,40p' tests/luau_corpus/results/current/O2_g1/25_product_controller.luau
```

Expected: 0 failures, 75 semantic matches, and the `blank` column now non-zero for every case. Read the case 25 output and confirm it looks like hand-written Lua — functions separated, declaration runs kept together.

- [ ] **Step 8: Check the performance ceiling**

```bash
python tools/measure_decompiler.py --runs 3
```

Expected: best-of-three under 16.0 s. The `format` phase grows; it must stay under 1.5 s.

- [ ] **Step 9: Commit**

```bash
git add ast/src/formatter.rs
git commit -m "feat: separate decompiled statements with blank lines"
```

---

## Task 5: Shadow-Free Local Naming

`name_closure` builds a fresh `Namer` with `counter: 1` and a `used_names` set holding only upvalue names, so an inner `v6` can shadow an enclosing `v6`. Replace the flat set with a scope stack.

**Files:**
- Modify: `ast/src/name_locals.rs:31-36` (`struct Namer`), `:293-314`, `:390-417`, `:449-456`
- Test: `ast/src/name_locals.rs` `mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces: unchanged public API — `name_locals(block: &mut Block, rename: bool)`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `ast/src/name_locals.rs`:

```rust
    #[test]
    fn inner_scope_never_shadows_a_visible_outer_name() {
        let outer = local(None);
        let inner = local(None);
        let mut inner_body = Block(vec![
            declaration(&inner, Literal::Number(2.0).into()).into(),
            crate::Return::new(vec![inner.clone().into()]).into(),
        ]);
        let closure = Closure {
            function: ByAddress(Arc::new(Mutex::new(Function {
                name: None,
                parameters: Vec::new(),
                is_variadic: false,
                is_method: false,
                body: std::mem::take(&mut inner_body),
            }))),
            upvalues: Vec::new(),
        };
        let holder = local(None);
        let mut block = Block(vec![
            declaration(&outer, Literal::Number(1.0).into()).into(),
            declaration(&holder, closure.into()).into(),
            crate::Return::new(vec![outer.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_ne!(local_name(&outer), local_name(&inner));
    }

    #[test]
    fn sibling_scopes_may_reuse_a_name() {
        let first = local(None);
        let second = local(None);
        let mut left = Block(vec![declaration(&first, Literal::Number(1.0).into()).into()]);
        let mut right = Block(vec![declaration(&second, Literal::Number(2.0).into()).into()]);
        let make = |body: &mut Block| Closure {
            function: ByAddress(Arc::new(Mutex::new(Function {
                name: None,
                parameters: Vec::new(),
                is_variadic: false,
                is_method: false,
                body: std::mem::take(body),
            }))),
            upvalues: Vec::new(),
        };
        let left_holder = local(None);
        let right_holder = local(None);
        let mut block = Block(vec![
            declaration(&left_holder, make(&mut left).into()).into(),
            declaration(&right_holder, make(&mut right).into()).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&first), local_name(&second));
    }
```

Read the existing helpers `local`, `local_name`, and `declaration` at `ast/src/name_locals.rs:471-484` and match their signatures exactly.

- [ ] **Step 2: Run the tests to verify the first one fails**

Run: `cargo +nightly test -p ast shadow`

Expected: `inner_scope_never_shadows_a_visible_outer_name` FAILS — both locals are named `v1`.

- [ ] **Step 3: Replace the flat name set with a scope stack**

Change the struct:

```rust
struct Namer {
    rename: bool,
    counter: usize,
    /// One frame per enclosing function. A name is taken if any frame holds
    /// it, so an inner binding cannot shadow one that is still visible.
    scopes: Vec<FxHashSet<String>>,
}
```

Add the two accessors and rewrite the name allocators:

```rust
impl Namer {
    fn is_taken(&self, name: &str) -> bool {
        self.scopes.iter().any(|scope| scope.contains(name))
    }

    fn claim(&mut self, name: String) -> bool {
        if self.is_taken(&name) {
            return false;
        }
        self.scopes
            .last_mut()
            .expect("namer always has an open scope")
            .insert(name);
        true
    }

    fn unique_name(&mut self, base: &str) -> String {
        if self.claim(base.to_owned()) {
            return base.to_owned();
        }
        for suffix in 2.. {
            let name = format!("{base}{suffix}");
            if self.claim(name.clone()) {
                return name;
            }
        }
        unreachable!()
    }

    fn fallback_name(&mut self, prefix: &str) -> String {
        loop {
            let name = format!("{prefix}{}", self.counter);
            self.counter += 1;
            if self.claim(name.clone()) {
                return name;
            }
        }
    }
```

- [ ] **Step 4: Make closure naming reuse the outer namer**

Replace `name_closure` with a `&mut self` version that pushes and pops a frame:

```rust
    fn name_closure(&mut self, closure: &crate::Closure) {
        let upvalue_names = closure
            .upvalues
            .iter()
            .filter_map(|upvalue| {
                let local = match upvalue {
                    crate::Upvalue::Copy(local) | crate::Upvalue::Ref(local) => local,
                };
                local
                    .0
                    .0
                    .lock()
                    .0
                    .clone()
                    .filter(|name| is_valid_identifier(name.as_bytes()))
            })
            .collect::<FxHashSet<_>>();
        let mut function = closure.function.lock();
        if function.is_method && self.is_taken("self") {
            function.is_method = false;
        }

        let outer_counter = std::mem::replace(&mut self.counter, 1);
        self.scopes.push(upvalue_names);
        self.name_function(&mut function);
        self.scopes.pop();
        self.counter = outer_counter;
    }
```

The `is_method` check changes from `used_names.contains("self")` to `self.is_taken("self")`, which now consults every enclosing frame rather than only the upvalue names. That is the same question asked correctly.

Popping the frame on exit is what lets sibling closures reuse short names: their scopes are disjoint, so reuse is not shadowing. Resetting `counter` per frame keeps generated numbers small instead of climbing into the thousands across a large chunk.

- [ ] **Step 5: Update the entry point**

```rust
pub fn name_locals(block: &mut Block, rename: bool) {
    Namer {
        rename,
        counter: 1,
        scopes: vec![FxHashSet::default()],
    }
    .name_scope(block, &[]);
}
```

Fix the resulting compile errors: `name_child_functions` calls `self.name_closure(closure)` inside a `traverse_rvalues` closure that borrows `self` — restructure it to collect the closures into a `Vec` first, then name them:

```rust
    fn name_child_functions(&mut self, block: &mut Block) {
        for statement in &mut block.0 {
            let mut children = Vec::new();
            statement.traverse_rvalues(&mut |value| {
                if let RValue::Closure(closure) = value {
                    children.push(closure.clone());
                }
            });
            for closure in &children {
                self.name_closure(closure);
            }
            match statement {
```

Leave the rest of `name_child_functions` unchanged. `Closure` is `Clone` and holds an `Arc<Mutex<Function>>`, so the clone shares the same function and naming still lands on the real body.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo +nightly test -p ast`

Expected: PASS. Existing tests that assert specific generated names may need their expectations updated where a name legitimately moved from `v1` to `v2`; verify each change is a shadowing fix before accepting it.

- [ ] **Step 7: Verify against the corpus and measure**

```bash
cargo +nightly build --release -p luau-lifter
python tools/run_luau_corpus.py --profiles primary --semantic \
  --decompiler target/release/luau-lifter.exe --no-build \
  --output tests/luau_corpus/results/current
head -4 tests/luau_corpus/results/current/summary.md
python tools/measure_decompiler.py --runs 3
```

Expected: 0 failures, 75 semantic matches, wall clock under 16.0 s.

- [ ] **Step 8: Commit**

```bash
git add ast/src/name_locals.rs
git commit -m "feat: keep generated local names free of shadowing"
```

---

## Task 6: Shape-Driven Fallback Names

Replace `v1`/`p1` with names carrying the initializer's shape.

**Files:**
- Modify: `ast/src/name_locals.rs:9-20` (`struct Evidence`), `:152-291` (`collect_evidence`), `:316-342` (`assign_name`)
- Test: `ast/src/name_locals.rs` `mod tests`

**Interfaces:**
- Consumes: `Namer::unique_name`, `Namer::fallback_name` from Task 5.
- Produces: `Evidence` gains `shape: Option<&'static str>`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests`:

```rust
    #[test]
    fn table_initializer_indexed_in_a_loop_is_named_for_its_shape() {
        let registers = local(None);
        let counter = local(None);
        let body = Block(vec![
            Assign::new(
                vec![
                    crate::Index::new(registers.clone().into(), counter.clone().into()).into(),
                ],
                vec![Literal::Number(1.0).into()],
            )
            .into(),
        ]);
        let mut block = Block(vec![
            declaration(&registers, crate::Table::default().into()).into(),
            crate::NumericFor::new(
                Literal::Number(1.0).into(),
                Literal::Number(4.0).into(),
                Literal::Number(1.0).into(),
                counter,
                body,
            )
            .into(),
            crate::Return::new(vec![registers.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&registers), "registers");
    }

    #[test]
    fn string_initializer_is_named_text() {
        let message = local(None);
        let mut block = Block(vec![
            declaration(&message, Literal::String(b"hello".to_vec()).into()).into(),
            crate::Return::new(vec![message.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&message), "text");
    }

    #[test]
    fn a_local_with_no_inferable_shape_is_named_value_not_v1() {
        let unknown = local(None);
        let source = local(Some("source"));
        let mut block = Block(vec![
            declaration(
                &unknown,
                crate::Index::new(source.clone().into(), Literal::Number(1.0).into()).into(),
            )
            .into(),
            crate::Return::new(vec![unknown.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&unknown), "value");
    }
```

Match the real constructor signatures for `Index`, `Table`, `NumericFor`, and `Return` by reading their modules.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo +nightly test -p ast named_for_its_shape named_text named_value`

Expected: FAIL — names come back as `v1`.

- [ ] **Step 3: Record the shape**

Add to `struct Evidence`:

```rust
    /// The name suggested by the initializer's form, used only when no
    /// stronger evidence names this local.
    shape: Option<&'static str>,
    /// Written through a computed index, which distinguishes an array used as
    /// storage from a record with fixed fields.
    computed_index_writes: usize,
    constant_index_writes: usize,
```

Add a classifier next to `Evidence`:

```rust
fn initializer_shape(value: &RValue) -> Option<&'static str> {
    match value {
        RValue::Closure(_) => Some("handler"),
        RValue::Literal(crate::Literal::String(_)) => Some("text"),
        RValue::Binary(binary)
            if matches!(binary.operation, crate::BinaryOperation::Concat) =>
        {
            Some("text")
        }
        RValue::Unary(unary)
            if matches!(unary.operation, crate::UnaryOperation::Length) =>
        {
            Some("count")
        }
        RValue::Table(table) if table.0.is_empty() => Some("slots"),
        RValue::Table(table) => {
            let all_string_keys = table.0.iter().all(|(key, _)| {
                matches!(key, Some(RValue::Literal(crate::Literal::String(_))))
            });
            Some(if all_string_keys { "record" } else { "slots" })
        }
        RValue::Call(_) | RValue::Select(crate::Select::Call(_)) => Some("result"),
        RValue::MethodCall(_) => Some("result"),
        RValue::Index(_) | RValue::Global(_) => Some("value"),
        _ => None,
    }
}
```

Read `ast/src/binary.rs` and `ast/src/unary.rs` to confirm the enum names `BinaryOperation::Concat` and `UnaryOperation::Length`, and correct them if they differ.

- [ ] **Step 4: Populate the shape during evidence collection**

In `collect_evidence`, inside the existing `Statement::Assign(assign)` arm, within the block already guarded by `if assign.prefix && let ([LValue::Local(local)], [value]) = ...`, add:

```rust
                        if let Some(shape) = initializer_shape(value) {
                            evidence.entry(local.clone()).or_default().shape = Some(shape);
                        }
```

In the same `Statement::Assign` arm but outside that guard, record index writes so a plain `{}` can be distinguished later:

```rust
                    if let [LValue::Index(index)] = assign.left.as_slice()
                        && let RValue::Local(container) = index.left.as_ref()
                    {
                        let container_evidence =
                            evidence.entry(container.clone()).or_default();
                        if matches!(index.right.as_ref(), RValue::Literal(_)) {
                            container_evidence.constant_index_writes += 1;
                        } else {
                            container_evidence.computed_index_writes += 1;
                        }
                    }
```

- [ ] **Step 5: Promote empty tables written through computed indexes**

In `name_scope`, inside the existing `for evidence in evidence.values_mut()` loop, add:

```rust
            if evidence.shape == Some("slots") && evidence.computed_index_writes > 0 {
                evidence.shape = Some("registers");
            }
```

- [ ] **Step 6: Use the shape in name assignment**

Replace the `name` computation in `assign_name`:

```rust
        let shape = evidence.and_then(|evidence| evidence.shape);

        let name = if let Some(name) =
            existing.or_else(|| structural_name.cloned()).or(field_name)
        {
            self.unique_name(&name)
        } else if let Some(role) = inferred {
            self.unique_name(role)
        } else if unused {
            "_".to_owned()
        } else if let Some(shape) = shape {
            self.unique_name(shape)
        } else {
            self.fallback_name(prefix)
        };
```

`fallback_name` stays as the last resort for locals with no initializer at all, such as a parameter with no evidence. Parameters keep the `p` prefix in that case, which is honest: nothing is known about them.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo +nightly test -p ast`

Expected: PASS. Update existing name expectations where a local legitimately gained a shape name.

- [ ] **Step 8: Verify against the corpus and measure**

```bash
cargo +nightly build --release -p luau-lifter
python tools/run_luau_corpus.py --profiles primary --semantic \
  --decompiler target/release/luau-lifter.exe --no-build \
  --output tests/luau_corpus/results/current
head -4 tests/luau_corpus/results/current/summary.md
python tools/measure_decompiler.py --runs 3
```

Expected: 0 failures, 75 semantic matches, wall clock under 16.0 s. The `vN` column in the summary should drop substantially.

- [ ] **Step 9: Commit**

```bash
git add ast/src/name_locals.rs
git commit -m "feat: name locals from initializer shape instead of a counter"
```

---

## Task 7: Library Return Names

**Files:**
- Modify: `ast/src/name_locals.rs`
- Test: `ast/src/name_locals.rs` `mod tests`

**Interfaces:**
- Consumes: `initializer_shape` from Task 6.
- Produces: `library_return_name(value: &RValue) -> Option<&'static str>`.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn table_pack_result_is_named_packed() {
        let packed = local(None);
        let call = crate::Call::new(
            crate::Index::new(
                crate::Global::new(b"table".to_vec()).into(),
                Literal::String(b"pack".to_vec()).into(),
            )
            .into(),
            Vec::new(),
        );
        let mut block = Block(vec![
            declaration(&packed, call.into()).into(),
            crate::Return::new(vec![packed.clone().into()]).into(),
        ]);

        name_locals(&mut block, false);

        assert_eq!(local_name(&packed), "packed");
    }
```

Read `ast/src/global.rs` for the real `Global` constructor and adjust.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo +nightly test -p ast named_packed`

Expected: FAIL — the name is `result`, from the Task 6 call shape.

- [ ] **Step 3: Add the lookup table**

```rust
/// Names for values whose producing call fixes their meaning.
///
/// Only entries whose name is certain belong here. A wrong name is worse
/// than a generic one, because it asserts something false about the code.
const LIBRARY_RETURN_NAMES: &[(&[u8], &[u8], &str)] = &[
    (b"table", b"pack", "packed"),
    (b"table", b"create", "buffer"),
    (b"table", b"concat", "text"),
    (b"string", b"format", "text"),
    (b"string", b"rep", "text"),
    (b"coroutine", b"create", "thread"),
    (b"os", b"clock", "started"),
];

fn library_return_name(value: &RValue) -> Option<&'static str> {
    let call = match value {
        RValue::Call(call) | RValue::Select(crate::Select::Call(call)) => call,
        _ => return None,
    };
    match call.value.as_ref() {
        RValue::Index(index) => {
            let RValue::Global(namespace) = index.left.as_ref() else {
                return None;
            };
            let RValue::Literal(crate::Literal::String(member)) = index.right.as_ref() else {
                return None;
            };
            LIBRARY_RETURN_NAMES
                .iter()
                .find(|(space, name, _)| {
                    *space == namespace.name() && name == &member.as_slice()
                })
                .map(|(_, _, label)| *label)
        }
        RValue::Global(global) if global.name() == b"setmetatable" => Some("object"),
        RValue::Global(global) if global.name() == b"pcall" => Some("ok"),
        _ => None,
    }
}
```

- [ ] **Step 4: Consult it before the generic shape**

In `initializer_shape`, make the call arms defer to it:

```rust
        RValue::Call(_) | RValue::Select(crate::Select::Call(_)) => {
            Some(library_return_name(value).unwrap_or("result"))
        }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo +nightly test -p ast`

Expected: PASS.

- [ ] **Step 6: Verify against the corpus and commit**

```bash
cargo +nightly build --release -p luau-lifter
python tools/run_luau_corpus.py --profiles primary --semantic \
  --decompiler target/release/luau-lifter.exe --no-build \
  --output tests/luau_corpus/results/current
head -4 tests/luau_corpus/results/current/summary.md
git add ast/src/name_locals.rs
git commit -m "feat: name locals from the library call that produced them"
```

---

## Task 8: Callee-Parameter Name Propagation

A local passed to `push_stack(stack, value)` should be called `stack`. Runs after linking, when every function body is in one tree.

**Files:**
- Create: `ast/src/name_flow.rs`
- Modify: `ast/src/lib.rs` (module registration), `luau-lifter/src/lib.rs:619-621`
- Test: `ast/src/name_flow.rs` `mod tests`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `pub fn propagate_parameter_names(block: &mut Block)`.

The pass writes proposed names into each local's debug-name slot. `name_locals(block, false)` then honours them through the `existing` branch of `assign_name`, which already uniquifies and validates. No change to `name_locals` is needed.

- [ ] **Step 1: Write the failing test**

Create `ast/src/name_flow.rs` with only the test module and a stub:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argument_takes_the_name_of_the_parameter_it_feeds() {
        // local pushValue = function(stack, value) end
        // local anonymous = {}
        // pushValue(anonymous, 1)
        let stack_parameter = local(Some("stack"));
        let value_parameter = local(Some("value"));
        let callee = local(Some("pushValue"));
        let argument = local(None);

        let closure = Closure {
            function: ByAddress(Arc::new(Mutex::new(Function {
                name: None,
                parameters: vec![stack_parameter, value_parameter],
                is_variadic: false,
                is_method: false,
                body: Block::default(),
            }))),
            upvalues: Vec::new(),
        };

        let mut block = Block(vec![
            declaration(&callee, closure.into()).into(),
            declaration(&argument, Table::default().into()).into(),
            Statement::Call(Call::new(
                callee.clone().into(),
                vec![argument.clone().into(), Literal::Number(1.0).into()],
            )),
        ]);

        propagate_parameter_names(&mut block);

        assert_eq!(argument.0.0.lock().0.as_deref(), Some("stack"));
    }

    #[test]
    fn conflicting_call_sites_leave_the_local_unnamed() {
        let first_callee = local(Some("first"));
        let second_callee = local(Some("second"));
        let argument = local(None);

        let make = |parameter_name: &str| Closure {
            function: ByAddress(Arc::new(Mutex::new(Function {
                name: None,
                parameters: vec![local(Some(parameter_name))],
                is_variadic: false,
                is_method: false,
                body: Block::default(),
            }))),
            upvalues: Vec::new(),
        };

        let mut block = Block(vec![
            declaration(&first_callee, make("stack").into()).into(),
            declaration(&second_callee, make("registry").into()).into(),
            Statement::Call(Call::new(
                first_callee.clone().into(),
                vec![argument.clone().into()],
            )),
            Statement::Call(Call::new(
                second_callee.clone().into(),
                vec![argument.clone().into()],
            )),
        ]);

        propagate_parameter_names(&mut block);

        assert_eq!(argument.0.0.lock().0, None);
    }

    #[test]
    fn a_local_that_already_has_a_name_is_left_alone() {
        let callee = local(Some("pushValue"));
        let argument = local(Some("existing"));

        let closure = Closure {
            function: ByAddress(Arc::new(Mutex::new(Function {
                name: None,
                parameters: vec![local(Some("stack"))],
                is_variadic: false,
                is_method: false,
                body: Block::default(),
            }))),
            upvalues: Vec::new(),
        };

        let mut block = Block(vec![
            declaration(&callee, closure.into()).into(),
            Statement::Call(Call::new(
                callee.clone().into(),
                vec![argument.clone().into()],
            )),
        ]);

        propagate_parameter_names(&mut block);

        assert_eq!(argument.0.0.lock().0.as_deref(), Some("existing"));
    }
}
```

Copy the `local` and `declaration` helpers from `ast/src/name_locals.rs:471-484` into this test module, and add the imports those tests need.

- [ ] **Step 2: Run the tests to verify they fail**

Register the module first — in `ast/src/lib.rs`, add `mod name_flow;` beside the other `mod` declarations and `pub use name_flow::*;` beside the other re-exports.

Run: `cargo +nightly test -p ast name_flow`

Expected: FAIL to compile — `propagate_parameter_names` is not defined.

- [ ] **Step 3: Implement the pass**

Write the implementation above the test module in `ast/src/name_flow.rs`:

```rust
//! Carries parameter names backwards to the arguments that feed them.
//!
//! A local passed as the first argument of `push_stack(stack, value)` is a
//! stack. The callee already states that; this pass moves the statement to
//! where a reader needs it.
//!
//! Names are written into each local's debug-name slot, so `name_locals`
//! picks them up through its existing path and applies its own uniqueness and
//! validity rules. This pass never renames a local that already has a name.

use rustc_hash::FxHashMap;

use crate::{
    Block, Call, Closure, LValue, RValue, RcLocal, Statement, Traverse, is_valid_identifier,
};

/// A name proposed for a local, or a marker that call sites disagreed.
enum Proposal {
    Single(String),
    Conflicting,
}

fn parameter_names(closure: &Closure) -> Vec<Option<String>> {
    closure
        .function
        .lock()
        .parameters
        .iter()
        .map(|parameter| {
            parameter
                .0
                .0
                .lock()
                .0
                .clone()
                .filter(|name| is_valid_identifier(name.as_bytes()))
        })
        .collect()
}

fn collect_callees(block: &Block, callees: &mut FxHashMap<RcLocal, Vec<Option<String>>>) {
    for statement in &block.0 {
        if let Statement::Assign(assign) = statement
            && let ([LValue::Local(target)], [RValue::Closure(closure)]) =
                (assign.left.as_slice(), assign.right.as_slice())
        {
            callees.insert(target.clone(), parameter_names(closure));
        }
        for_each_child_block(statement, &mut |child| collect_callees(child, callees));
    }
}

fn record_call(
    call: &Call,
    callees: &FxHashMap<RcLocal, Vec<Option<String>>>,
    proposals: &mut FxHashMap<RcLocal, Proposal>,
) {
    let RValue::Local(callee) = call.value.as_ref() else {
        return;
    };
    let Some(parameters) = callees.get(callee) else {
        return;
    };
    for (argument, parameter) in call.arguments.iter().zip(parameters) {
        let (RValue::Local(argument), Some(name)) = (argument, parameter) else {
            continue;
        };
        if argument.0.0.lock().0.is_some() {
            continue;
        }
        match proposals.get(argument) {
            None => {
                proposals.insert(argument.clone(), Proposal::Single(name.clone()));
            }
            Some(Proposal::Single(existing)) if existing == name => {}
            Some(Proposal::Single(_)) => {
                proposals.insert(argument.clone(), Proposal::Conflicting);
            }
            Some(Proposal::Conflicting) => {}
        }
    }
}

fn collect_proposals(
    block: &Block,
    callees: &FxHashMap<RcLocal, Vec<Option<String>>>,
    proposals: &mut FxHashMap<RcLocal, Proposal>,
) {
    for statement in &block.0 {
        if let Statement::Call(call) = statement {
            record_call(call, callees, proposals);
        }
        for value in statement.rvalues() {
            record_rvalue(value, callees, proposals);
        }
        for_each_child_block(statement, &mut |child| {
            collect_proposals(child, callees, proposals)
        });
    }
}

fn record_rvalue(
    value: &RValue,
    callees: &FxHashMap<RcLocal, Vec<Option<String>>>,
    proposals: &mut FxHashMap<RcLocal, Proposal>,
) {
    if let RValue::Call(call) | RValue::Select(crate::Select::Call(call)) = value {
        record_call(call, callees, proposals);
    }
    for child in value.rvalues() {
        record_rvalue(child, callees, proposals);
    }
}

/// Visits every block nested inside a statement, including closure bodies.
///
/// Closure bodies matter most: a helper is declared once at the top level
/// and called from inside other functions, so skipping closure bodies would
/// miss nearly every call site worth naming.
fn for_each_child_block(statement: &Statement, visit: &mut impl FnMut(&Block)) {
    match statement {
        Statement::If(r#if) => {
            visit(&r#if.then_block.lock());
            visit(&r#if.else_block.lock());
        }
        Statement::While(r#while) => visit(&r#while.block.lock()),
        Statement::Repeat(repeat) => visit(&repeat.block.lock()),
        Statement::NumericFor(numeric_for) => visit(&numeric_for.block.lock()),
        Statement::GenericFor(generic_for) => visit(&generic_for.block.lock()),
        _ => {}
    }

    for value in statement.rvalues() {
        visit_closure_bodies(value, visit);
    }
}

/// Visits the body of every closure reachable from an expression.
///
/// `Closure` holds `Arc<Mutex<Function>>` and the body is a plain `Block`
/// field inside it, so the lock is taken here and the borrow handed straight
/// to the visitor.
fn visit_closure_bodies(value: &RValue, visit: &mut impl FnMut(&Block)) {
    if let RValue::Closure(closure) = value {
        let function = closure.function.lock();
        visit(&function.body);
    }
    for child in value.rvalues() {
        visit_closure_bodies(child, visit);
    }
}

/// Names unnamed locals after the parameters they are passed to.
///
/// Runs before `name_locals`, which applies uniqueness and scoping to
/// whatever this leaves behind.
pub fn propagate_parameter_names(block: &mut Block) {
    let mut callees = FxHashMap::default();
    collect_callees(block, &mut callees);
    if callees.is_empty() {
        return;
    }

    let mut proposals = FxHashMap::default();
    collect_proposals(block, &callees, &mut proposals);

    for (local, proposal) in proposals {
        if let Proposal::Single(name) = proposal {
            let mut slot = local.0.0.lock();
            if slot.0.is_none() {
                slot.0 = Some(name);
            }
        }
    }
}
```

Recursion into closure bodies is what makes this pass useful: helpers are declared once at the top level and called from inside other functions. `RcLocal` identity is by address, not by name, so a callee resolved inside a nested body is the same map entry as the one declared outside it — no scope tracking is needed.

Add a test proving the recursion works:

```rust
    #[test]
    fn a_call_inside_a_closure_body_still_proposes_a_name() {
        let stack_parameter = local(Some("stack"));
        let callee = local(Some("pushValue"));
        let argument = local(None);

        let helper = Closure {
            function: ByAddress(Arc::new(Mutex::new(Function {
                name: None,
                parameters: vec![stack_parameter],
                is_variadic: false,
                is_method: false,
                body: Block::default(),
            }))),
            upvalues: Vec::new(),
        };

        let caller = Closure {
            function: ByAddress(Arc::new(Mutex::new(Function {
                name: None,
                parameters: Vec::new(),
                is_variadic: false,
                is_method: false,
                body: Block(vec![
                    declaration(&argument, Table::default().into()).into(),
                    Statement::Call(Call::new(
                        callee.clone().into(),
                        vec![argument.clone().into()],
                    )),
                ]),
            }))),
            upvalues: Vec::new(),
        };

        let holder = local(Some("caller"));
        let mut block = Block(vec![
            declaration(&callee, helper.into()).into(),
            declaration(&holder, caller.into()).into(),
        ]);

        propagate_parameter_names(&mut block);

        assert_eq!(argument.0.0.lock().0.as_deref(), Some("stack"));
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo +nightly test -p ast name_flow`

Expected: PASS, 3 tests.

- [ ] **Step 5: Call the pass from the pipeline**

In `luau-lifter/src/lib.rs`, inside the `catch_phase(DecompilePhase::Format, ...)` closure, insert between `recover_function_syntax` and `name_locals`:

```rust
        ast::recover_function_syntax(&mut body);
        profiling::checkpoint("function-syntax-recovered");
        ast::propagate_parameter_names(&mut body);
        profiling::checkpoint("parameter-names-propagated");
        name_locals(&mut body, false);
```

- [ ] **Step 6: Verify against the corpus and measure**

```bash
cargo +nightly build --release -p luau-lifter
python tools/run_luau_corpus.py --profiles primary --semantic \
  --decompiler target/release/luau-lifter.exe --no-build \
  --output tests/luau_corpus/results/current
head -4 tests/luau_corpus/results/current/summary.md
python tools/measure_decompiler.py --runs 3
```

Expected: 0 failures, 75 semantic matches, `format` phase under 1.5 s, wall clock under 16.0 s.

- [ ] **Step 7: Commit**

```bash
git add ast/src/name_flow.rs ast/src/lib.rs luau-lifter/src/lib.rs
git commit -m "feat: name call arguments after the parameters they feed"
```

---

## Task 9: Corpus Cases For Slot-Folding Preconditions

Written and passing **before** folding exists, so they establish expected behaviour on known-good output. Each case is deliberately built so a wrong fold changes printed values.

**Files:**
- Create: `tests/luau_corpus/cases/27_register_array_vm.luau` through `32_slot_across_control_flow.luau`
- Create: matching probes under `tests/luau_corpus/probes/`
- Modify: `tools/luau_corpus/semantic.py` (`_PROBE_NAMES`)

**Interfaces:**
- Consumes: the 25-entry `_PROBE_NAMES` from Task 3.
- Produces: a 31-entry `_PROBE_NAMES`.

- [ ] **Step 1: Write the six cases**

`27_register_array_vm.luau` — the pattern folding must handle:

```lua
local function execute(input)
    local registers = {}
    registers[1] = input
    registers[2] = math.floor
    registers[3] = registers[1] / 2
    registers[3] = registers[2](registers[3])
    registers[4] = registers[3] + registers[1]
    return registers[3], registers[4]
end

return execute
```

`28_escaping_slot_table.luau` — a call sits between write and read:

```lua
local function observe(slots)
    slots[2] = slots[2] + 100
    return slots
end

local function execute(input)
    local registers = {}
    registers[1] = input
    registers[2] = input * 2
    observe(registers)
    registers[3] = registers[2] + 1
    return registers[2], registers[3]
end

return execute
```

`29_slot_metatable.luau` — `__index` and `__newindex` intercept:

```lua
local function execute(input)
    local backing = {}
    local registers = setmetatable({}, {
        __index = function(_, key)
            return (backing[key] or 0) + 1000
        end,
        __newindex = function(_, key, value)
            backing[key] = value * 2
        end,
    })
    registers[1] = input
    registers[2] = registers[1] + 1
    return registers[1], registers[2], backing[1], backing[2]
end

return execute
```

`30_aliased_slot_write.luau` — two names for one table:

```lua
local function execute(input)
    local registers = {}
    local alias = registers
    registers[1] = input
    alias[1] = input + 50
    registers[2] = registers[1] * 2
    return registers[1], registers[2], alias[2]
end

return execute
```

`31_nonconstant_slot_key.luau` — a computed key may alias a constant one:

```lua
local function execute(input, index)
    local registers = {}
    registers[1] = input
    registers[index] = input + 7
    registers[2] = registers[1] + 1
    return registers[1], registers[2], registers[3]
end

return execute
```

`32_slot_across_control_flow.luau` — write in a branch, read after the join:

```lua
local function execute(input, flag)
    local registers = {}
    registers[1] = input

    if flag then
        registers[2] = input * 3
    else
        registers[2] = input - 3
    end

    registers[3] = registers[2] + registers[1]
    return registers[1], registers[2], registers[3]
end

return execute
```

- [ ] **Step 2: Write the six probes**

`tests/luau_corpus/probes/27_register_array_vm.luau`:
```lua
return function(subject)
    return subject(9), subject(-4), subject(0)
end
```

`28_escaping_slot_table.luau`:
```lua
return function(subject)
    return subject(5), subject(-2)
end
```

`29_slot_metatable.luau`:
```lua
return function(subject)
    return subject(3), subject(11)
end
```

`30_aliased_slot_write.luau`:
```lua
return function(subject)
    return subject(6), subject(-1)
end
```

`31_nonconstant_slot_key.luau`:
```lua
return function(subject)
    return subject(4, 1), subject(4, 2), subject(4, 3)
end
```

`32_slot_across_control_flow.luau`:
```lua
return function(subject)
    return subject(8, true), subject(8, false)
end
```

- [ ] **Step 3: Add the six names to the manifest**

Append to `_PROBE_NAMES` in `tools/luau_corpus/semantic.py`:

```python
    "27_register_array_vm",
    "28_escaping_slot_table",
    "29_slot_metatable",
    "30_aliased_slot_write",
    "31_nonconstant_slot_key",
    "32_slot_across_control_flow",
```

- [ ] **Step 4: Verify each case compiles, decompiles, recompiles, and matches**

```bash
python tools/run_luau_corpus.py --profiles primary --semantic \
  --decompiler target/release/luau-lifter.exe --no-build \
  --output tests/luau_corpus/results/current
head -4 tests/luau_corpus/results/current/summary.md
```

Expected: `Cases: 96; compile failures: 0; decompile failures: 0; recompile failures: 0.` and 93 semantic matches (31 probed cases × 3 profiles; `18_recursion` is unprobeable).

If any of the six fails to decompile on unmodified code, that is a pre-existing defect the case has exposed. Report it and keep the case — it belongs in the corpus regardless.

- [ ] **Step 5: Pin the literal results and run the tests**

Run the six through the runner as in Task 3 Step 2, add their encodings to the `expected` dict, then:

Run: `python -m unittest tests.python.test_luau_corpus -v`

Expected: PASS, 31 subtests in the literal-results test.

- [ ] **Step 6: Commit**

```bash
git add tests/luau_corpus/cases/ tests/luau_corpus/probes/ \
  tools/luau_corpus/semantic.py tests/python/test_luau_corpus.py
git commit -m "test: cover table-slot folding preconditions"
```

---

## Task 10: Slot Folding

The only pass that can produce wrong code. Preconditions are stated in the spec; implement them as written and do not relax one to gain a fold.

**Files:**
- Create: `ast/src/slot_folding.rs`
- Modify: `ast/src/lib.rs`, `luau-lifter/src/lib.rs:1076-1079`
- Test: `ast/src/slot_folding.rs` `mod tests`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `pub fn fold_table_slots(block: &mut Block, protected: &[RcLocal]) -> usize`, returning the number of folds applied.

- [ ] **Step 1: Write the failing tests**

One test per precondition, plus the positive case. In `ast/src/slot_folding.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // registers[1] = source.field
    // registers[2] = registers[1]
    // folds to: registers[2] = source.field
    #[test]
    fn folds_a_constant_slot_into_its_only_read() {
        let mut block = fixture_simple_chain();
        assert_eq!(fold_table_slots(&mut block, &[]), 1);
        assert_eq!(block.0.len(), 1);
    }

    #[test]
    fn keeps_the_write_when_a_call_intervenes() {
        let mut block = fixture_chain_with_intervening_call();
        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    #[test]
    fn keeps_the_write_when_the_table_is_a_parameter() {
        let mut block = fixture_chain_on_parameter_table();
        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    #[test]
    fn keeps_the_write_when_a_computed_key_is_written_anywhere() {
        let mut block = fixture_chain_with_computed_key_write();
        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    #[test]
    fn keeps_the_write_when_setmetatable_is_applied_to_the_table() {
        let mut block = fixture_chain_with_setmetatable();
        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    #[test]
    fn keeps_the_write_when_an_index_assignment_intervenes() {
        let mut block = fixture_chain_with_intervening_index_write();
        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    #[test]
    fn keeps_the_write_when_the_slot_is_read_twice() {
        let mut block = fixture_chain_with_two_reads();
        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    #[test]
    fn keeps_the_write_when_a_structured_statement_intervenes() {
        let mut block = fixture_chain_across_an_if();
        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }
}
```

Write each `fixture_*` helper as a plain function building the `Block` it describes. Copy the `local` helper from `ast/src/name_locals.rs:471`. Here is the positive fixture in full; the rest follow the same shape with one statement changed:

```rust
    fn slot(table: &RcLocal, key: f64) -> LValue {
        LValue::Index(Index::new(
            table.clone().into(),
            Literal::Number(key).into(),
        ))
    }

    fn slot_read(table: &RcLocal, key: f64) -> RValue {
        RValue::Index(Index::new(
            table.clone().into(),
            Literal::Number(key).into(),
        ))
    }

    /// local registers = {}
    /// registers[1] = source
    /// registers[2] = registers[1]
    fn fixture_simple_chain() -> Block {
        let registers = local(Some("registers"));
        let source = local(Some("source"));
        let mut declaration = Assign::new(
            vec![registers.clone().into()],
            vec![Table::default().into()],
        );
        declaration.prefix = true;

        Block(vec![
            declaration.into(),
            Assign::new(vec![slot(&registers, 1.0)], vec![source.into()]).into(),
            Assign::new(
                vec![slot(&registers, 2.0)],
                vec![slot_read(&registers, 1.0)],
            )
            .into(),
        ])
    }
```

The variations, each differing from `fixture_simple_chain` by exactly one thing:

| Fixture | Change |
| --- | --- |
| `fixture_chain_with_intervening_call` | Insert `Statement::Call(Call::new(local(Some("observe")).into(), vec![]))` between the two slot assignments |
| `fixture_chain_on_parameter_table` | Drop the `local registers = {}` declaration so `registers` is unbound in this block |
| `fixture_chain_with_computed_key_write` | Append `registers[index] = source` using a `RValue::Local` key instead of a literal |
| `fixture_chain_with_setmetatable` | Insert `Statement::Call(Call::new(Global::new(b"setmetatable".to_vec()).into(), vec![registers.clone().into(), Table::default().into()]))` after the declaration |
| `fixture_chain_with_intervening_index_write` | Insert `alias[1] = source` between the two slot assignments, where `alias` is a different local |
| `fixture_chain_with_two_reads` | Append a second statement reading `registers[1]` |
| `fixture_chain_across_an_if` | Wrap the second slot assignment in an `If` |

Confirm `Global::new` and `If` construction against `ast/src/global.rs` and `ast/src/if.rs` before writing them.

- [ ] **Step 2: Run the tests to verify they fail**

Register the module in `ast/src/lib.rs` with `mod slot_folding;` and `pub use slot_folding::*;`.

Run: `cargo +nightly test -p ast slot_folding`

Expected: FAIL to compile — `fold_table_slots` is not defined.

- [ ] **Step 3: Implement the pass**

Create the implementation in `ast/src/slot_folding.rs`. Three stages, so each precondition is enforced in exactly one place:

```rust
//! Folds a table used as a register array back into expressions.
//!
//! Obfuscated output routes almost every value through numbered slots of a
//! scratch table. Reading it means tracking those slots by hand. This pass
//! substitutes a slot's value into its single read when doing so cannot
//! change what the program does.
//!
//! The preconditions in `docs/superpowers/specs/2026-07-30-decompile-
//! readability-design.md` are the contract. They are conditions, not
//! heuristics: any one of them failing abandons the fold entirely.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    Assign, Block, LValue, Literal, LocalRw, RValue, RcLocal, SideEffects, Statement, Traverse,
};

/// How far forward a fold looks for its read.
///
/// The cap keeps the scan linear in block length. Folds beyond this distance
/// are rare and not worth a quadratic worst case in a phase measured in
/// milliseconds.
const SLOT_SCAN_LIMIT: usize = 64;

/// A constant slot key. Only these can be matched between a write and a read.
#[derive(PartialEq, Eq, Hash, Clone)]
enum SlotKey {
    Integer(i64),
    Name(Vec<u8>),
}

fn slot_key(value: &RValue) -> Option<SlotKey> {
    match value {
        RValue::Literal(Literal::Integer(key)) => Some(SlotKey::Integer(*key)),
        RValue::Literal(Literal::Number(key)) if key.fract() == 0.0 => {
            Some(SlotKey::Integer(*key as i64))
        }
        RValue::Literal(Literal::String(key)) => Some(SlotKey::Name(key.clone())),
        _ => None,
    }
}

/// Splits an assignment into `(table, key, value)` when it writes one
/// constant slot of one local table.
fn slot_write(statement: &Statement) -> Option<(RcLocal, SlotKey, &RValue)> {
    let assign = statement.as_assign()?;
    if assign.prefix || assign.parallel || assign.left.len() != 1 || assign.right.len() != 1 {
        return None;
    }
    let LValue::Index(index) = &assign.left[0] else {
        return None;
    };
    let RValue::Local(table) = index.left.as_ref() else {
        return None;
    };
    Some((table.clone(), slot_key(&index.right)?, &assign.right[0]))
}
```

**Stage one — which tables are eligible at all.** Enforces preconditions 2, 5, and 6 once per block rather than per fold:

```rust
/// Locals that are provably plain tables created here.
///
/// A table is dropped from the set if anything about it is unknown: it was
/// not created by a literal in this block, some write uses a computed key
/// that could collide with a constant one, or `setmetatable` was applied and
/// `__index`/`__newindex` could intercept the slots.
fn foldable_tables(block: &Block, protected: &FxHashSet<RcLocal>) -> FxHashSet<RcLocal> {
    let mut candidates = FxHashSet::default();
    let mut rejected = FxHashSet::default();

    for statement in &block.0 {
        if let Statement::Assign(assign) = statement
            && assign.prefix
            && let ([LValue::Local(target)], [RValue::Table(_)]) =
                (assign.left.as_slice(), assign.right.as_slice())
        {
            candidates.insert(target.clone());
        }

        if let Statement::Assign(assign) = statement {
            for lvalue in &assign.left {
                if let LValue::Index(index) = lvalue
                    && let RValue::Local(table) = index.left.as_ref()
                    && slot_key(&index.right).is_none()
                {
                    rejected.insert(table.clone());
                }
            }
        }

        reject_metatable_targets(statement, &mut rejected);
    }

    candidates
        .into_iter()
        .filter(|table| !rejected.contains(table) && !protected.contains(table))
        .collect()
}
```

Write `reject_metatable_targets` to walk every `RValue` of the statement and, for each `Call` whose callee is the global `setmetatable`, insert any `RValue::Local` argument into `rejected`.

**Stage two — find one fold.** Enforces preconditions 3, 4, and 7:

```rust
struct Fold {
    write: usize,
    read: usize,
    value: RValue,
}

/// Whether a statement could observe a table slot written before it.
///
/// Any call may run a closure that captured the table, and any write through
/// an index may target the same table under a different name. Both end the
/// window without needing to prove that either actually happened.
fn blocks_window(statement: &Statement) -> bool {
    if matches!(
        statement,
        Statement::If(_)
            | Statement::While(_)
            | Statement::Repeat(_)
            | Statement::NumericFor(_)
            | Statement::GenericFor(_)
            | Statement::Label(_)
            | Statement::Goto(_)
            | Statement::Break(_)
            | Statement::Continue(_)
            | Statement::Return(_)
    ) {
        return true;
    }
    if matches!(statement, Statement::Call(_) | Statement::MethodCall(_)) {
        return true;
    }
    if let Statement::Assign(assign) = statement
        && assign.left.iter().any(|lvalue| matches!(lvalue, LValue::Index(_)))
    {
        return true;
    }
    statement.has_side_effects()
}
```

`find_fold(block, start, foldable) -> Option<Fold>` then: read `(table, key, value)` from `slot_write(&block[start])`, bail unless `foldable` contains `table`, bail if `value` reads `table`, and scan `start + 1 ..= start + SLOT_SCAN_LIMIT`. At each statement count reads of `table[key]`; more than one total abandons the fold. Stop at the first statement where `blocks_window` is true — but check that statement for the read *before* stopping, since the read may be the very statement that also blocks. Return `Fold` only when exactly one read was found and the write is dead afterwards.

**Stage three — apply.** Replace the matching `RValue::Index` inside `block[fold.read]` with `fold.value`, then remove `block[fold.write]`. Use `traverse_rvalues` to find the index node rather than assuming a position.

**Driver:**

```rust
pub fn fold_table_slots(block: &mut Block, protected: &[RcLocal]) -> usize {
    let protected = protected.iter().cloned().collect::<FxHashSet<_>>();
    fold_block(block, &protected)
}
```

`fold_block` recurses into child blocks first, then folds the current block, and returns the total count. A table whose local appears in `protected` is never folded — those are upvalues visible to other functions, so this block cannot see every write to them.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo +nightly test -p ast slot_folding`

Expected: PASS, 8 tests. Only the first reports a non-zero fold count.

- [ ] **Step 5: Wire the pass into the fixpoint loop**

In `luau-lifter/src/lib.rs`, replace the `AstRecovery` block at line 1076:

```rust
    catch_phase(DecompilePhase::AstRecovery, Some(function_id), None, || {
        // Slot folding exposes new single-use locals for expression
        // recovery, and recovery exposes new adjacent slot writes. Iterating
        // lets each feed the other; the cap bounds the work.
        const RECOVERY_ROUNDS: usize = 4;
        for _ in 0..RECOVERY_ROUNDS {
            let mut changes = 0;
            changes += ast::eliminate_aliases_with_protected(&mut block, &upvalues_in);
            changes += ast::fold_table_slots(&mut block, &upvalues_in);
            changes += ast::recover_expressions_with_protected(&mut block, &upvalues_in);
            if changes == 0 {
                break;
            }
        }
        ast::cleanup_control_flow(&mut block);
    })?;
```

Check the return type of `recover_expressions_with_protected` at `ast/src/expression_recovery.rs:419` — if it returns a stats struct rather than `usize`, use its change count field.

- [ ] **Step 6: Verify against the corpus**

```bash
cargo +nightly build --release -p luau-lifter
python tools/run_luau_corpus.py --profiles primary --semantic \
  --decompiler target/release/luau-lifter.exe --no-build \
  --output tests/luau_corpus/results/current
head -4 tests/luau_corpus/results/current/summary.md
```

Expected: `Cases: 96; compile failures: 0; decompile failures: 0; recompile failures: 0.` and 93 semantic matches (31 probed cases × 3 profiles; `18_recursion` is unprobeable).

**A single semantic mismatch here means a precondition is wrong.** Do not adjust the expected value. Read the failing case's `.log`, find which precondition the fold violated, and fix the precondition. If case `29_slot_metatable` fails, apply the fallback from the spec: tighten precondition 2 so a table that appears as a bare argument to any call is never foldable.

- [ ] **Step 7: Check the real-file output and performance**

```bash
./target/release/luau-lifter.exe "$MEDAL_BIG_FIXTURE" > /tmp/folded.lua
./.tools/luau-windows/luau-compile.exe --binary /tmp/folded.lua > /dev/null; echo "exit=$?"
wc -l /tmp/folded.lua
python tools/measure_decompiler.py --runs 3
```

Expected: `luau-compile --binary` exits 0. NOTE: `luau-analyze` does NOT finish within 300 s on a 5.5 MB file, so it is not a usable check at this size; the compiler validates the whole output in under a second. Line count drops well below 248,778 — the spec's estimate is roughly 40%. Wall clock under 16.0 s, `ast-recovery` under 1.0 s.

If `ast-recovery` exceeds 1.0 s, the scan is doing more work than the cap allows. Check that `find_fold` aborts at the first blocking statement rather than scanning the full window every time.

- [ ] **Step 8: Commit**

```bash
git add ast/src/slot_folding.rs ast/src/lib.rs luau-lifter/src/lib.rs
git commit -m "feat: fold table-slot writes into their reads"
```

---

## Task 11: Constructor Folding

**Files:**
- Create: `ast/src/table_construction.rs`
- Modify: `ast/src/lib.rs`, `luau-lifter/src/lib.rs` (fixpoint loop)
- Test: `ast/src/table_construction.rs` `mod tests`

**Interfaces:**
- Consumes: `foldable_tables` logic from Task 10 — extract it into a shared `pub(crate) fn` in `slot_folding.rs` rather than duplicating it.
- Produces: `pub fn fold_table_constructors(block: &mut Block, protected: &[RcLocal]) -> usize`.

- [ ] **Step 1: Write the failing tests**

```rust
    // local t = {}
    // t[1] = 10
    // t[2] = 20
    // -> local t = { 10, 20 }
    #[test]
    fn folds_a_dense_positional_run_into_the_literal() {
        let mut block = fixture_dense_run();
        assert_eq!(fold_table_constructors(&mut block, &[]), 1);
        assert_eq!(block.0.len(), 1);
    }

    #[test]
    fn folds_a_constant_string_key_run_into_named_fields() {
        let mut block = fixture_named_run();
        assert_eq!(fold_table_constructors(&mut block, &[]), 1);
    }

    #[test]
    fn keeps_a_sparse_run_expanded() {
        let mut block = fixture_sparse_run();
        assert_eq!(fold_table_constructors(&mut block, &[]), 0);
    }

    #[test]
    fn keeps_the_run_when_an_element_reads_the_table() {
        let mut block = fixture_run_reading_the_table();
        assert_eq!(fold_table_constructors(&mut block, &[]), 0);
    }

    #[test]
    fn keeps_the_run_when_a_call_intervenes() {
        let mut block = fixture_run_with_intervening_call();
        assert_eq!(fold_table_constructors(&mut block, &[]), 0);
    }

    #[test]
    fn keeps_the_run_when_it_does_not_start_at_the_literal() {
        let mut block = fixture_run_starting_late();
        assert_eq!(fold_table_constructors(&mut block, &[]), 0);
    }
```

Write each fixture as a plain builder function, as in Task 10.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo +nightly test -p ast table_construction`

Expected: FAIL to compile.

- [ ] **Step 3: Implement the pass**

Reuse `foldable_tables` for preconditions 2, 5, and 6. Then, for each declaration `local T = <table literal>`:

- Walk forward while each statement is `T[K] = E` with `K` a constant.
- Stop at the first statement that is not such a write, or that contains a call, has side effects, or is structured.
- Stop if any `E` reads `T`.
- Fold only if the collected keys are dense integers starting at 1, or all constant strings. Mixed or sparse runs are left alone.
- Append the collected `(key, value)` pairs to the literal in write order and delete the folded statements.

Field order follows write order, which keeps output stable across runs.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo +nightly test -p ast table_construction`

Expected: PASS, 6 tests.

- [ ] **Step 5: Add to the fixpoint loop**

In `luau-lifter/src/lib.rs`, add inside the loop body after `recover_expressions_with_protected`:

```rust
            changes += ast::fold_table_constructors(&mut block, &upvalues_in);
```

- [ ] **Step 6: Verify against the corpus and measure**

```bash
cargo +nightly build --release -p luau-lifter
python tools/run_luau_corpus.py --profiles primary --semantic \
  --decompiler target/release/luau-lifter.exe --no-build \
  --output tests/luau_corpus/results/current
head -4 tests/luau_corpus/results/current/summary.md
python tools/measure_decompiler.py --runs 3
```

Expected: 96 cases, 0 failures, 93 semantic matches, wall clock under 16.0 s.

Pay attention to case `08_table_incremental` — it mixes positional, named, computed, and `#result + 1` keys, so most of its run must stay expanded. If it folds entirely, the density check is wrong.

- [ ] **Step 7: Commit**

```bash
git add ast/src/table_construction.rs ast/src/lib.rs luau-lifter/src/lib.rs
git commit -m "feat: fold table field runs into constructors"
```

---

## Task 12: Column Budget Wrap Guard

Folding merges statements, so lines that were short become long. This is the only wrapping in scope.

**Files:**
- Modify: `ast/src/formatter.rs`
- Test: `ast/src/formatter.rs` `mod tests`

**Interfaces:**
- Consumes: `Formatter::table_renders_multiline` from Task 4.
- Produces: no new public API.

- [ ] **Step 1: Measure whether this is needed**

```bash
./target/release/luau-lifter.exe "$MEDAL_BIG_FIXTURE" > /tmp/folded.lua
awk 'length > 120' /tmp/folded.lua | wc -l
awk 'length > 120 && !/"/' /tmp/folded.lua | wc -l
```

The second number is the count of long **code** lines. On unmodified `main` it is 0. If it is still 0 after folding, **skip this task** — record the measurement in the commit message for Task 11 and stop here. The spec admits wrapping only as a guard for lines folding creates; with no such lines, the guard is dead code.

- [ ] **Step 2: Write the failing test**

Only if Step 1 found long code lines:

```rust
    #[test]
    fn an_argument_list_past_the_column_budget_wraps_one_per_line() {
        let call = Call::new(
            local("someFunctionWithAVeryLongName").into(),
            (0..8)
                .map(|index| {
                    RValue::Local(local(&format!(
                        "argumentNumber{index}WithALongName"
                    )))
                })
                .collect(),
        );
        let block = Block(vec![Statement::Call(call)]);

        let formatted = block.to_string();

        assert!(formatted.contains(",\n"));
        assert!(formatted.lines().all(|line| line.len() <= 120));
    }

    #[test]
    fn a_short_argument_list_stays_on_one_line() {
        let call = Call::new(local("f").into(), vec![local("a").into(), local("b").into()]);
        let block = Block(vec![Statement::Call(call)]);

        assert_eq!(block.to_string(), "f(a, b)");
    }
```

- [ ] **Step 3: Run the tests to verify the first fails**

Run: `cargo +nightly test -p ast column_budget`

Expected: FAIL — the long call renders on one line.

- [ ] **Step 4: Implement the guard**

Add a `COLUMN_BUDGET: usize = 120` constant to `ast/src/formatter.rs`. In `format_arg_list`, render the arguments into a scratch `String` first; if the current indentation plus that string exceeds the budget, emit one argument per line at one extra indentation level instead.

The scratch render is acceptable here only because it runs on the small minority of argument lists that are candidates. Guard it with a cheap length estimate — sum of argument display lengths — so the scratch buffer is never built for a list that is obviously short.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo +nightly test -p ast`

Expected: PASS.

- [ ] **Step 6: Verify and measure**

```bash
cargo +nightly build --release -p luau-lifter
python tools/run_luau_corpus.py --profiles primary --semantic \
  --decompiler target/release/luau-lifter.exe --no-build \
  --output tests/luau_corpus/results/current
head -4 tests/luau_corpus/results/current/summary.md
python tools/measure_decompiler.py --runs 3
```

Expected: 96 cases, 0 failures, 93 semantic matches, `format` phase under 1.5 s.

- [ ] **Step 7: Commit**

```bash
git add ast/src/formatter.rs
git commit -m "feat: wrap argument lists past the column budget"
```

---

## Final Verification

- [ ] **Step 1: Full matrix including compatibility profiles**

```bash
python tools/run_luau_corpus.py --profiles all --semantic \
  --decompiler target/release/luau-lifter.exe --no-build \
  --output tests/luau_corpus/results/current
head -4 tests/luau_corpus/results/current/summary.md
```

Expected: 0 compile, decompile, and recompile failures across primary, secondary, and compatibility profiles. Bytecode V9 through V12 all still round-trip.

- [ ] **Step 2: Full test suites**

```bash
cargo +nightly test
python -m unittest discover -s tests/python -t . -v
```

Expected: PASS.

- [ ] **Step 3: Record the final readability and performance numbers**

```bash
python tools/measure_decompiler.py --runs 3
./target/release/luau-lifter.exe "$MEDAL_BIG_FIXTURE" > /tmp/final.lua
./.tools/luau-windows/luau-compile.exe --binary /tmp/final.lua > /dev/null; echo "exit=$?"
wc -l /tmp/final.lua
grep -c '^$' /tmp/final.lua
grep -cE '^\s*local v[0-9]+' /tmp/final.lua
grep -cE '^\s*v[0-9]+\[[0-9]+\] = ' /tmp/final.lua
```

Compare against the baseline recorded in the spec: 248,778 lines, 0 blank lines, ~1,500 `vN` locals, 113,209 slot assignments, 13.42–13.56 s, 1,818 MB.

- [ ] **Step 4: Write the results into the spec**

Add a `## Results` section to `docs/superpowers/specs/2026-07-30-decompile-readability-design.md` with a before/after table for every metric above, matching the format used by `2026-07-30-luau-decompiler-performance-design.md`.

State plainly anything that did not land — a skipped Task 12, a precondition that had to tighten, a case removed from the probe manifest. A results section that reports only what worked is not a record.

```bash
git add docs/superpowers/specs/2026-07-30-decompile-readability-design.md
git commit -m "docs: record decompile readability results"
```
