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
- Resource optimization does not omit, stub, split, truncate, or otherwise
  simplify the decompiled program.

The 3 GiB limit is a ceiling, not a target. The data structure should keep
ordinary and adversarial cases substantially below it.

## Output Fidelity

The optimization changes only Medal's internal representation of live captured
locals. It must not change the bytecode instructions, CFG edges, prototypes, or
closure relationships presented to later decompilation phases.

For chunks that Medal already decompiles, representative output fixtures must
remain textually stable and the full semantic corpus must remain behaviorally
equivalent. For `connected-root.luac`, where no complete baseline output exists,
the result must represent the entire connected chunk without the presentation
cuts or stubs used by the diagnostic package and must pass the pinned Luau
parser.

Exact recovery of original whitespace, comments, and stripped identifier names
is outside the bytecode's information content. Completeness means that Medal
recovers all representable program behavior and structure from the supplied
chunk rather than fabricating or discarding content.

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
- Do not trade output completeness or semantic fidelity for lower resource use.
- Return a structured phase error if an internal resource bound cannot be
  represented or reserved; do not abort the process.
- Measure wall time and peak working set on both prototype 31 and the complete
  connected chunk.

## Verification

Use a focused red-green cycle:

1. Add a synthetic branching CFG that makes the current path-history vector
   grow exponentially and assert bounded epoch state.
2. Add focused capture, merge, loop, and close tests for epoch identity.
3. Record representative pre-change decompiler outputs and require textual
   stability after the internal optimization.
4. Implement the compact worklist analysis.
5. Run the focused CFG/SSA tests and the Rust workspace suite.
6. Decompile isolated prototype 31 and then `connected-root.luac`, measuring
   runtime and peak memory.
7. Parse the produced source with the pinned Luau parser.
8. Confirm the complete output retains every prototype and closure relationship
   represented by the input chunk.
9. Run the existing Luau corpus and semantic regression gates, including the
   previously identified multi-return, vararg, repeat, generic-for, protected
   call, and state-machine cases.
