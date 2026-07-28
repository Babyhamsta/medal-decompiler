# Luau Function and Table Recovery Plan

**Goal:** Recover source-like function declarations, method syntax, callback
tables, and incremental table construction without changing closure capture,
evaluation order, or table write behavior.

**Branch:** `agent/function-table-recovery`, stacked on
`agent/expression-recovery`.

## Task 1: Advanced truth fixtures

- [x] Add one product-style controller/module source with constructors,
  methods, callbacks, nested result tables, and incremental fields.
- [x] Add one adversarial dataflow source with recursive closures, reference
  captures, dynamic callback keys, snapshots, and multi-return boundaries.
- [x] Compile, decompile, and recompile all 260 source/profile rows. Do not
  execute authored or generated Luau.

## Task 2: Function syntax recovery

- [x] Write focused failing AST tests for recursive `local function` recovery.
- [x] Collapse only an adjacent one-local declaration plus same-local closure
  assignment. Preserve forward declaration semantics for every other shape.
- [x] Write focused failing tests for method recovery and static-function
  rejection.
- [x] Recover `function object:method(...)` only when the assigned dotted
  closure's first parameter has strong receiver evidence. Rename that exact
  parameter identity to `self`; do not infer methods from names or filenames.

## Task 3: Incremental table folding

- [x] Write focused failing tests for adjacent literal and callback fields.
- [x] Fold `local = {}` followed by exact `local[key] = value` writes while
  preserving order and single-result field semantics.
- [x] Reject folding across structured statements, nested targets, direct
  target reads/captures, open multi-return tails, non-table initializers, and
  effectful values when the target's SSA capture group is observable.
- [x] Retain dynamic keys and duplicate writes in their original order.

## Task 4: Static proof and PR

- [x] Run formatting, focused tests, the full Rust/Python suites, and all 260
  static round trips with zero unsupported jumps.
- [x] Measure named functions, method declarations, callback/table folding,
  generated locals, aliases, nesting, and nonblank lines against the expression
  branch.
- [x] Obtain independent semantic and output-quality review; repair every
  Critical or Important finding.
- [ ] Commit, push, and open a draft PR targeting
  `agent/expression-recovery`. Keep it unmerged until explicit approval.

Local commits are complete. Push and PR creation are blocked by the current
environment's GitHub network/credential restrictions.
