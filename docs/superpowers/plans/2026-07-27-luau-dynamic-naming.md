# Luau Dynamic Naming Plan

**Goal:** Prefer valid compiler debug names, infer only high-confidence
usage roles, and keep collision-safe generated fallbacks when evidence is weak.

**Branch:** `agent/dynamic-naming`, stacked on
`agent/control-flow-cleanup`.

## Task 1: Preserve debug evidence

- [x] Add failing end-to-end `-g2` tests for local and parameter names.
- [x] Seed register/upvalue locals from unambiguous debug metadata.
- [x] Propagate debug names through SSA construction and destruction.
- [x] Reject invalid, reserved, or colliding names before formatting.

## Task 2: Infer conservative roles

- [x] Add failing AST tests for numeric and generic-for roles, returned table
  results, directly invoked callbacks, and weak-evidence fallbacks.
- [x] Infer names from AST usage and initializer evidence only.
- [x] Keep names unique per function and preserve debug names over inferred
  roles.
- [x] Remove generated upvalue markers from fallback identifiers.

## Task 3: Static proof and PR

- [x] Run formatting, focused tests, the full Rust/Python suites, and all 260
  static round trips with zero unsupported jumps.
- [x] Measure debug-name recovery, inferred roles, generated fallbacks,
  locals, aliases, lines, and indentation against the control-flow branch.
- [x] Obtain independent semantic and output-quality review; repair every
  Critical or Important finding.
- [ ] Commit, push, and open a draft PR targeting
  `agent/control-flow-cleanup`. Keep it unmerged until explicit approval.
