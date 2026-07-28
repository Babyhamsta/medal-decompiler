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

- `cargo +nightly test --workspace --offline`: 78 passed, 0 failed.
- `python -m unittest discover -s tests/python -v`: 10 passed, 0 failed.
- V9-V12/current-profile compiler round trips: 240/240; 0 source compile
  failures, 0 decompile failures, 0 recompile failures, and 0 generated gotos.
- The corpus tools compiled, decompiled, and recompiled files only. They did
  not execute authored or generated Luau.

### Comparable corpus metrics

Both snapshots contain the same 24 sources and 10 profiles. The corpus runner
counts nonblank generated `.luau` lines. Conditional and compound counts use
the canonical single-line syntax emitted by the formatter. Split short-circuit
pairs are adjacent assignments followed by a matching `if local` or
`if not local` block that reassigns the same local.

| Metric | Before | After | Change |
| --- | ---: | ---: | ---: |
| Generated nonblank lines | 4,996 | 4,707 | -289 (-5.8%) |
| Generated locals | 922 | 879 | -43 (-4.7%) |
| Trivial aliases | 48 | 32 | -16 (-33.3%) |
| Conditional expressions | 0 | 148 | +148 |
| Compound assignments | 0 | 138 | +138 |
| Split short-circuit assignment/`if` pairs | 8 | 0 | -8 (-100.0%) |
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

## Function and Table Recovery Verification (2026-07-27)

The corpus now includes two advanced truth sources: a product-style controller
and an adversarial dataflow module. Together they cover static constructors,
receiver methods, callback fields, recursive and forward closures, dynamic
keys, snapshots, incremental tables, and open multi-return boundaries. This
expands the comparable matrix from 240 to 260 static rows.

Recovery uses AST shape, closure debug metadata, receiver-use evidence,
read/capture identity, SSA capture groups, effect analysis, and multi-return
selection. It does not inspect fixture names, source constants, URLs, register
numbers, or script-specific instruction sequences.

The pass now:

- emits scoped `local function` declarations for recursive locals, including
  unnamed self-capturing closures;
- emits dotted named functions when debug metadata supports them;
- emits colon methods only for a matching dotted closure with strong first-
  parameter receiver evidence, while retaining static constructors as dotted
  functions;
- keeps anonymous callback fields as `field = function(...)` assignments;
- folds adjacent incremental table fields when the target cannot be observed
  through an SSA capture group;
- preserves separate writes for target-capturing closures, effectful writes to
  capture-observable tables, open call/vararg tails, mixed positional key
  collisions, and fractional numeric keys.

### Static verification

- `cargo +nightly test --workspace --offline`: 95 passed, 0 failed.
- `python -m unittest discover -s tests/python -v`: 10 passed, 0 failed.
- All profiles and compatibility versions: 260/260; 0 source compile failures,
  0 decompile failures, 0 recompile failures, and 0 generated gotos.
- Three independent reviews passed after every Critical and Important finding
  was repaired.
- The corpus tools compiled, decompiled, and recompiled files only. They did
  not execute authored or generated Luau.

### Comparable corpus metrics

Both snapshots contain the same 26 sources and 10 profiles.

| Metric | Before | After | Change |
| --- | ---: | ---: | ---: |
| Generated nonblank lines | 6,871 | 6,851 | -20 |
| Generated locals | 1,179 | 1,189 | +10 |
| Trivial aliases | 72 | 72 | unchanged |
| Named function declarations | 262 | 206 | -56 |
| Receiver method declarations | 0 | 48 | +48 |
| Anonymous function assignments | 98 | 154 | +56 |
| Aggregate tab indentation | 11,480 | 11,360 | -120 |
| Generated gotos | 0 | 0 | unchanged |

The local increase is intentional: one preserved table local per profile keeps
an open call tail from changing result width, while one preserved registry
local per profile keeps a callback bound to the correct lexical table. The 56
anonymous assignments replace false named-function declarations rather than
adding new closures.

The adversarial source exposed two transformations that ordinary
compile/recompile acceptance missed. These now remain separate:

```luau
local values = { source.seed() }
values.status = "ready"

local registry = {}
registry.current = function()
	return registry
end
```

The local before/after reports are generated, ignored artifacts at
`tests/luau_corpus/results/function-table-before/` and
`tests/luau_corpus/results/function-table-after/`.

## Control-flow cleanup

The control-flow pass runs after expression recovery and before local
declarations. It uses AST structure and side-effect evidence rather than
source names or fixture patterns:

- nested single-`else` conditionals continue to format dynamically as
  `elseif`;
- empty `then` branches are inverted without rewriting relational operators;
- empty conditionals are removed only when evaluating the condition is
  provably unobservable;
- terminal `return`, `break`, `continue`, `goto`, or fully terminal nested
  branches become guard clauses;
- complex loop-tail conditionals become `continue` guards, while simple
  one-statement tail conditionals retain their source-like `if` shape;
- loop cleanup reaches a local fixed point when one recovered guard exposes a
  nested eligible guard.

### Static verification

- `cargo +nightly test --workspace --offline`: 106 passed, 0 failed.
- `python -m unittest discover -s tests/python -v`: 10 passed, 0 failed.
- All profiles and compatibility versions: 260/260; 0 source compile failures,
  0 decompile failures, 0 recompile failures, and 0 generated gotos.
- Three independent reviews passed after both Important findings were repaired.
- The corpus tools compiled, decompiled, and recompiled files only. They did
  not execute authored or generated Luau.

### Comparable corpus metrics

Both snapshots contain the same 26 sources and 10 profiles.

| Metric | Before | After | Change |
| --- | ---: | ---: | ---: |
| Generated nonblank lines | 6,851 | 6,813 | -38 |
| Generated locals | 1,189 | 1,189 | unchanged |
| Trivial aliases | 72 | 72 | unchanged |
| `if` branches | 343 | 343 | unchanged |
| `elseif` branches | 60 | 60 | unchanged |
| Standalone `else` branches | 108 | 50 | -58 |
| Recovered `continue` guards | 0 | 20 | +20 |
| Empty textual branches | 0 | 0 | unchanged |
| Aggregate tab indentation | 11,360 | 10,868 | -492 |
| Maximum tab indentation | 6 | 6 | unchanged |
| Generated gotos | 0 | 0 | unchanged |

Only 50 of 260 outputs changed, across the five control-flow-heavy truth
sources. Simple generic-for conditionals are byte-for-byte unchanged from the
input branch. The local before/after reports are generated, ignored artifacts
at `tests/luau_corpus/results/control-flow-before/` and
`tests/luau_corpus/results/control-flow-after/`.

## Dynamic naming

Naming now runs after function recovery and uses general AST/debug evidence
rather than fixture names or register-number rules:

- valid function and upvalue debug names survive lifting and SSA;
- a register debug name is accepted only when exactly one valid record covers
  the whole function, preventing a scoped name from leaking across register
  reuse;
- conflicting, invalid, reserved, or colliding names fall back safely;
- numeric loops, recognized `pairs`/`ipairs` loops, returned tables, invoked
  parameters, and callback collections receive conservative role names;
- unknown generic iterators and ambiguous roles keep generated names;
- generated closure/upvalue fallbacks no longer expose `_u_` implementation
  markers;
- a captured outer `self` cannot produce a colon method whose omitted receiver
  is renamed and therefore unbound.

The conservative whole-function rule intentionally leaves scoped debug locals
unclaimed until the lifter has program-counter-aware local identities. This
loses some available spelling evidence but prevents misleading names and
semantic rebinding.

### Static verification

- `cargo +nightly test --workspace --offline`: 116 passed, 0 failed.
- `python -m unittest discover -s tests/python -p 'test_*.py'`: 10 passed,
  0 failed.
- All profiles and compatibility versions: 260/260; 0 source compile failures,
  0 decompile failures, 0 recompile failures, and 0 generated gotos.
- Focused `O1/g0`, `O1/g1`, and `O1/g2` product-style outputs recompile.
- Three independent semantic, naming, and corpus reviews passed after every
  Critical and Important finding was repaired.
- The corpus tools compiled, decompiled, and recompiled files only. They did
  not execute authored or generated Luau.

### Comparable corpus metrics

Both snapshots contain the same 26 sources and 10 profiles. Generated-name
counts are identifier-reference occurrences matching `vN`, `pN`, `v_u_N`, or
`p_u_N`. Role references use the selected conservative role vocabulary.

| Metric | Before | After | Change |
| --- | ---: | ---: | ---: |
| Generated nonblank lines | 6,813 | 6,813 | unchanged |
| Generated locals | 1,189 | 1,189 | unchanged |
| Trivial aliases | 72 | 72 | unchanged |
| Generated-name references | 9,018 | 7,212 | -1,806 (-20.0%) |
| Generated upvalue-marker references | 2,693 | 0 | -2,693 (-100.0%) |
| Conservative role references | 224 | 1,062 | +838 |
| `O1/g2` generated-name references | 891 | 302 | -589 (-66.1%) |
| Aggregate tab indentation | 10,868 | 10,868 | unchanged |
| Maximum tab indentation | 6 | 6 | unchanged |
| Generated gotos | 0 | 0 | unchanged |

Representative `O1/g2` output now retains source-level structure and names:

```luau
local function mergeOptions(overrides)
	for key, value in pairs(overrides) do
		result[key] = value
	end
	return result
end

function Controller:use(callback)
	self.middleware[#self.middleware + 1] = callback
	return self
end
```

The local before/after reports are generated, ignored artifacts at
`tests/luau_corpus/results/naming-before/` and
`tests/luau_corpus/results/dynamic-naming-after/`.
