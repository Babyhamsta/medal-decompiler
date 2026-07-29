# Bounded SSA Upvalue Analysis

## Goal

Medal must decompile large, connected Luau chunks without duplicating control-flow
path histories until the allocator aborts. The implementation must remain
input-agnostic and preserve capture and `CLOSEUPVALS` semantics.

For the current diagnostic chunk, success means:

- `connected-root.luac` decompiles as one connected chunk.
- The resulting source is accepted by the pinned Luau parser.
- Runtime is at most five minutes on the current workstation.
- Peak process memory remains below the approved 3 GiB hard ceiling.
- Existing semantic and corpus regressions remain green.

The 3 GiB limit is a ceiling, not a target. The data structure should keep
ordinary and adversarial cases substantially below it.

## Design

Replace per-path vectors of opening locations with compact capture epochs.
Each reference-captured local has at most one active epoch in a control-flow
state. A capture starts an epoch when none is active. Additional captures while
that epoch remains open share it. `CLOSEUPVALS` removes the active epoch.

Propagate these states through the CFG with a deterministic worklist. When
different active epochs for the same local meet at a merge, unify their epoch
identities and retain the canonical representative. Store that representative
at statement ranges for the existing SSA grouping consumer.

This models the identity Medal actually consumes without retaining every path
that reached the identity. Memory is bounded by CFG state and capture sites,
not by the number of control-flow paths.

## Resource Safety

- Use checked size calculations and fallible reservations where growth depends
  on input.
- Do not add chunk-specific prototype IDs, instruction limits, or byte patterns.
- Return a structured phase error if an internal resource bound cannot be
  represented or reserved; do not abort the process.
- Measure wall time and peak working set on both prototype 31 and the complete
  connected chunk.

## Verification

Use a focused red-green cycle:

1. Add a synthetic branching CFG that makes the current path-history vector
   grow exponentially and assert bounded epoch state.
2. Add focused capture, merge, loop, and close tests for epoch identity.
3. Implement the compact worklist analysis.
4. Run the focused CFG/SSA tests and the Rust workspace suite.
5. Decompile isolated prototype 31 and then `connected-root.luac`, measuring
   runtime and peak memory.
6. Parse the produced source with the pinned Luau parser.
7. Run the existing Luau corpus and semantic regression gates, including the
   previously identified multi-return, vararg, repeat, generic-for, protected
   call, and state-machine cases.

