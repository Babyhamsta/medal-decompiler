# Luau Correctness Spine Design

## Status and Precedence

This is the umbrella design for correctness-first source reconstruction. Where
it conflicts with either of these earlier documents, this design controls phase
order, acceptance gates, and delivery boundaries:

- `2026-07-27-luau-decompiler-quality-design.md`
- `2026-07-27-luau-source-reconstruction-design.md`

Their generalization, safety, and readability goals still apply. Earlier pass
contracts remain independently testable and idempotent unless a phase plan
explicitly replaces them.

## Objective

Repair the existing Luau decompiler from the lowest semantic invariant upward
so generated source first preserves recoverable runtime behavior and then
approaches the most natural form supported by bytecode evidence.

The current test scripts remain the primary corpus. This work adds a narrow
semantic gate around six confirmed failures, not a second test platform or a
large fixture expansion.

## Boundaries

- Keep the current lifter, CFG, SSA, restructuring, AST, and formatter
  architecture.
- Add a correctness spine through those layers; do not introduce a replacement
  IR.
- Never execute arbitrary user bytecode or arbitrary decompiled scripts.
- Differential execution is restricted to an explicit allowlist of six
  repository-authored fixtures and their fixed probes.
- Arbitrary inputs receive compilation and internal invariant validation only.
- Reconstruction rules may use bytecode, CFG, SSA, dataflow, debug metadata,
  source locations, and Luau language semantics.
- Reconstruction rules may not inspect fixture names, probe values, expected
  results, exact constants, generated register numbers, URLs, or expected
  formatted text.
- Comments, erased types, stripped names, optimized-away assignments, and
  compiler-inlined helper boundaries are not always recoverable.
- When source authorship cannot be proven, prefer semantically correct,
  conventional Luau over fabricated specificity.

## Phase 0 Acceptance Harness

Before changing decompilation semantics, extend the existing corpus runner with
a trusted semantic mode.

### Allowlist

The runner owns a small manifest that maps only the six accepted repository
fixtures to a fixed invocation and supported profiles. The manifest and probe
rules are test infrastructure and are never passed into the decompiler.

At baseline, run the profile that reproduced each failure. When a phase repairs
a fixture, run its probe across every existing corpus profile for which the
fixture is present:

- `O0_g1`
- `O1_g0`
- `O1_g1`
- `O1_g2`
- `O2_g0`
- `O2_g1`
- `V9`
- `V10`
- `V11`
- `V12`

This is at most sixty fixture/profile executions at the final gate.

### Exact Probes

All returned values are captured with `table.pack` so result count and interior
`nil` values remain observable.

| Fixture | Baseline profile | Fixed invocation | Required normalized result |
| --- | --- | --- | --- |
| `04_calls_multireturn` | `O1_g1` | `run(20)` | `n=2`, `"q=5:4"`, `12` |
| `05_varargs` | `O1_g1` | `forward("p", 1, 2, 3, 4)` | `n=11`: `"p"`, `4`, `1`, `2`, `3`, `{2,3,4}`, `4`, `1`, `2`, `3`, `{2,3,4}` |
| `13_repeat_until` | `O1_g1` | `converge(30)` | `n=2`, `10`, `6` |
| `15_generic_for` | `O1_g1` | `collect({2, 4, name="kept"})` | `n=1`, table `{4,8,20,12,6,2,0,name="kept"}` |
| `20_pcall_style_flow` | `O1_g1` | the three callback paths below | all three packed results match |
| `21_state_machine` | `O0_g1` | `run({"start", "tick", "stop"})` | `n=3`, `"done"`, `1`, `3` |

The three `20_pcall_style_flow` paths are:

1. success callback returns `7, 8`; expected `n=2`, `7`, `8`;
2. callback returns `nil, "reason"` and fallback returns `9, 10`; expected
   `n=2`, `9`, `"recovered"`;
3. callback returns `nil, "reason"` and fallback returns
   `nil, "still missing"`; expected `n=2`, `nil`, `"still missing"`.

Normalization compares:

- exact top-level return count;
- ordered scalar values, including `nil`;
- nested acyclic tables by keys and values, ignoring table identity and key
  iteration order;
- source and generated runtime exit status and error category.

Functions, userdata, cyclic tables, timing, and printed text are outside these
six probes.

### Reporting

For an allowlisted run, retain the existing bytecode, generated source, and log.
Add:

- `[source-runtime]` and `[generated-runtime]` log sections;
- runtime exit status;
- normalized result;
- `semantic_match` in the JSON and Markdown summaries.

One fixture failure does not stop the corpus. Arbitrary cases never enter
trusted semantic mode.

### Structured Decompiler Errors

Phase 0 also removes failure comments masquerading as source. Function
decompilation returns a structured `Result` through the call chain. A localized
failure records function, instruction position when known, invariant, and
phase. The corpus runner reports it as a decompile failure and continues.

Normal output never contains a diagnostic comment in place of a function body.

## Correctness Spine

The pipeline remains:

```text
deserialize
  -> lift bytecode and attach immutable origins
  -> CFG and SSA
  -> derive immutable recovery facts
  -> destroy SSA
  -> graph restructuring
  -> AST recovery
  -> final declaration placement
  -> naming and formatting
```

Implementation phases follow dependency order. A later phase may consume facts
created earlier, but it may not guess around a missing lower-layer invariant.

## Phase 1: Result Groups and List Semantics

A multi-result operation is one indivisible producer group, not several
unrelated scalar expressions.

Each result group carries:

- producer identity and origin;
- `ResultDemand::Exact(usize)` or `ResultDemand::Open`;
- ordered destination projections;
- the list context that consumed it;
- its legal materialization forms.

`Exact(0)` preserves a side-effect-only producer. `Exact(1)` selects one value.
`Exact(n)` for `n > 1` truncates extra values and preserves Luau `nil` padding
when the producer yields fewer. `Open` allows the producer to supply the
remaining values in a list.

Only the final expression of an assignment, argument list, return list, or
table list may remain `Open`. If a transformation moves an open producer away
from the tail, it must first materialize the exact width demanded by bytecode.

The group and demand originate from `CALL`, `CALLFB`, `NAMECALL`, `VARARG`,
return, assignment, argument, and table-list bytecode. They survive SSA,
inlining, restructuring, AST recovery, and formatting. SSA projections may
refer to an element of a group, but no pass may duplicate the producer to
manufacture those projections.

Parentheses are permitted to express `Exact(1)`. They may not wrap the tail of
an `Exact(n)` group or an `Open` group when doing so changes cardinality.
Bytecode does not preserve authored parentheses, so the decompiler does not
claim to recover them.

Phase acceptance:

- exact probes `04`, `05`, and all three paths of `20` pass;
- a focused Rust matrix covers fixed and open `CALL`, `NAMECALL`, and `VARARG`
  behavior plus `Exact(1)` formatting;
- inlining, CFG edge arguments, returns, calls, assignments, and table tails
  retain the same producer group and demand;
- producer-origin validation proves no result-producing call was duplicated.

## Phase 2: Immutable Source Provenance

Do not create an independent register-versioning system in the lifter. Physical
register identity remains the family key used by SSA reaching definitions; SSA
construction remains the authority that creates definition versions.

Before SSA construction, the lifter attaches an immutable origin to each
instruction-level definition, use, and emitted statement:

- instruction position;
- source line when present;
- debug-local lifetime when present;
- opcode and operand origin;
- physical register family;
- closure or capture origin where applicable.

SSA-created definitions inherit their defining origin. Phi and equivalent
merges retain the union of contributing origins. Coalescing, SSA destruction,
restructuring, and AST lowering preserve those origin sets or a stable reference
to them.

Provenance is advisory for names and source shape. Dataflow remains semantic
authority. Missing debug data must reduce confidence, not correctness.

Phase acceptance:

- one reused-register test proves disjoint definition lifetimes stay distinct;
- one merge test proves all contributing origins survive SSA destruction;
- debug names cannot escape their recorded lifetimes;
- one `g0` case recompiles deterministically without debug evidence.

## Phase 3: Identifier Legality and Binding Identity

This phase separates lexical legality from final declaration placement.

Every reference carries an intended binding identity: local, parameter,
upvalue, global, import, or member. An early validator rejects any lowering
that changes this identity, such as turning a local reference into a global.
It does not yet decide the final lexical declaration region.

Identifier spelling is context-specific:

- local and parameter identifiers;
- global expression identifiers;
- function names;
- method names;
- dotted member names;
- table constructor fields;
- type declaration names.

Luau contextual words remain direct syntax where their context permits it.
Therefore `type(key)` is emitted as a direct builtin call and
`class = value` remains a valid table field.

For a genuinely unspellable global, use environment indexing only if a focused
compiler/runtime probe proves the chosen form has the required semantics for
the supported Luau toolchain. `getfenv(0)[escaped_name]` is a candidate, not an
assumption. If no valid form is proven, return a localized unsupported-identifier
error. Never invent an undeclared `__FENV`.

Phase acceptance:

- exact probe `15` passes across the existing profiles;
- one binding-validator unit test catches an intended-local/generated-global
  mismatch;
- focused lexical tests cover `type`, `class`, method names, and unspellable
  global handling;
- generated source contains no synthetic undeclared environment name.

## Phase 4: SSA and CFG Source Reconstruction

Phase 4 is split into bounded delivery units so the strongest evidence is
captured before SSA destruction and each transformation can be verified alone.

### Phase 4A: Recovery Facts and Pass Contract

Before SSA destruction, derive immutable `RecoveryFacts` containing stable
references to:

- result groups;
- definition/use and origin sets;
- dominance and post-dominance;
- loop and candidate region membership;
- edge arguments and merge relationships;
- binding and capture identities;
- location-aware effect summaries.

Later passes consume `RecoveryFacts`; they do not query destroyed SSA nodes.

Effect summaries distinguish:

- calls and metamethod-capable operations;
- local and upvalue reads and writes;
- table-root reads, writes, and escapes;
- allocation and closure capture;
- result demand and open-list behavior.

Each transformation returns a `PassChange` describing dataflow, CFG topology,
region, and AST-shape changes. The scheduler invalidates only dependent
analyses, reruns to a deterministic bounded fixed point, and detects repeated
states instead of silently relying on a pass-count ceiling.

Phase 4A acceptance:

- every pass reports a precise change set;
- a no-change rerun is idempotent;
- changed CFG and dataflow invalidate the correct facts;
- a synthetic oscillation is reported as a localized reconstruction error.

### Phase 4B: Expression and Dataflow Reconstruction

Using result groups, effect summaries, and immutable recovery facts, recover:

- short-circuit and conditional expressions;
- parallel assignments;
- safe aliases and expression inlining;
- table constructors and member installation;
- shared producer lists without duplicated evaluation.

A rewrite is legal only when it preserves result demand, binding identity,
operation count, and observable order. It must also preserve location-aware
table aliasing and escape constraints.

Phase 4B acceptance:

- the trusted `10_short_circuit` probe
  `run(false, true, true)` returns `false, true, true` with log
  `{"a", "x", "y", "fallback"}`;
- effect-origin checks prove each source operation appears once after rewriting;
- direct review confirms the three short-circuit values are expressed without
  avoidable branch scaffolding;
- the review uses the generated source, not a brittle full-file snapshot.

### Phase 4C: Region and Control-Flow Reconstruction

Using dominance, post-dominance, loop regions, exits, and merge facts, recover:

- `repeat ... until`;
- `while` and generic loop regions;
- shared loop exits, `break`, and `continue`;
- state-machine loops without duplicated terminal paths;
- reachable terminal returns.

Candidate forms are admitted only when their region boundaries and edge
semantics match the CFG. Readability scoring may choose among valid forms, but
cannot make an invalid form eligible.

Phase 4C acceptance:

- fixture `13` has one `repeat ... until` region and no synthetic outer loop;
- fixture `21` has one state-machine loop, no nested always-true scaffold, and
  one reachable terminal return;
- structural assertions inspect AST/region shape rather than formatted text;
- no exit, return, or observable operation is duplicated or dropped.

## Phase 5: Final Declaration Placement

Final declaration placement runs only after Phase 4 establishes lexical
structure.

It uses every definition, read, capture, merge, condition use, and loop-exit
use. The chosen declaration region is the least enclosing lexical region that:

- dominates every required use;
- preserves initialization along each reachable path;
- preserves capture identity and lifetime;
- respects Luau loop scopes, including `repeat` condition visibility;
- does not merge distinct SSA definitions merely because they reused a physical
  register.

The final binding validator resolves the generated AST exactly as Luau would
and compares every reference with its intended binding identity. Compilation
is still required, but is not considered proof of correct binding.

Phase acceptance:

- exact probes `13` and `21` pass across the existing profiles;
- the already repaired `15` binding remains valid;
- no generated `vN` or `pN` local reference resolves as a global;
- focused scope tests cover a repeat-condition local, a loop-carried value, and
  a captured value without adding new corpus fixtures.

## Phase 6: Names, Functions, Methods, and Formatting

Naming and syntax recovery use a scored evidence graph containing:

- valid debug names and lifetimes;
- function metadata and origins;
- member assignment targets;
- `NAMECALL` call sites;
- receiver dataflow;
- `__index` class patterns;
- capture relationships;
- exported table keys;
- iterator, callback, result, state, key, value, and options roles.

A name or colon-method form is accepted only when evidence has one dominant
interpretation. Conflicts retain a neutral generated name.

Named function boundaries supported by metadata or repeated use are retained
instead of being inlined into anonymous returned closures. Formatting may
introduce a local or wrap an expression only when result demand and evaluation
order remain unchanged.

Upvalue annotations and internal diagnostics are disabled in normal output and
available only through an explicit diagnostic mode.

Phase acceptance for `25_product_controller` is exact:

- declarations recover `Controller.new`, `Controller:use`, `Controller:on`,
  `Controller:_applyMiddleware`, and `Controller:dispatch`;
- calls recover `self:_applyMiddleware(...)` and `self:on(...)`;
- normal output contains no `-- upvalues` diagnostic;
- the generated file passes the existing compile/decompile/recompile gate.

Fixtures `10`, `22`, `23`, and `26` receive direct source review for expression
size, helper boundaries, fallback-name density, and duplicated helper bodies.
These reviews are recorded as evidence; no full-file snapshots or
fixture-specific rewrite rules are introduced.

## Cumulative Verification

Tests stay lean and tied to confirmed risks.

Minimum focused additions:

- Phase 0: allowlist/normalizer coverage and structured error propagation;
- Phase 1: one small result-demand matrix plus probes `04`, `05`, and `20`;
- Phase 2: reused-register, merge-provenance, and deterministic `g0` checks;
- Phase 3: binding mismatch and contextual-identifier checks plus probe `15`;
- Phase 4A: pass idempotence, invalidation, and cycle detection;
- Phase 4B: one effect-order trace for fixture `10`;
- Phase 4C: structural assertions for fixtures `13` and `21`;
- Phase 5: one focused declaration-placement matrix plus probes `13` and `21`;
- Phase 6: the exact `Controller` structural check and diagnostic-mode check.

After each phase:

1. run its focused Rust or Python tests;
2. inspect the affected generated source;
3. run every six-case probe that has passed in an earlier phase;
4. run the newly repaired probe across all ten existing profiles;
5. run all existing Rust tests;
6. run all existing Python tests;
7. run the existing 260-case compile/decompile/recompile matrix.

A later phase fails if it regresses any earlier semantic probe, even when all
generated files compile.

Static checks are intentionally narrow:

- result group and demand consistency;
- binding identity resolution;
- return widths;
- global/import access shape;
- effect-origin count and order for passes that rewrite expressions;
- AST region shape for the repeat and state-machine cases.

This design does not add a generic semantic equivalence analyzer, arbitrary
script runtime, grammar fuzzer, large new fixture suite, or broad snapshot
suite.

## Error Handling

- Semantic invariant violations return structured errors instead of plausible
  but incorrect source.
- Errors identify function, phase, invariant, and instruction/origin when
  available.
- Corpus output distinguishes toolchain setup, source compile, decompile,
  generated recompile, source runtime, generated runtime, and semantic mismatch
  failures.
- Diagnostic mode may expose internal details; normal output remains valid
  source or a decompile error.

## Delivery Units

The detailed implementation work is divided into dependency-ordered plans:

1. trusted six-probe gate and structured error plumbing;
2. result groups and list semantics;
3. immutable source provenance;
4. contextual identifiers and early binding identity;
5. recovery facts and pass scheduler;
6. expression and dataflow reconstruction;
7. region and control-flow reconstruction;
8. final declaration placement and binding validation;
9. names, methods, helper boundaries, and formatting.

Each unit is independently reviewed, tested, committed, and merged before its
dependent unit starts. Phase 4 is deliberately three plans, not one broad
rewrite.

If a unit exposes a lower-layer semantic defect, work returns to the earliest
affected invariant. Readability work never bypasses an unresolved correctness
failure.
