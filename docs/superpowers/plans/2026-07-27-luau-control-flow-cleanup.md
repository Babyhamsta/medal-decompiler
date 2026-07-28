# Luau Control-Flow Cleanup Plan

**Goal:** Reduce decompiler-shaped branching and nesting while preserving
condition effects, branch scope, and loop/function exits.

**Branch:** `agent/control-flow-cleanup`, stacked on
`agent/function-table-recovery`.

## Task 1: Focused control-flow recovery

- [ ] Add failing AST tests for empty-then inversion, terminal-branch guard
  clauses, terminal-else inversion, and pure empty-if removal.
- [ ] Recursively clean functions and structured blocks using AST exit and
  side-effect evidence only.
- [ ] Strip an existing `not` when inverting; otherwise wrap the exact
  condition in `not` without relational rewrites.

## Task 2: Semantic boundaries

- [ ] Preserve empty branches whose conditions can call, index, invoke
  metamethods, or otherwise have observable effects.
- [ ] Do not flatten a branch unless the opposite branch provably terminates
  with return, break, continue, or a fully terminal conditional.
- [ ] Keep existing dynamic `elseif` formatting and avoid fixture-specific
  patterns.

## Task 3: Static proof and PR

- [ ] Run formatting, focused tests, the full Rust/Python suites, and all 260
  static round trips with zero unsupported jumps.
- [ ] Measure nesting, branch count, empty branches, generated locals,
  aliases, indentation, and nonblank lines against the function/table branch.
- [ ] Obtain independent semantic and output-quality review; repair every
  Critical or Important finding.
- [ ] Commit, push, and open a draft PR targeting
  `agent/function-table-recovery`. Keep it unmerged until explicit approval.
