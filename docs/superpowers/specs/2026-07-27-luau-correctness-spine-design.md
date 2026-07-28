# Luau Correctness Spine Design

## Objective

Repair the existing Luau decompiler in a correctness-first order so generated
source preserves recoverable runtime behavior and then approaches the most
natural source form supported by bytecode evidence.

The existing corpus remains the primary source of truth. Testing stays focused:
each confirmed defect receives the smallest useful regression, relevant trusted
fixtures receive differential execution, and the existing Rust, Python, and
260-case compile/decompile/recompile gates remain intact.

## Boundaries

- Keep the current lifter, CFG, SSA, AST, and formatter architecture.
- Add first-class semantic information where the existing representation loses
  it; do not introduce a replacement IR.
- Never execute arbitrary user bytecode or arbitrary decompiled scripts.
- Differential execution is limited to trusted repository-authored fixtures.
- Arbitrary inputs receive static validation only.
- Reconstruction rules use bytecode, CFG, SSA, dataflow, debug metadata, and
  Luau grammar semantics. They may not inspect fixture names, exact constants,
  generated register numbers, URLs, or expected formatted text.
- Exact recovery is not claimed for comments, erased types, stripped names,
  optimized-away assignments, or compiler-inlined helper boundaries.

## Acceptance Cases

These reproduced failures are mandatory gates:

| Fixture | Original behavior | Current decompiled behavior | Required result |
| --- | --- | --- | --- |
| `04_calls_multireturn` | returns `q=5:4`, `12` | arithmetic-on-`nil` error | exact returned values, no error |
| `05_varargs` | preserves all selected and forwarded values | several values become `nil` | exact value count and order |
| `13_repeat_until` | returns `10`, `6` for the audited input | returns `nil`, `6` | exact returned values and no generated-global read |
| `15_generic_for` | returns the collected table | `__FENV["type"]` runtime error | equivalent table and direct valid `type` access |
| `20_pcall_style_flow` | returns `7`, `8` for the audited callbacks | returns `nil`, `nil` | exact returned values |
| `21_state_machine` under `O0/g1` | returns `done`, `1`, `3` | returns no values | exact returned values and valid local bindings |

Source recompilation alone does not satisfy these gates.

## Architecture

The correctness spine retains the current pipeline:

```text
deserialize
  -> lift bytecode
  -> CFG and SSA
  -> SSA destruction
  -> graph restructuring
  -> AST recovery
  -> declaration placement
  -> naming and formatting
```

Work proceeds from semantic invariants toward presentation. A later phase may
not compensate for an earlier semantic defect.

### Phase 1: Result-Width Correctness

Represent expression-list cardinality explicitly:

- `One`: exactly one result, including an intentionally selected call or
  vararg;
- `Fixed(n)`: exactly `n` results required by bytecode;
- `Open`: the final call or vararg supplies the remaining values.

This information originates from `CALL`, `CALLFB`, `NAMECALL`, `VARARG`, return,
argument, assignment, and table-list bytecode. It survives SSA construction,
inlining, restructuring, AST recovery, and formatting.

Parentheses may enforce `One`. They must never be used for `Fixed(n)` or `Open`
when doing so truncates values. Assignment target count is not a substitute for
carrying source cardinality through transformations.

Phase acceptance:

- fixtures `04`, `05`, and `20` match original behavior;
- no multi-target assignment is formatted with a one-result wrapper unless
  bytecode explicitly selected one result;
- focused unit tests cover calls, method calls, and varargs in assignment,
  argument, table-tail, and return contexts.

### Phase 2: Scope and Identifier Correctness

Declaration placement uses every definition, read, capture, and loop-exit use.
The selected declaration scope is the least enclosing lexical region that
dominates all required uses without changing capture behavior.

A final binding validator rejects output when an intended local reference would
resolve as a global. Generated fallback identifiers such as `v4` and `p2` may
not appear as unresolved global accesses.

Identifier validation becomes context-specific:

- local and parameter identifiers;
- global expression identifiers;
- function and method names;
- dotted member names;
- table constructor fields;
- type declaration names.

Luau contextual words that are valid in an expression or field context remain
direct syntax there. Truly unspellable global names use a valid environment
access rather than the synthetic undeclared `__FENV` name.

Phase acceptance:

- fixtures `13` and `15` match original behavior;
- the `O0/g1` state-machine output contains no escaped generated local;
- compiling generated output introduces no synthetic `vN`/`pN` global reads;
- `type(key)` remains a valid direct builtin call;
- valid table field syntax such as `class = value` is retained.

### Phase 3: Register and Source Provenance

The lifter creates register-versioned values rather than one function-wide
identity per physical register. Each value may carry:

- definition instruction position;
- source line when present;
- debug-local start and end positions;
- opcode and operand origin;
- physical register for diagnostics only.

SSA merges retain the contributing provenance set. Dataflow remains the
semantic authority; debug and line information provide advisory evidence for
scope, statement grouping, names, and function boundaries.

Provenance is unavailable in stripped bytecode and therefore cannot be required
for correct decompilation.

Phase acceptance:

- disjoint debug-local lifetimes sharing one register remain distinct;
- names never leak outside their debug lifetime;
- parallel/source-line grouping uses provenance only when it agrees with
  dataflow;
- `g0` output remains valid and stable without debug evidence.

### Phase 4: SSA and CFG Source Reconstruction

Recover source constructs before destroying the strongest evidence. Recovery
uses SSA def-use chains, dominance, post-dominance, loop regions, capture
groups, result widths, and location-aware effect summaries.

Effect summaries distinguish:

- calls and metamethod-capable operations;
- local and upvalue reads and writes;
- table-root reads, writes, and escapes;
- allocation and closure capture;
- result cardinality and open-list behavior.

The pass worklist runs to a deterministic bounded fixed point. Each pass reports
whether it changed dataflow or CFG topology and invalidates only the analyses it
actually affects.

Target recoveries:

- `repeat ... until`;
- short-circuit and conditional expressions;
- shared loop exits, `break`, and `continue`;
- state-machine loops without duplicated terminal returns;
- parallel assignments;
- safe aliases and expression inlining;
- table constructors and member installation;
- named helpers retained by provenance or reconstructed from repeated,
  semantically equivalent regions when safe.

Candidate structured forms are scored by semantic validity first, then
unsupported jumps, duplicated effects, nesting, generated locals, source-order
continuity, and readability.

Phase acceptance:

- fixture `13` uses a natural repeat-loop form when supported by its CFG;
- fixture `21` has one valid state-machine loop and matches original behavior
  under every profile;
- optimized short-circuit output preserves call order while avoiding avoidable
  branch expansion;
- no transformation duplicates, removes, or reorders observable work.

### Phase 5: Names, Functions, Methods, and Formatting

Naming and syntax recovery use a scored evidence graph containing:

- valid debug names and lifetimes;
- function metadata;
- member assignment targets;
- `NAMECALL` call sites;
- receiver dataflow;
- `__index` class patterns;
- capture relationships;
- exported table keys;
- iterator, callback, result, state, key, value, and options roles.

A name or colon-method form is accepted only when evidence has one dominant
interpretation. Conflicts retain neutral generated names.

Named function boundaries supported by metadata or repeated use are preserved
instead of being inlined into anonymous returned closures. Formatting may
introduce a local for readability when a fully inlined expression would create
an excessively long or deeply nested statement.

Upvalue annotations and other internal diagnostics are disabled in normal
output and available through an explicit diagnostic mode.

Phase acceptance:

- recovered methods use consistent declaration and call syntax;
- `25_product_controller` recovers every method with sufficient receiver and
  call-site evidence;
- high-confidence class, callback, result, iterator, and state names replace
  fallbacks;
- line wrapping and temporary introduction improve readability without
  altering evaluation order or result width.

## Verification Strategy

Testing remains proportional and direct.

For each phase:

1. Add one or two focused failing regressions for each confirmed root cause.
2. Run the focused test and confirm the intended failure.
3. Implement the smallest general rule.
4. Re-run the focused test.
5. Compare representative original and decompiled source directly.
6. Run affected trusted runtime probes.
7. Run existing Rust and Python suites.
8. Run the full 260-case compile/decompile/recompile matrix.

Static semantic checks cover:

- call and vararg result widths;
- unresolved generated identifiers;
- local binding identity and capture mode;
- return widths;
- global/import access shape;
- observable operation count and order where a pass changes expressions.

No general arbitrary-script execution harness, grammar fuzzer, or large new
fixture suite is part of this design.

## Error Handling

- A semantic invariant violation returns a localized decompiler error rather
  than emitting plausibly valid but incorrect source.
- Diagnostics identify function, instruction position when known, invariant,
  and reconstruction phase.
- One failed corpus case does not stop remaining cases.
- Corpus reporting distinguishes blocked toolchain setup, source compile
  failure, decompile failure, recompile failure, and semantic mismatch.

## Delivery Order

Each phase is independently reviewed, tested, committed, and merged before the
next phase begins:

1. result width;
2. scope and contextual identifiers;
3. register/source provenance;
4. SSA/CFG reconstruction;
5. naming and formatting.

If a phase exposes a lower-layer semantic defect, work returns to the earliest
affected phase. Readability improvements never bypass unresolved correctness
failures.
