# Luau Decompiler Quality Design

## Objective

Make decompiled Luau substantially more structured, readable, and source-like across arbitrary scripts. Exact source recovery is not possible when compilation removes names, comments, types, syntactic choices, or entire expressions. Improvements must recover the most natural equivalent program supported by bytecode evidence.

## Non-Goals

- Reproduce comments, formatting, or identifiers absent from bytecode.
- Match a fixture through constants, URLs, register numbers, instruction offsets, or exact source text.
- Rewrite the decompiler wholesale before corpus evidence shows existing architecture cannot support a required improvement.
- Treat textual similarity alone as proof of semantic equivalence.

## Generalization Rule

Every reconstruction rule must be expressed using general evidence such as:

- control-flow graph topology;
- dominance and post-dominance;
- SSA definitions, uses, and phi-like merges;
- side-effect and evaluation-order constraints;
- register liveness and local lifetime;
- opcode semantics;
- constant type and role;
- call arity and return arity;
- closure and upvalue relationships;
- debug metadata when present.

A change is rejected if its justification depends on a particular corpus file, literal value, generated register number, or fixed instruction sequence that is not required by Luau bytecode semantics.

## Bytecode Compatibility

The deserializer must accept Luau bytecode versions 4 through 12. Version-specific fields, constant kinds, opcodes, and trailers must be selected from the serialized version instead of treating the latest layout as universal. Existing version 4–6 behavior must remain covered while versions 7–12 are added.

## Truth Corpus

Create approximately 20–30 focused `.luau` sources, progressing from simple to complex:

1. Literals, locals, assignments, unary/binary precedence, and globals.
2. Calls, method calls, argument evaluation order, multiple returns, and varargs.
3. Array, record, mixed, computed-key, and incrementally populated tables.
4. `if`/`elseif`/`else`, compound conditions, short-circuit expressions, and conditional expressions.
5. `while`, `repeat`, numeric `for`, generic `for`, `break`, and `continue`.
6. Nested closures, copied and mutable upvalues, recursion, and callback factories.
7. State machines, nested early exits, pcall-style flows, alias chains, and intentionally awkward combinations.
8. Larger integration scripts combining several categories without external dependencies.

Each source is compiled under these primary profiles:

- `O1/g1`: official Luau default and primary source-likeness baseline.
- `O2/g1`: aggressive optimization with line and function-name metadata.
- `O2/g0`: difficult stripped profile.

Secondary diagnostic profiles are `O0/g1`, `O1/g0`, and `g2`. Debug level 2 must not become a crutch: local-name recovery available only there cannot count as structural reconstruction.

Compatibility probes must exercise every bytecode version the bundled compiler can emit and use format-level fixtures for older supported versions that it cannot emit.

## Evaluation Pipeline

For every source/profile pair:

1. Compile source with bundled `.tools/luau-windows/luau-compile.exe`.
2. Decompile binary bytecode with the local `luau-lifter`.
3. Save source, compiler profile, decompiled output, and failure diagnostics together.
4. Parse and compile decompiled output again to catch invalid Luau.
5. Review representative source/output pairs for structural quality.

The harness should support regenerating outputs deterministically and selecting one file or profile for fast diagnosis.

## Quality Signals

Primary signals:

- natural structured control flow instead of avoidable labels, gotos, or boolean temporaries;
- direct expressions instead of register-shuffle assignments;
- natural call and method-call syntax;
- compact table construction where evaluation order permits;
- correct local declaration scope and stable generated names;
- natural multiple assignment and multiple return handling;
- closures attached to correct captures;
- valid Luau output that recompiles.

Secondary signals:

- fewer generated locals and statements;
- lower count of trivial copy assignments;
- lower count of single-use boolean temporaries;
- fewer avoidable table slot writes;
- closer statement and nesting shape to source.

Metrics are diagnostic. Representative output remains acceptance evidence.

## Improvement Loop

1. Generate baseline outputs.
2. Group poor output by recurring structural symptom.
3. Trace each symptom backward through formatter, restructuring, SSA destruction/inlining, lifter, and deserializer boundaries.
4. State one root-cause hypothesis.
5. Add the smallest corpus case that reproduces that general failure.
6. Confirm the case fails before implementation.
7. Implement one general transformation.
8. Re-run the focused case, full corpus, recompile validation, and Rust workspace checks.
9. Keep the change only when it improves the target family without material regressions elsewhere.

## Likely Improvement Areas

Evidence decides exact changes, but likely areas include:

- copy propagation constrained by liveness and side effects;
- expression inlining constrained by evaluation order;
- compound boolean and conditional-expression recovery;
- structured loop and early-exit recovery;
- call/method-call and multi-return reconstruction;
- table-constructor aggregation;
- declaration placement and local lifetime minimization;
- stable semantic fallback naming;
- consumption of function names and line metadata when available;
- graceful handling of supported debug information instead of panicking.

## Error Handling

- One unsupported or malformed case must produce a localized diagnostic instead of crashing the entire corpus run.
- Harness failures identify source, compiler profile, stage, command, and exit status.
- Unsupported bytecode versions or debug records are reported explicitly.
- Decompiler fallback output must remain syntactically valid whenever the available IR permits it.

## Parallel Investigation

Use decorrelated read-only agents after baseline generation:

- bytecode deserializer and lifter correctness;
- CFG/SSA/restructuring quality;
- AST formatting, declaration placement, and naming;
- corpus output review and recurring-symptom classification.

Agents return evidence, root-cause candidates, affected files, and generality risks. Implementation remains coordinated so overlapping passes do not conflict.

## Acceptance

Work is acceptable when:

- primary-profile corpus compilation and decompilation complete without unexplained crashes;
- decompiled outputs recompile, apart from explicitly documented unsupported semantics;
- representative simple, medium, complex, and wonky outputs show material structural improvement;
- the unreadable register-shuffle pattern is reduced through general transformations;
- full Rust workspace verification passes;
- each retained rule has a bytecode/IR invariant-based justification;
- remaining limitations are documented without claiming unrecoverable source details.
