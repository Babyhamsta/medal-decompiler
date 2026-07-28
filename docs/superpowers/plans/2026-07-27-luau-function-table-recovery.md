# Luau Function and Table Recovery Plan

**Goal:** Recover source-like function declarations, method syntax, callback
tables, and incremental table construction without changing closure capture,
evaluation order, or table write behavior.

**Branch:** `agent/function-table-recovery`, stacked on
`agent/expression-recovery`.

## Task 1: Advanced truth fixtures

- [ ] Add one product-style controller/module source with constructors,
  methods, callbacks, nested result tables, and incremental fields.
- [ ] Add one adversarial dataflow source with recursive closures, reference
  captures, dynamic callback keys, snapshots, and multi-return boundaries.
- [ ] Compile, decompile, and recompile all 260 source/profile rows. Do not
  execute authored or generated Luau.

## Task 2: Function syntax recovery

- [ ] Write focused failing AST tests for recursive `local function` recovery.
- [ ] Collapse only an adjacent one-local declaration plus same-local closure
  assignment. Preserve forward declaration semantics for every other shape.
- [ ] Write focused failing tests for method recovery and static-function
  rejection.
- [ ] Recover `function object:method(...)` only when the assigned dotted
  closure's first parameter has strong receiver evidence. Rename that exact
  parameter identity to `self`; do not infer methods from names or filenames.

## Task 3: Incremental table folding

- [ ] Write focused failing tests for adjacent literal and callback fields.
- [ ] Fold `local = {}` followed by exact `local[key] = value` writes while
  preserving order and single-result field semantics.
- [ ] Reject folding across calls, aliases, structured statements, nested
  targets, target reads/captures, or non-table initializers.
- [ ] Retain dynamic keys and duplicate writes in their original order.

## Task 4: Static proof and PR

- [ ] Run formatting, focused tests, the full Rust/Python suites, and all 260
  static round trips with zero unsupported jumps.
- [ ] Measure named functions, method declarations, callback/table folding,
  generated locals, aliases, nesting, and nonblank lines against the expression
  branch.
- [ ] Obtain independent semantic and output-quality review; repair every
  Critical or Important finding.
- [ ] Commit, push, and open a draft PR targeting
  `agent/expression-recovery`. Keep it unmerged until explicit approval.
