# Luau Phase 0 Semantic Harness and Errors Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Establish trustworthy differential runtime evidence for the six known
semantic failures and replace comment-shaped decompiler failures with structured
errors.

**Architecture:** The existing Python corpus runner invokes a committed Luau
probe runner only for six explicitly mapped repository fixtures. The probe
runner canonicalizes packed return values so result width, interior `nil`, and
table contents compare deterministically. Rust carries a typed
`DecompileError` through CLI, web server, and worker boundaries; panics are
caught at named pipeline phases and never emitted as source comments.

**Tech Stack:** Python 3 `unittest` and `subprocess`, Luau 0.731 CLI, Rust
nightly, existing `luau-lifter`, `luau-worker`, and `web-server` crates.

## Global Constraints

- Execute only the six trusted repository fixtures `04`, `05`, `13`, `15`,
  `20`, and `21`.
- Never execute arbitrary user bytecode or non-allowlisted corpus cases.
- Probe names, arguments, and expected behavior remain test infrastructure and
  are never passed to decompiler logic.
- Capture every return list with `table.pack`.
- Compare exact top-level width, ordered scalars including `nil`, and nested
  acyclic tables independent of key iteration order and table identity.
- Keep `27_orchestration_engine` diagnostic-only and outside the Phase 0
  blocking allowlist.
- One runtime or decompiler failure must not stop remaining corpus cases.
- Normal decompiler output is valid source or a typed error; it is never a
  diagnostic comment pretending to be source.
- Do not add a general arbitrary-script runtime, fuzzer, or semantic analyzer.

---

### Task 1: Trusted Probe Contract and Canonical Luau Output

**Files:**
- Create: `tools/luau_corpus/semantic.py`
- Create: `tests/luau_corpus/probes/runner.luau`
- Create: `tests/luau_corpus/probes/04_calls_multireturn.luau`
- Create: `tests/luau_corpus/probes/05_varargs.luau`
- Create: `tests/luau_corpus/probes/13_repeat_until.luau`
- Create: `tests/luau_corpus/probes/15_generic_for.luau`
- Create: `tests/luau_corpus/probes/20_pcall_style_flow.luau`
- Create: `tests/luau_corpus/probes/21_state_machine.luau`
- Modify: `tests/python/test_luau_corpus.py`

**Interfaces:**
- Produces: immutable `SemanticProbe(case_name: str, probe_path: Path)`.
- Produces: `TRUSTED_SEMANTIC_PROBES: Mapping[str, SemanticProbe]`.
- Produces:
  `runtime_command(runtime: Path, runner: Path, subject_module: str,
  probe_module: str) -> tuple[str, ...]`.
- Produces: one stdout record beginning with `SEMANTIC_RESULT ` followed by a
  canonical packed-value encoding.

- [ ] **Step 1: Write failing manifest and command tests**

Add tests that require exactly this allowlist:

```python
def test_semantic_probe_manifest_is_explicit_and_limited() -> None:
    self.assertEqual(
        tuple(TRUSTED_SEMANTIC_PROBES),
        (
            "04_calls_multireturn",
            "05_varargs",
            "13_repeat_until",
            "15_generic_for",
            "20_pcall_style_flow",
            "21_state_machine",
        ),
    )
    self.assertNotIn("27_orchestration_engine", TRUSTED_SEMANTIC_PROBES)


def test_runtime_command_passes_subject_and_probe_after_program_args() -> None:
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
```

- [ ] **Step 2: Run the focused tests and verify red state**

Run:

```bash
python -m unittest tests.python.test_luau_corpus.ProfileTests.test_semantic_probe_manifest_is_explicit_and_limited
python -m unittest tests.python.test_luau_corpus.ProfileTests.test_runtime_command_passes_subject_and_probe_after_program_args
```

Expected: import failure because `tools.luau_corpus.semantic` does not exist.

- [ ] **Step 3: Implement the immutable manifest and runtime command**

Use `MappingProxyType` so runtime code cannot extend the allowlist:

```python
@dataclass(frozen=True)
class SemanticProbe:
    case_name: str
    probe_path: Path


TRUSTED_SEMANTIC_PROBES = MappingProxyType(
    {
        name: SemanticProbe(name, Path(f"tests/luau_corpus/probes/{name}.luau"))
        for name in (
            "04_calls_multireturn",
            "05_varargs",
            "13_repeat_until",
            "15_generic_for",
            "20_pcall_style_flow",
            "21_state_machine",
        )
    }
)
```

`runtime_command` accepts module paths that are already relative to
`runner.luau` and strips the `.luau` suffix before passing them after `-a`.

- [ ] **Step 4: Implement the canonical Luau runner**

`runner.luau` receives `subjectPath, probePath = ...`, requires both modules,
calls `probe(subject)`, captures its returns with `table.pack`, and prints one
`SEMANTIC_RESULT ` line.

Canonical encoding uses these tags:

```text
n                         nil
b0 or b1                  boolean
d<%.17g>;                 finite number
s<byte-length>:<bytes>    string
p<count>[values...]       packed table with integer n
t<count>{key=value...}    ordinary table, sorted by encoded key
```

Reject non-finite numbers, cycles, functions, userdata, threads, vector values,
and table keys other than strings, finite numbers, or booleans. A packed table
encodes every index from `1` through `n`, including absent entries as `nil`.
An ordinary table sorts entries by each key's canonical encoding.

- [ ] **Step 5: Implement the exact six probes**

Each file returns one function accepting the fixture's exported function.
Implement these literal invocations:

```lua
-- 04
return subject(20)

-- 05
return subject("p", 1, 2, 3, 4)

-- 13
return subject(30)

-- 15
return subject({ 2, 4, name = "kept" })

-- 20
local success = table.pack(subject(
    function() return 7, 8 end,
    function() return 0 end
))
local recovered = table.pack(subject(
    function() return nil, "reason" end,
    function() return 9, 10 end
))
local failed = table.pack(subject(
    function() return nil, "reason" end,
    function() return nil, "still missing" end
))
return success, recovered, failed

-- 21
return subject({ "start", "tick", "stop" })
```

- [ ] **Step 6: Add a real-runtime canonicalization test**

When `.tools/luau-windows/luau.exe` exists, create a temporary subject and probe
module and run the committed runner. Assert the exact record encodes
`table.pack("x", nil, { b = 2, a = 1 })` identically across two runs. Skip only
when the bundled runtime is absent.

- [ ] **Step 7: Verify Task 1 and commit**

Run:

```bash
python -m unittest tests.python.test_luau_corpus -v
```

Expected: all Python tests pass.

Commit:

```bash
git add tools/luau_corpus/semantic.py tests/luau_corpus/probes tests/python/test_luau_corpus.py
git commit -m "test: add trusted Luau semantic probes"
```

---

### Task 2: Corpus Runtime Execution and Reporting

**Files:**
- Modify: `tools/luau_corpus/model.py`
- Modify: `tools/luau_corpus/process.py`
- Modify: `tools/luau_corpus/report.py`
- Modify: `tools/luau_corpus/__init__.py`
- Modify: `tests/python/test_luau_corpus.py`

**Interfaces:**
- Produces:
  `RuntimeResult(exit_code: int, normalized_result: str | None, stderr: str)`.
- Extends `CaseResult` with `source_runtime`, `generated_runtime`, and
  `semantic_match: bool | None`.
- Extends `run_corpus` with `semantic: bool = False` and
  `runtime: Path | None = None`.
- Preserves existing behavior and output when `semantic=False`.

- [ ] **Step 1: Write failing allowlist and mismatch tests**

Extend the temporary corpus with `04_calls_multireturn.luau` and
`99_untrusted.luau`. Use a fake runtime that appends its subject argument to an
invocation log and emits different `SEMANTIC_RESULT` records for source and
generated `04`.

Assert:

```python
self.assertEqual(len(runtime_invocations), 2)
self.assertTrue(all("04_calls_multireturn" in item for item in runtime_invocations))
self.assertIsNone(untrusted.source_runtime)
self.assertIsNone(untrusted.generated_runtime)
self.assertIsNone(untrusted.semantic_match)
self.assertFalse(trusted.semantic_match)
```

Add a second test where generated runtime exits `7`. Assert later cases still
run, the generated exit is recorded, and `semantic_match` is `False`.

- [ ] **Step 2: Run focused tests and verify red state**

Run:

```bash
python -m unittest tests.python.test_luau_corpus.CorpusRunnerTests -v
```

Expected: `run_corpus` rejects `semantic` and `runtime`, and `CaseResult` lacks
runtime fields.

- [ ] **Step 3: Add runtime model and parser**

Implement:

```python
@dataclass(frozen=True)
class RuntimeResult:
    exit_code: int
    normalized_result: str | None
    stderr: str
```

Parse exactly one stdout line beginning with `SEMANTIC_RESULT `. Treat a zero
exit without exactly one result record as a failed runtime normalization:
`normalized_result=None`.

- [ ] **Step 4: Execute only allowlisted source/generated pairs**

In `run_corpus`, runtime execution is eligible only when:

- `semantic` is true;
- `source.stem` is an exact key in `TRUSTED_SEMANTIC_PROBES`;
- source compilation, decompilation, and generated recompilation all exit zero.

Resolve source and generated module paths relative to
`tests/luau_corpus/probes/runner.luau`, remove `.luau`, normalize separators to
`/`, and ensure the relative path begins with `./` or `../`.

Run source and generated subjects independently. Set `semantic_match=True` only
when both exit zero, both normalized records exist, and the records are equal.

Append `[source-runtime]` and `[generated-runtime]` sections containing exit,
normalized result when present, and stderr.

- [ ] **Step 5: Extend reports without changing non-semantic exit rules**

JSON case fields:

```text
source_runtime_exit
source_runtime_result
generated_runtime_exit
generated_runtime_result
semantic_match
```

JSON totals:

```text
semantic_checked
semantic_mismatched
source_runtime_failed
generated_runtime_failed
```

Markdown adds `source run`, `generated run`, and `semantic` columns. Use `-` for
not checked, integer exits for runtime columns, and `pass`/`fail` for semantic
comparison.

- [ ] **Step 6: Verify Task 2 and commit**

Run:

```bash
python -m unittest tests.python.test_luau_corpus -v
```

Expected: all Python tests pass, including legacy report assertions updated for
the three new columns.

Commit:

```bash
git add tools/luau_corpus tests/python/test_luau_corpus.py
git commit -m "feat: compare trusted Luau runtime results"
```

---

### Task 3: Semantic CLI and Baseline Evidence

**Files:**
- Modify: `tools/run_luau_corpus.py`
- Modify: `tests/python/test_luau_corpus.py`
- Modify: `tests/luau_corpus/README.md`

**Interfaces:**
- Produces CLI flag `--semantic`.
- Produces CLI option `--runtime PATH`, defaulting to
  `.tools/luau-windows/luau.exe`.
- Makes semantic mismatches and trusted runtime failures affect CLI exit status
  only when `--semantic` is present.

- [ ] **Step 1: Write failing CLI exit-policy tests**

Extract and test:

```python
def run_failed(result: RunResult, semantic: bool) -> bool
```

Assert a trusted mismatch is ignored when `semantic=False`, fails when
`semantic=True`, and an unchecked case never fails semantic mode.

- [ ] **Step 2: Run the focused test and verify red state**

Run:

```bash
python -m unittest tests.python.test_luau_corpus.ProfileTests.test_semantic_exit_policy
```

Expected: import failure because `run_failed` does not exist.

- [ ] **Step 3: Add CLI arguments and exit policy**

Pass `semantic=args.semantic` and the resolved runtime path to `run_corpus`.
`run_failed` retains compile/decompile/recompile checks and, in semantic mode,
also fails for any checked case whose source/generated runtime is nonzero,
normalization is absent, or `semantic_match` is not `True`.

- [ ] **Step 4: Document trusted execution**

Document:

```bash
python tools/run_luau_corpus.py --profiles all --semantic
python tools/run_luau_corpus.py --profiles primary --semantic --case 04_calls_multireturn
```

State explicitly that only six committed probes execute and
`27_orchestration_engine` remains manual diagnostic input.

- [ ] **Step 5: Run current baseline**

Run each reproduced profile/case:

```bash
python tools/run_luau_corpus.py --profiles primary --semantic --case 04_calls_multireturn --output tests/luau_corpus/results/phase0-04
python tools/run_luau_corpus.py --profiles primary --semantic --case 05_varargs --output tests/luau_corpus/results/phase0-05
python tools/run_luau_corpus.py --profiles primary --semantic --case 13_repeat_until --output tests/luau_corpus/results/phase0-13
python tools/run_luau_corpus.py --profiles primary --semantic --case 15_generic_for --output tests/luau_corpus/results/phase0-15
python tools/run_luau_corpus.py --profiles primary --semantic --case 20_pcall_style_flow --output tests/luau_corpus/results/phase0-20
python tools/run_luau_corpus.py --profiles secondary --semantic --case 21_state_machine --output tests/luau_corpus/results/phase0-21
```

Expected: source runtime succeeds; the known generated runtime errors or
mismatches are recorded; each command exits nonzero because Phase 0 measures
existing defects rather than repairing them.

- [ ] **Step 6: Verify Task 3 and commit**

Run:

```bash
python -m unittest tests.python.test_luau_corpus -v
```

Commit:

```bash
git add tools/run_luau_corpus.py tests/python/test_luau_corpus.py tests/luau_corpus/README.md
git commit -m "feat: expose trusted semantic corpus mode"
```

---

### Task 4: Typed Decompiler Errors

**Files:**
- Create: `luau-lifter/src/error.rs`
- Modify: `luau-lifter/src/lib.rs`
- Modify: `luau-lifter/src/main.rs`
- Modify: `luau-worker/src/lib.rs`
- Modify: `web-server/src/main.rs`

**Interfaces:**
- Produces `DecompilePhase` variants `Deserialize`, `Lift`, `Ssa`,
  `Structure`, `SsaDestruction`, `Restructure`, `AstRecovery`, `Declaration`,
  `Link`, `Validate`, `Format`, and `Unknown`.
- Produces:
  `DecompileError { phase, function_id, instruction, invariant, detail }`.
- Produces:
  `decompile_bytecode(bytecode, encode_key) -> Result<String, DecompileError>`.
- Preserves `try_decompile_bytecode` as a typed alias for compatibility.

- [ ] **Step 1: Write failing error-display and panic-boundary tests**

Add Rust unit tests:

```rust
#[test]
fn structured_error_display_includes_available_context()

#[test]
fn phase_boundary_converts_panic_without_emitting_source()

#[test]
fn invalid_bytecode_returns_deserialize_error()
```

The phase-boundary test invokes the internal catcher with `function_id=7`,
`instruction=Some(12)`, and a closure that panics `"bad merge"`. Assert the
error fields and display text. The invalid bytecode test asserts
`decompile_bytecode(&[0xff], 1).is_err()`.

- [ ] **Step 2: Run focused Rust tests and verify red state**

Run:

```bash
cargo +nightly test -p luau-lifter error -- --nocapture
```

Expected: `error` module and typed API do not exist.

- [ ] **Step 3: Implement typed errors and phase catcher**

Implement `Display` and `std::error::Error` without adding a dependency.
`catch_phase` uses `catch_unwind(AssertUnwindSafe(operation))` and maps the
panic payload into:

```rust
DecompileError {
    phase,
    function_id,
    instruction,
    invariant: "panic-free decompilation",
    detail: panic_message(payload),
}
```

Deserializer strings map to `Deserialize` with invariant
`"valid Luau bytecode"`.

- [ ] **Step 4: Replace per-function failure comments**

Change `decompile_function` to return
`Result<(ByAddress<Arc<Mutex<ast::Function>>>, Vec<ast::RcLocal>),
DecompileError>`.

Wrap lift, SSA construction/rewrites, SSA destruction, restructuring, AST
recovery, declaration placement, upvalue linking, validation, and formatting
at their named boundaries. Collect function results as
`Result<FxHashMap<_, _>, DecompileError>`. Remove the panic hook, backtrace
thread-local, `"failed to decompile"` comment insertion, and the old
comment-producing `decompile_bytecode` wrapper.

- [ ] **Step 5: Update external callers**

CLI prints `decompiler error: {error}` to stderr and exits one.

`web-server` adds a `Decompile(String)` error mapped to HTTP 422 and propagates
the typed result.

`luau-worker` HTTP returns status 422 for a decompiler error. Websocket
responses use:

```rust
struct DecompileResponse {
    id: String,
    decompilation: Option<String>,
    error: Option<String>,
}
```

Exactly one of `decompilation` and `error` is populated.

- [ ] **Step 6: Verify Task 4 and commit**

Run:

```bash
cargo +nightly test -p luau-lifter -- --nocapture
cargo +nightly check -p web-server
cargo +nightly check -p luau-worker
```

Expected: all commands exit zero and no failure path emits comment-shaped
source.

Commit:

```bash
git add luau-lifter/src/error.rs luau-lifter/src/lib.rs luau-lifter/src/main.rs luau-worker/src/lib.rs web-server/src/main.rs
git commit -m "refactor: propagate structured decompiler errors"
```

---

### Task 5: Phase 0 Regression Gate

**Files:**
- Modify only if verification exposes a Phase 0 defect in files owned by Tasks
  1 through 4.

**Interfaces:**
- Consumes all Phase 0 interfaces.
- Produces baseline semantic evidence for the six known failures.

- [ ] **Step 1: Run focused suites**

```bash
python -m unittest tests.python.test_luau_corpus -v
cargo +nightly test -p luau-lifter -- --nocapture
```

Expected: all focused tests pass.

- [ ] **Step 2: Run the full Rust workspace**

```bash
cargo +nightly test --workspace
```

Expected: all workspace tests pass.

- [ ] **Step 3: Run all existing corpus configurations**

```bash
python tools/run_luau_corpus.py --profiles all --output tests/luau_corpus/results/phase0-static
```

Expected: every existing blocking case compiles, decompiles, and recompiles.
The diagnostic directory is not discovered.

- [ ] **Step 4: Audit semantic baseline artifacts**

For each Phase 0 semantic result, verify:

- source runtime exit is zero;
- source normalized result matches the literal probe contract;
- generated failure or mismatch matches the reproduced defect;
- log contains both runtime sections;
- JSON and Markdown agree on `semantic_match`;
- remaining cases continued after each mismatch.

- [ ] **Step 5: Record Phase 0 completion**

Update the active implementation checklist only after all infrastructure tests
pass and the six failures are measured. Do not claim the six semantic defects
are fixed; their failing baseline is the input to Phases 1, 3, and 5.
