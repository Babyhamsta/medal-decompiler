# Luau Source Reconstruction Design

## Context

The V9-V12/current-profile compiler-round-trip corpus and V4-V8 parser/format
fixtures are complete. The current decompiler produces valid, structured Luau
across the 240-case compiler matrix, but representative output still contains
temporary aliases, generic names, avoidable nesting, and expressions that
visibly reflect bytecode registers.

This design covers the next quality phase. Its purpose is not merely to shorten
output. It should reconstruct the most natural source form justified by bytecode,
SSA, control-flow, and debug evidence.

## Primary Objective

Make output resemble a real Luau script as closely as recoverable information
allows.

Output quality takes priority over a globally conservative or aggressive policy.
Each transformation chooses the strongest rewrite supported by its local
evidence. A rewrite may be aggressive when SSA identity, dominance, liveness,
and evaluation order prove it equivalent. It must stop when calls, metamethods,
mutation, closure capture, or multiple-return behavior make the result
ambiguous.

## Safety Boundary

Arbitrary source or decompiled output will not be executed.

Verification uses:

- bytecode compilation;
- decompilation;
- parsing and recompilation of generated Luau;
- bytecode and IR inspection;
- targeted AST and CFG assertions;
- static output-quality metrics.

Known corpus scripts may be compiled because compilation does not run their
program logic. Runtime equivalence is not an acceptance gate for this phase.

## Generalization Boundary

All rules must depend on general program evidence:

- SSA definitions and uses;
- value classes and copy identity;
- dominance and post-dominance;
- local and upvalue writes;
- expression side effects and Luau evaluation order;
- call and return arity;
- closure capture mode;
- table-construction provenance;
- CFG topology;
- serialized debug names and function metadata.

No pass may inspect corpus filenames, exact constants, URLs, generated register
numbers, or formatted source fragments to decide whether a rewrite applies.
Formatted-text substitution is not an implementation mechanism.

## Pass Architecture

Readability work will be implemented as small, ordered IR or AST passes. Each
pass reports whether it changed the function and must be independently testable
and idempotent.

The preferred order is:

1. alias and copy elimination;
2. expression recovery;
3. function and table recovery;
4. control-flow cleanup;
5. declaration placement;
6. naming;
7. formatting.

Early passes expose stronger evidence to later passes. Naming stays late so it
does not affect semantic matching.

## Section 1: Alias and Copy Elimination

This is the highest-priority section.

The pass will eliminate:

- single-use register moves;
- chains such as `a = b; c = a`;
- pass-through locals used only as call arguments or return values;
- redundant copies of closure captures;
- aliases created only to satisfy bytecode register placement;
- dead assignments left after propagation.

An alias can be removed when replacing its use preserves:

- the value observed at the original snapshot point;
- expression evaluation order;
- the number and order of calls or metamethod-capable operations;
- local and upvalue mutation visibility;
- closure capture-by-value versus capture-by-reference behavior;
- multiple-return selection.

Pure local-to-local SSA copies may propagate across blocks when dominance and
value-class evidence prove identity. Reads of mutable upvalues may move only
across expressions proven not to call, index through a metamethod, or write the
captured value. When proof is unavailable, the alias remains.

The target example:

```luau
local alias = moduleTable
return setmetatable({...}, alias)
```

becomes:

```luau
return setmetatable({...}, moduleTable)
```

only when construction of the preceding arguments cannot change the value read
from `moduleTable`.

## Section 2: Expression Recovery

After copy noise is reduced, the expression pass will recover:

- direct return and call expressions;
- local compound assignments;
- indexed compound assignments when the object and key are evaluated exactly
  once in the bytecode;
- short-circuit `and` and `or` expressions;
- Luau conditional expressions;
- negated comparisons instead of temporary booleans;
- unnecessary single-result selection and parenthesized temporaries.

Expression folding must preserve left-to-right evaluation, metamethod behavior,
and call multiplicity. Algebraic simplification is allowed only when Luau
semantics make it valid for the inferred operand types; it will not assume all
values are plain numbers.

## Section 3: Function and Table Recovery

This section will reconstruct source-level declarations and constructors:

- `local function name(...)` when declaration and closure lifetime agree;
- table member functions and methods;
- immediately assigned callback closures;
- recursive declarations without temporary initialization spam;
- table literals assembled by safe sequential field writes;
- array and record fields folded into one constructor;
- duplicate template fields reduced to the final visible value;
- direct method syntax when the receiver and implicit `self` flow prove it.

Field folding stops at any operation that may observe the partially constructed
table. Closure captures of the table are observation points unless capture
analysis proves the closure cannot run before construction completes.

## Section 4: Control-Flow Cleanup

The control-flow pass will improve already structured output:

- collapse nested `else` plus `if` into `elseif`;
- remove empty branches;
- invert conditions when that removes a level of nesting;
- prefer a guard clause when the CFG has an early exit and one continuing path;
- merge adjacent conditions representing one short-circuit expression;
- remove redundant loop-state assignments;
- preserve `break` and `continue` when they are the natural structured form.

Selection is shape-driven. The pass will compare equivalent representations
using nesting depth, temporary count, duplicated conditions, and source-order
continuity. It will not force guard clauses when inversion makes the condition
less readable.

## Section 5: Declaration Placement and Naming

Declaration placement will minimize artificial lifetime without changing
capture behavior:

- declare locals near their first meaningful definition;
- avoid `local value = nil` before an immediately reconstructed declaration;
- retain predeclarations required for recursion or by-reference capture;
- group parallel declarations only when bytecode preserves parallel assignment.

Naming uses an evidence hierarchy:

1. serialized debug-local names;
2. serialized function names and class/member names;
3. assignment from a named global or member;
4. stable usage roles such as loop index, callback, result, key, value, or
   options;
5. generated fallback names.

Heuristic names require a single dominant role. Conflicting evidence falls back
to a neutral generated name instead of inventing a misleading semantic name.
Names are unique, valid Luau identifiers, and stable across repeated runs.

## Testing Strategy

Every section follows test-first development:

1. Add the smallest truth source or direct IR fixture exhibiting one unwanted
   shape.
2. Compile and decompile it to confirm the focused assertion fails for the
   intended reason.
3. Implement one general rewrite.
4. Re-run the focused test.
5. Re-run the Rust workspace and Python harness tests.
6. Re-run the complete V9-V12/current-profile 240-case
   compile/decompile/recompile matrix.
7. Compare representative simple, medium, complex, and wonky outputs.

Focused tests assert both desired forms and forbidden artifacts. Examples
include absence of a redundant alias, presence of `elseif`, stable closure
capture, or a folded table constructor. Full-output golden files are avoided
where they would make harmless formatting changes expensive.

## Quality Measurement

The report will track per case and profile:

- trivial local-to-local assignments;
- single-use generated locals;
- total generated locals;
- total statements;
- maximum conditional nesting;
- empty branches;
- generated `goto` or label nodes;
- incremental table writes that could be constructor fields;
- parse and recompile status.

Metrics guide diagnosis rather than act as an absolute score. A transformation
is retained when representative output is more source-like and static semantic
evidence remains sound, even if one raw count stays flat.

## Section Gates

Each section is completed before beginning the next:

- focused regression tests pass;
- all workspace tests pass;
- the full corpus recompiles;
- no unsupported jumps appear;
- representative outputs show the intended improvement;
- no unrelated output family materially regresses.

If a section exposes a lower-layer correctness defect, that defect receives its
own failing test and is repaired before readability work continues.

## Acceptance

The phase is complete when:

- redundant alias and copy spam is materially reduced;
- complex output reads as structured Luau rather than register operations;
- expression, function, table, and control-flow forms are closer to their truth
  sources;
- useful debug names are consumed when present;
- stripped bytecode still receives stable, non-misleading generated names;
- all retained transformations are based on bytecode, IR, or CFG invariants;
- all static verification gates pass without executing arbitrary scripts;
- remaining differences are attributable to information compilation genuinely
  erased or to explicitly documented ambiguity.
