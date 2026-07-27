# Decompiler Baseline Findings

## Environment

- Bundled compiler: Luau 0.731-era Windows CLI, SHA-256 `AC732C01BA5F169EC9C7E0ADB7A060C6D25EED028B286B06C1B664E056ED5B41`.
- Bundled compiler default bytecode version: 12; type-information version: 3.
- Rust: `rustc 1.99.0-nightly (dc3f85158 2026-07-26)`.
- Python: 3.11.9.
- Repository: source download without `.git` metadata.

The Rust workspace used Rust 2021 while current nightly requires Rust 2024 for its stabilized let-chain syntax. The workspace and explicit `luau-lifter` crate edition were migrated to 2024, with four compiler-directed binding-pattern updates. `cargo +nightly build -p luau-lifter` then completed.

## Final Remediation Result

The completed compile/decompile/recompile matrix is fully green:

- primary profiles (`O1/g1`, `O2/g1`, `O2/g0`): 72/72;
- secondary profiles (`O0/g1`, `O1/g0`, `O1/g2`): 72/72;
- compatibility profiles (bytecode V9, V10, V11, V12): 96/96;
- V9-V12/current-profile compiler round trips: 240/240;
- V4-V8 parser/format fixtures: passing;
- generated `goto`/label statements: 0;
- source compile failures: 0;
- decompiler failures: 0;
- decompiled-source recompile failures: 0.

The final machine-readable and Markdown reports are under:

```text
tests/luau_corpus/results/final-primary/
tests/luau_corpus/results/final-all/
tests/luau_corpus/results/final-versions/
```

The compatibility layer accepts V4 through V12. The bundled compiler can emit
only V9 through V12, so those versions have real compiler round trips. V4
through V8 are covered with format-level parser fixtures derived from the
versioned wire layout. Historical compiler round trips remain a useful future
addition, but are not claimed here.

Retained improvements are source-agnostic:

- validated version context and bounded V12 prototype parsing;
- exhaustive constant/opcode layout metadata and version gates;
- V9-V11 opcode-family lifting;
- V11 feedback and V12 cost metadata consumption;
- debug-record parsing without panic;
- exact integer and double-vector constants;
- `DUPTABLE` template reconstruction and safe duplicate-field compaction;
- side-effect-aware table-assignment folding;
- CFG edge-target splitting for shared continuations;
- a final AST invariant that rejects unsupported jump nodes instead of emitting
  invalid Luau.

## Initial Corpus Result

The authored corpus contains 24 sources. All 24 compile under `O1/g1`, `O2/g1`, and `O2/g0`: 72/72 source compilations succeeded.

The first focused compile/decompile boundary attempted `01_literals_locals` under all three primary profiles:

- compile: 3/3 succeeded;
- decompile: 0/3 succeeded;
- failure: `Unsupported bytecode version: 12`;
- recompile: not attempted because no decompiled source existed.

A full corpus run at this point would repeat the same version-gate failure 72 times, so it was paused until compatibility is implemented.

## Representative Output

No meaningful current-compiler decompiled output exists yet. The decompiler terminates before chunk, function, CFG, SSA, or formatting work starts.

The focused diagnostics are under:

```text
tests/luau_corpus/results/current/O1_g1/01_literals_locals.log
tests/luau_corpus/results/current/O2_g1/01_literals_locals.log
tests/luau_corpus/results/current/O2_g0/01_literals_locals.log
```

## Confirmed Root Causes

### 1. Top-level version gate

`luau-lifter/src/deserializer/bytecode.rs` accepts only bytecode versions 4 through 6. Bundled compiler output begins with version 12, so decompilation panics before parsing.

Confidence: high.

### 2. Version is not threaded into function parsing

`Chunk::parse` receives the version but calls `Function::parse` without it. Version 11 adds a feedback trailer. Version 12 adds a byte-length prefix per prototype and optional cost metadata. These layouts cannot be selected correctly without a validated version context.

Confidence: high.

### 3. Constants stop at the version-5 schema

The local constant parser recognizes tags 0 through 7. Later formats add:

- v7 tag 8: table templates with constant values;
- v8 tag 9: signed 64-bit integers;
- v10 tag 10: class shapes;
- current V4-V12 format tag 11: double-precision vectors.

The existing `DUPTABLE` lifter discards even recognized template shape and always emits an empty table.

Confidence: high.

### 4. Opcodes, lengths, and lifter semantics stop at version 6

Local opcode definitions end at ordinal 82 (`IDIVK`). Later formats add:

- v9 ordinals 83–85: userdata get, set, and namecall;
- v10 ordinal 86: class-member creation;
- v11 ordinals 87–88: feedback call and prototype comparison.

All six use an AUX word. Local instruction encoding and AUX classification use separate numeric whitelists, so accepting enum values alone would shift program counters and corrupt later control flow.

Confidence: high.

### 5. Debug records deliberately panic

Valid `g2` bytecode reaches `panic!("we have debug info")`. Existing record-consuming code below that panic is unreachable. Absolute line deltas are serialized as signed 32-bit values but stored as unsigned.

Confidence: high.

### 6. Trailing bytes are silently ignored

Top-level deserialization discards unconsumed input. Missing v11 feedback or v12 cost parsing can therefore look successful in some layouts while leaving the parser misaligned.

Confidence: high.

### 7. Failures occur outside the decompiler panic boundary

Deserialization and initial lifting happen before function-level panic catching. Unsupported versions, tags, opcodes, or malformed layouts terminate the process with exit 101 instead of returning a localized diagnostic.

Confidence: high.

## Empirical Version Profiles

Bundled compiler flags produced real version headers:

| Version | Required flag shape | Observed on wonky case |
| --- | --- | --- |
| 9 | cost model off, call feedback off, classes off | 1684 bytes |
| 10 | cost model off, call feedback off, classes on | 1684 bytes |
| 11 | cost model off, call feedback on | 1733 bytes |
| 12 | bundled defaults/cost model on | 1746 bytes |

Version 11 text output contains `CALLFB`, proving the profile exercises new instruction semantics rather than only a different header.

Bundled compiler cannot emit versions V4-V8. Those require format-level fixtures or pinned official historical compilers. Existing V4-V6 behavior remains covered while V7-V8 fixtures exercise their new constants.

## Rejected Hypotheses

- **Only widen `4..=6` to `4..=12`:** rejected. Version 12 immediately misreads its prototype-size prefix as the first function-header byte.
- **Use an old compiler and avoid current formats:** rejected by user requirement. Target is backward-compatible support for versions V4-V12.
- **Treat new records as harmless trailing metadata:** rejected. Feedback and cost are per-prototype; failing to consume them shifts the next prototype or main-function id.
- **Textual formatter issue causes current failure:** rejected. No AST or formatter stage is reached.

## Ranked General Improvements

1. Introduce validated bytecode-version context and version-aware bounded prototype parsing.
2. Parse debug records, v11 feedback, v12 cost data, and require complete chunk consumption.
3. Replace numeric instruction/AUX whitelists with exhaustive opcode metadata.
4. Add later constant payloads while preserving exact integer/vector values.
5. Add semantic lifter behavior for equivalent userdata/call opcodes and a fallback-path lowering for prototype guards.
6. Reconstruct prefilled `DUPTABLE` values from constant-table evidence.
7. Add real compiler profiles for V9-V12 and format fixtures for V4-V8.
8. Only then generate the full quality baseline and address CFG/SSA/readability failures.

## Hard-Coding Audit

Proposed rules depend on serialized version, official tag/opcode discriminants, declared record sizes, instruction encoding, and semantic opcode families. They do not inspect corpus filenames, literals, URLs, register numbers, or exact instruction sequences.

Version discriminants and payload layouts are required bytecode protocol data, not script-specific exceptions. Equivalent opcodes are grouped by semantics, and instruction length comes from exhaustive opcode metadata so any script using that opcode follows the same path.

## Alias Copy Elimination Verification (2026-07-27)

The post-SSA alias pass removes only statically proven, single-use local copies.
It recurses through structured blocks but retains a copy when a source write, an
effectful evaluation prefix, or a reference capture could change snapshot
semantics.

### Provenance and ordering policy

`GETIMPORT` roots retain `CompilerImport` provenance. Only that compiler-origin
import prefix may be reordered toward authored expression order. This is opcode
provenance, not a global-name whitelist. `GETGLOBAL` roots remain dynamic;
ordinary table/index operations remain metamethod-capable dynamic boundaries.
They therefore do not receive import-prefix reordering and block unsafe alias
elimination across the evaluation boundary.

### Static verification

- `cargo +stable fmt --all -- --check`: passed.
- `cargo +nightly test --workspace`: 40 passed, 0 failed.
- `python -m unittest discover -s tests/python -v`: 10 passed, 0 failed.
- V9-V12/current-profile compiler round trips: 240/240; 0 compile failures, 0
  decompile failures, 0 recompile failures, and 0 generated gotos.
- V4-V8 parser/format fixtures: passing.

The corpus runner invoked only the bundled compiler, the built decompiler, and
the bundled recompiler; no arbitrary Luau scripts ran.

`--no-build` initially exposed a missing `target/debug/luau-lifter.exe` in this
worktree (all decompiler launches returned `WinError 2`). Building the
decompiler with `cargo +nightly build -p luau-lifter` restored the expected
binary; the recorded 240-case result above is the fresh rerun.

### Alias metrics and representative output

`count_trivial_aliases` performs static text analysis of generated `.luau`
files only. Pre-change retained artifacts contain 66 aliases across `final-all`
(240 outputs) and 28 aliases across `final-versions` (96 outputs). Those
snapshots overlap in profile coverage, so their 94 aliases across 336 files are
an undeduplicated retained aggregate only; they are non-comparable to the fresh
matrix. The fresh `final-fix` matrix contains 48 aliases across 240
outputs. The sole like-for-like result is the matching 240-output profile set:
66 -> 48 aliases (18 fewer, 27.3%).

| Representative | Before locals / aliases | After locals / aliases | Evidence |
| --- | ---: | ---: | --- |
| V12 `24_wonky_integration` | 9 / 1 | 8 / 0 | `local v3 = v_u_1` disappeared; `setmetatable(..., v_u_1)` now uses the `Machine` table directly. |
| V12 `23_register_pressure_aliases` | 33 / 0 | 33 / 0 | Register-pressure locals are not trivial local-to-local aliases and remain intact. |
| O0/g1 `24_wonky_integration` | 8 / 0 | 8 / 0 | No qualifying alias existed, so output remains unchanged. |

The inspected after artifacts are:

```text
tests/luau_corpus/results/final-fix/V12/24_wonky_integration.luau
tests/luau_corpus/results/final-fix/V12/23_register_pressure_aliases.luau
tests/luau_corpus/results/final-fix/O0_g1/24_wonky_integration.luau
```

## Expression Recovery Verification (2026-07-27)

Expression recovery runs after alias elimination and before local declaration
placement. It uses AST identity, read/write sets, effect boundaries, and
single-result `Select` nodes; it does not inspect script names, constants,
URLs, or generated register numbers.

The pass now:

- emits first-class Luau conditional expressions, including `elseif` chains,
  instead of encoding falsy branches with unsafe `and`/`or` expressions;
- rebuilds exact adjacent `and`/`or` assignment chains;
- inlines a single-use expression only before the first observable consumer
  operation and without crossing calls, indexing, metamethod-capable
  expressions, source writes, structured control flow, or reference captures;
- retains parentheses around a final selected call result, such as
  `return (produce())`, so one result cannot become an open multi-return;
- formats exact local and stable indexed updates with Luau compound operators.

A static compiler probe showed that stable local-backed
`target[key] = target[key] + value` and `target[key] += value` both lower to
`GETTABLE`, the arithmetic operation, and `SETTABLE`; only temporary register
allocation differs.

### Static verification

- `cargo +nightly test --workspace --offline`: 76 passed, 0 failed.
- `python -m unittest discover -s tests/python -v`: 10 passed, 0 failed.
- V9-V12/current-profile compiler round trips: 240/240; 0 source compile
  failures, 0 decompile failures, 0 recompile failures, and 0 generated gotos.
- The corpus tools compiled, decompiled, and recompiled files only. They did
  not execute authored or generated Luau.

### Comparable corpus metrics

Both snapshots contain the same 24 sources and 10 profiles. The corpus runner
counts nonblank generated `.luau` lines. Conditional and compound counts use
the canonical single-line syntax emitted by the formatter.

| Metric | Before | After | Change |
| --- | ---: | ---: | ---: |
| Generated nonblank lines | 4,996 | 4,707 | -289 (-5.8%) |
| Generated locals | 922 | 879 | -43 (-4.7%) |
| Trivial aliases | 48 | 32 | -16 (-33.3%) |
| Conditional expressions | 0 | 148 | +148 |
| Compound assignments | 0 | 138 | +138 |
| Generated gotos | 0 | 0 | unchanged |

Representative changes include:

```luau
-- before
local v10
if p4 < 0 then
	v10 = -p4
else
	v10 = p4
end

-- after
local v10 = if p4 < 0 then -p4 else p4
```

```luau
-- before
p6.total = p6.total + (v8 or 0)
v19 = v19 + 1

-- after
p6.total += v8 or 0
v19 += 1
```

The local before/after reports are generated, ignored artifacts at
`tests/luau_corpus/results/expression-before/` and
`tests/luau_corpus/results/expression-after/`.
