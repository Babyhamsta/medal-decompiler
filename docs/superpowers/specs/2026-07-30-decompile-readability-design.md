# Luau Decompile Readability Design

## Status and Precedence

This design covers the readability and structure of decompiled output.

Where it touches `2026-07-27-luau-correctness-spine-design.md`, the
correctness spine controls. No change here may make a currently-succeeding
input fail, and no change here may alter observable program behaviour.

This design deliberately supersedes one gate in
`2026-07-30-luau-decompiler-performance-design.md`. That document requires
decompiled output to remain byte-identical. The entire purpose of this work
is to change that output, so the byte-identity gate is retired and replaced
by the behavioural gate in *Verification* below: output must recompile, and
must produce identical runtime results to the original source across every
corpus probe. Throughput and memory limits from that document remain in
force, restated as a budget in *Performance Budget*.

## Objective

Make decompiled Luau readable at scale. The working target is a real
capture: a 7,838,266-byte version 12 chunk that decompiles to 248,778 lines.

Three axes, in order of measured leverage:

1. Naming. Roughly 1,500 of 1,700 declared locals are `v1..vN`, appearing as
   337,499 tokens.
2. Statement folding. 113,209 of 248,778 lines (45%) are register-slot
   assignments of the form `vN[k] = ...`.
3. Vertical spacing. The output contains exactly zero blank lines.

Line wrapping is explicitly out of scope; see *Non-Goals*.

## Boundaries

- Decompiled output must recompile under the pinned Luau compiler for every
  input that currently succeeds. Hard gate.
- Decompiled output must produce identical runtime results to the original
  source for every corpus probe. Hard gate.
- Keep the existing lifter, CFG, SSA, restructuring, AST, and formatter
  architecture. No replacement IR, no new pass framework.
- New behaviour ships default-on. No feature flags, no `--raw` escape hatch.
- Every new pass is justified by measurement. A pass with no measured effect
  on readability metrics is reverted, not kept.

## Non-Goals

**Line wrapping.** The 248,778-line output contains 45 lines longer than 120
characters. All 45 contain a large string literal — base64 payloads and
embedded Lua source. A Lua string literal cannot be split across lines
without inserting `..` concatenation, which changes the AST. There are zero
long *code* lines to wrap.

Statement folding will produce longer lines than exist today, so one guard
ships with it: an argument list or table constructor whose rendered width
exceeds 120 columns wraps one element per line. Nothing else wraps.

**Section banners and header comments.** Rejected. A banner is the
decompiler asserting structure it inferred rather than observed.

**Recovering original identifiers.** Not possible from stripped bytecode.
Naming work infers *roles*, not original names.

## Measured Baseline

Release build at `3483a8c`, Windows 11, stage-27 fixture
(`recovered.reconstructed.v9-g2.luac`, 7,838,266 bytes, version 12).

Wall clock and peak resident set, three consecutive runs:

| Run | Seconds | Peak RSS |
| ---: | ---: | ---: |
| 1 | 13.56 | 1,817.9 MB |
| 2 | 13.45 | 1,818.7 MB |
| 3 | 13.42 | 1,819.3 MB |

Output: 248,778 lines, 5,525,808 bytes.

Exclusive time and allocation by phase, from a `--features profiling` build
across 486 functions. A profiling build carries accounting overhead and runs
slower than the release figures above; the ratios are what matter.

| Phase | Seconds | Allocated | Allocations |
| --- | ---: | ---: | ---: |
| structure | 7.456 | 1,022.2 MB | 11,474,201 |
| ssa | 2.634 | 9,080.7 MB | 17,749,712 |
| restructure | 1.492 | 2,445.1 MB | 15,350,722 |
| ssa-destruction | 1.447 | 1,431.8 MB | 1,925,832 |
| lift | 1.341 | 4,579.8 MB | 8,808,751 |
| format | 0.205 | 12.7 MB | 27,459 |
| declaration | 0.095 | 18.2 MB | 111,189 |
| unknown | 0.072 | 0.3 MB | 1,524 |
| link | 0.068 | 0.6 MB | 4,714 |
| ast-recovery | 0.060 | 1.3 MB | 13,308 |
| deserialize | 0.014 | 23.6 MB | 6,140 |

Peak live heap 1,622.5 MB, reached during `ssa` at function 39.

Live heap at checkpoint `functions-decompiled`: 403.9 MB.

### Why this baseline makes the work affordable

Every pass in this design lands in exactly two phases: `ast-recovery`
(0.060 s, 1.3 MB) and `format` (0.205 s, 12.7 MB). Together they are 0.265 s
and 14.0 MB, roughly 2% of runtime and under 0.1% of allocation.

Peak resident set is owned by the `ssa` phase, which this design does not
touch. Whole-program passes run after `functions-decompiled`, where live
heap is 403.9 MB — about 1.2 GB below the run's peak. A whole-program pass
would have to allocate more than a gigabyte before it could move peak RSS.

### Readability baseline

Metrics this design moves, measured on the same fixture:

| Metric | Baseline |
| --- | ---: |
| Lines | 248,778 |
| Blank lines | 0 |
| Declared locals named `vN` | ~1,500 of ~1,700 |
| `vN` tokens | 337,499 |
| Lines matching `vN[k] = ...` | 113,209 |
| Lines over 120 characters | 45 (all string literals) |

## Architecture

### Current pipeline

Per function, in `decompile_function` (`luau-lifter/src/lib.rs`), phase
`AstRecovery`:

```
eliminate_aliases_with_protected
recover_expressions_with_protected
cleanup_control_flow
  -> declare_locals -> validate_bindings
```

Whole program, phase `Format`:

```
recover_function_syntax
name_locals(body, false)
body.to_string()
```

### Target pipeline

Per function, phase `AstRecovery`:

```
loop, at most 4 iterations, until no pass reports a change:
    eliminate_aliases_with_protected
    fold_table_slots              NEW  ast/src/slot_folding.rs
    recover_expressions_with_protected
    fold_table_constructors       NEW  ast/src/table_construction.rs
cleanup_control_flow
  -> declare_locals -> validate_bindings
```

Whole program, phase `Format`:

```
recover_function_syntax
propagate_parameter_names         NEW  ast/src/name_flow.rs
name_locals(body, false)          EXTENDED
body.to_string()                  EXTENDED
```

The fixpoint loop exists because slot folding creates new single-use locals
that `recover_expressions` can inline, and inlining creates new adjacent slot
writes that folding can merge. Every pass in the loop already returns a
`usize` change count, so the loop costs one comparison per iteration.

Four iterations is a cap, not a target. The loop exits as soon as an
iteration reports zero changes. If any input reaches the cap, that is
recorded and reported, matching how `bounded prototype expansion` handles its
own limits — a bound that is hit is a fact to surface, not a silent
truncation.

`declare_locals` and `validate_bindings` stay downstream of everything. An
unsound fold or a rename that breaks lexical binding fails the decompile with
a `Declaration` phase error rather than emitting incorrect Lua.

## Component: Slot Folding

New module `ast/src/slot_folding.rs`. This is the only component that can
produce semantically wrong output, so its rules are stated as preconditions
rather than heuristics.

### What it does

Collapse a table used as a register array back into expressions:

```lua
-- before, 4 statements
v2[3] = _ENV.table
v2[4] = "unpack"
v2[3] = v2[3][v2[4]]
push_stack(v3, v2[3])

-- after, 1 statement
push_stack(stack, _ENV.table.unpack)
```

### Preconditions

Fold a write `T[K] = E` into a later read of `T[K]` only when all of the
following hold. Any failure abandons the fold for that slot; there is no
partial or best-effort path.

1. `T` is a local. `K` is a constant literal, number or string.
2. `T`'s binding in this function is a table literal (`{}` or `{...}`). Not
   a parameter, upvalue, call result, or index expression. Unknown
   provenance means an unknown metatable.
3. The write and the read are straight-line in the same block. No branch,
   loop, or label boundary between them.
4. No intervening call and no side-effecting statement. An assignment whose
   target is an index expression counts as side-effecting, so a write
   through an alias of `T` blocks the fold without needing to prove the
   alias exists.
5. No write to `T` anywhere in the function uses a computed key.
6. `setmetatable` is never applied to `T` in this function.
7. The write is removed only if that slot has no other read before it is
   overwritten.

### Why precondition 4 removes the need for escape analysis

`T` may have leaked to a closure before the fold window. Only a call can run
that closure. Precondition 4 forbids any call between the write and the read,
so no leaked reference can observe the slot in that window. This buys
soundness without a whole-function escape analysis, and without the cost such
an analysis would add to a phase that currently runs in 60 ms.

### The precondition least likely to survive contact

Precondition 6 is the weak one. Verifying that `setmetatable` is never
applied to `T` requires tracking every alias of `T`, and aliasing through a
call argument is not tractable under the cheap analysis this phase can
afford. The corpus case `29_slot_metatable` exists to determine empirically
whether the conservative check is strict enough. If it is not, precondition 2
tightens to require that `T` never appears as a bare argument to any call —
losing some folds, keeping soundness.

### Constructor folding

Separate module `ast/src/table_construction.rs`, same phase, running after
slot folding.

```lua
-- before
local v1 = {}
v1[1] = 0
v1[2] = 12
v1[3] = 16

-- after
local entry = { 0, 12, 16 }
```

Reuses preconditions 1, 2, 4, and 6, plus:

- The run is contiguous from the table literal.
- Keys are dense from 1 (emitted positionally) or all constant strings
  (emitted as named fields).
- No element expression reads `T`.

Field order follows write order, not hash order, so the output is stable
across runs.

## Component: Naming

Three additions. All are renames. A rename cannot change semantics; the only
hazard it introduces is shadowing, which `validate_bindings` already detects.

### Callee-parameter propagation

New module `ast/src/name_flow.rs`, running after linking, when every function
body is present in a single tree.

Build a map from each local that holds a closure to that closure's parameter
names. For each call `f(a, b)` where an argument is an unnamed local, propose
the corresponding parameter's name.

```lua
local function push_stack(stack, value) ... end
...
push_stack(v3, x)     -- v3 takes the name "stack"
```

Rules:

- Only for locals with no name from stronger evidence.
- Every call site must agree. Conflicting proposals mean no name.
- Skip if the proposed name is already bound in scope.
- The map is keyed by function, so its size is O(functions) — 486 entries on
  the fixture, not O(statements).

### Known-library returns

A static table consulted when a local has no other evidence:

| Call | Name |
| --- | --- |
| `table.pack` | `packed` |
| `table.create` | `buffer` |
| `setmetatable` | `object` |
| `pcall` / `xpcall` | `ok` |
| `select("#", ...)` | `count` |
| `string.format` | `text` |
| `coroutine.create` | `thread` |

The table is data, extended by adding rows. It is not a place for
speculative entries; a wrong name is worse than `value` because it asserts
something false about the code.

### Typed fallback replacing `vN`

`Namer::fallback_name` in `ast/src/name_locals.rs` currently emits `v{n}`.
It is replaced by a deterministic taxonomy driven by the initializer shape:

| Initializer shape | Name |
| --- | --- |
| Table literal, written `t[i]` inside a loop | `registers` |
| Table literal, indexed by constant integers | `slots` |
| Table literal, string keys only | `record` |
| Closure | `handler` |
| Arithmetic on a length or count | `count` |
| String literal or concatenation | `text` |
| Call result, no other evidence | `result` |
| Nothing inferable | `value` |

A numeric suffix is appended only on collision: `count`, `count2`. `vN`
disappears from output.

### Shadow-free numbering

`Namer` tracks in-scope names as a stack across nested function boundaries,
so an inner binding never shadows a visible outer one. This fixes the case
where per-function counters restart and an inner `v6` shadows an enclosing
`v6`.

## Component: Formatter Spacing

`format_block_no_indent` in `ast/src/formatter.rs` gains a
`needs_blank_between(previous, current)` predicate.

Whether a statement renders across multiple lines is decided
**structurally**, from the statement kind and its value shape. It is not
decided by rendering the statement to a scratch buffer and looking for a
newline — that would double formatting cost on 248,778 lines to answer a
question the AST already knows.

Treated as multi-line:

- `If`, `While`, `Repeat`, `NumericFor`, `GenericFor`
- any assignment whose value contains a `Closure`
- any assignment whose value is a `Table` that the formatter would already
  render across lines

The last rule must not restate the formatter's table decision, or the two
will drift. `format_table` computes that decision today as a local
`should_format`:

```rust
let should_format = !table.0.is_empty()
    && (!sequential_keys || table.0.len() > 3)
    || Self::contains_table(table);
```

That expression is extracted into a `fn table_renders_multiline(&Table) ->
bool`, called from both `format_table` and the blank-line predicate, so one
definition serves both.

Blank line rules:

- Before and after each multi-line statement.
- Before a `Return` that follows a non-return statement.
- After a run of two or more consecutive `local` declarations, when the next
  statement is not itself a declaration.
- Never between consecutive single-line statements of the same kind, so runs
  like `t[1] = a; t[2] = b` stay tight.
- Never as the first or last line of a block.

## Performance Budget

The user constraint is explicit: this work must not undo the optimization
that took the fixture from ~33 s to ~13 s.

| Metric | Baseline | Ceiling |
| --- | ---: | ---: |
| Wall clock | 13.42–13.56 s | 16.0 s (+18%) |
| Peak RSS | 1,817.9–1,819.3 MB | 2,000 MB (+10%) |
| `ast-recovery` phase | 0.060 s | 1.0 s |
| `format` phase | 0.205 s | 1.5 s |

The phase ceilings allow the touched phases to grow roughly sixteenfold and
sevenfold respectively and still cost 2.5 s combined. They are deliberately
loose: the point is to catch a pass that is accidentally quadratic or that
leaks memory, not to police tens of milliseconds.

A ceiling is a failure threshold, not a budget to spend. The measured
per-phase table is recorded on every commit regardless, so a pass that costs
0.4 s where 0.05 s was expected is visible and gets questioned even though it
is nowhere near the ceiling.

Complexity rules that keep those ceilings reachable:

- Slot folding scans forward from each write with a bounded window, capped at
  64 statements, and aborts at the first side-effecting statement. Linear in
  block length, never quadratic.
- The fixpoint loop is capped at 4 iterations and exits early on zero
  changes. The existing passes it wraps cost 0.060 s combined today.
- `name_flow` builds one map of size O(functions) in a single traversal. No
  per-call-site tree walk.
- The formatter predicate is O(1) per statement and allocates nothing.
- Constructor folding rewrites in place. No cloning of table values.

Every commit re-measures with a `--features profiling` build and records the
per-phase table. A commit that moves a phase past its ceiling does not land.

## Verification

The existing harness runs 78 compile/decompile/recompile combinations but
executes only 6 of the 26 corpus cases. Six probes that contain no table used
as a register array cannot catch a bad slot fold. The harness is extended
before any folding pass is written.

### Gate

| Check | Bar |
| --- | --- |
| Corpus, 3 primary profiles | 78 runs, 0 compile/decompile/recompile failures |
| Semantic probes | 26 of 26 cases executed, output identical to source |
| Compatibility profiles | Bytecode V9–V12 still decompile and recompile |
| Real-file smoke | 248,778-line decompile parses under `luau-analyze` |
| Performance | Within the ceilings in *Performance Budget* |

### Promoting 6 probes to 26

Every corpus case becomes self-checking: it computes deterministic values and
prints them through the existing `SEMANTIC_RESULT` protocol in
`tools/luau_corpus/semantic.py`. A case that merely parses proves nothing
about a pass that rewrites its statements.

This is the bulk of the harness work and lands first, on its own commit, with
no decompiler change alongside it. That ordering means the new probes run
against unmodified `main` and establish their expected output there.

### New corpus cases

All six are executable probes, written so a wrong fold changes printed output
rather than only shape:

| Case | Exercises |
| --- | --- |
| `27_register_array_vm` | Slot writes and reads with no escape — the fold should fire |
| `28_escaping_slot_table` | Table passed to a call mid-sequence — precondition 4 |
| `29_slot_metatable` | `__index`/`__newindex` on the slot table — precondition 6 |
| `30_aliased_slot_write` | Two locals bound to the same table — preconditions 2 and 4 |
| `31_nonconstant_slot_key` | `t[i]` where `i` is computed — preconditions 1 and 4 |

### Precondition 5 is subsumed and cannot be isolated

Precondition 5 (no write to `T` anywhere in the function uses a computed key)
has no case of its own because no such case exists.

Precondition 4 already treats *any* assignment whose target is an index
expression as side-effecting, with no carve-out for computed versus constant
keys. That covers every computed-key write inside a fold window. Outside the
window, a computed-key write before the target write is overwritten by it, so
folding stays safe; one after the read cannot retroactively change a value
already read, and any later read of the same slot is blocked independently by
preconditions 3 and 4.

Two independent attempts to construct a program where precondition 5 is the
only thing preventing a wrong fold failed, one of them checked against the
concrete algorithm rather than this document. Precondition 5 is therefore
redundant given preconditions 3 and 4.

It stays in the implementation as a cheap explicit guard — a redundant check
that costs nothing is worth keeping when the cost of being wrong is silently
incorrect output. But it must not be counted as independently verified
coverage, and no case should claim to test it.
| `32_slot_across_control_flow` | Write in one branch, read after the join — precondition 3 |

### Real-file regression fixture

The 7.8 MB stage-27 capture lives outside the repository. Committing 7.8 MB
of third-party bytecode is the wrong trade.

Instead: a checked-in `.luau` source models the register-array VM pattern and
serves as the committed fixture. The full capture is checked additionally
when `MEDAL_BIG_FIXTURE` names an existing path, and skipped when it does
not, so the check runs on this machine and does not fail elsewhere.

### Readability metrics

The harness records, per run, on whatever fixture is available: line count,
blank-line count, count of locals still named `vN`, count of lines matching
`vN[k] = ...`, and count of lines over 120 characters. These are reported,
not gated — they are how "did this actually improve readability" gets
answered with a number instead of an impression.

## Risks

**Slot folding is unsound in a case the corpus does not model.** Highest
risk in the design. Mitigated by stating preconditions rather than
heuristics, by six targeted probes, and by keeping folding in its own module
so it can be reverted without touching naming or formatting.

**Precondition 6 cannot be checked cheaply enough.** Covered above; the
fallback is a stricter precondition 2 that costs folds and keeps correctness.

**Typed fallback names assert something false.** A local named `registers`
that is not a register array misleads worse than `v7` does. Mitigated by
driving the taxonomy from initializer shape only, and by falling back to
`value` rather than guessing.

**The fixpoint loop does not converge on some input.** Capped at 4
iterations. A capped run is recorded and surfaced.

**Blank-line rules make small functions worse.** A three-statement function
gaining two blank lines reads worse, not better. The "never between
consecutive single-line statements of the same kind" rule exists for this;
the readability metrics will show whether it is sufficient.

## Sequencing

1. Harness: promote 6 probes to 26, add readability metrics, add the
   performance measurement step. No decompiler change.
2. Formatter spacing. Lowest risk, immediately visible, independent of
   everything else.
3. Naming: shadow-free numbering, then typed fallback, then library returns,
   then callee-parameter propagation.
4. Six new corpus cases, written and passing against unmodified folding
   behaviour.
5. Slot folding.
6. Constructor folding.
7. The 120-column wrap guard, last, once folding has produced the long lines
   it exists to handle.

Each step is a separate commit with its own corpus run and per-phase
measurement.
