# Luau Decompiler Performance Design

## Status and Precedence

This design covers throughput and memory only. Where it touches work
described in `2026-07-27-luau-correctness-spine-design.md`, the correctness
spine controls: no change here may alter decompiled output for any input that
currently succeeds.

## Objective

Cut wall-clock time and peak memory on large deobfuscated Luau chunks without
changing decompiled output, and repair one reconstruction failure that
currently produces no output at all.

The working target is a real capture: a 7,838,266-byte version 12 chunk that
decompiles to 5,525,808 bytes across 248,778 lines.

## Boundaries

- Decompiled output for any currently-succeeding input must remain
  byte-identical. This is a hard gate, not a goal.
- Keep the existing lifter, CFG, SSA, restructuring, AST, and formatter
  architecture. No replacement IR.
- Public API of `cfg` may gain accessors but must not lose capability.
- Optimizations are justified by measurement, not by inspection. A change with
  no measured effect is reverted, not kept "because it should help".

## Measured Baseline

Release build at `04d7f9d`, single run, Windows 11, stage-27 fixture.

| Metric | Value |
| --- | ---: |
| Wall clock | 33.9 s |
| Peak resident set | 3,307 MB |
| Peak live heap | 3,134 MB |
| Live heap after all functions decompile | 404 MB |
| Total allocated | 38,032 MB |
| Total allocations | 235,178,274 |
| Output | 5,525,808 bytes / 248,778 lines |
| Output SHA-256 | `4eda076821e7edfdccb6517e464aee9b2d97ece7365a010fd8979ca41a241544` |

Exclusive time and allocation by phase, across 486 functions:

| Phase | Seconds | Allocated | Allocations |
| --- | ---: | ---: | ---: |
| structure | 22.94 | 20,493 MB | 137,665,802 |
| ssa | 4.42 | 9,256 MB | 53,405,345 |
| restructure | 2.52 | 2,445 MB | 15,350,722 |
| lift | 1.71 | 4,649 MB | 8,888,172 |
| ssa-destruction | 1.63 | 954 MB | 5,614,603 |
| unknown | 0.50 | 12 MB | 986,100 |
| format | 0.37 | 64 MB | 4,179,214 |
| link | 0.23 | 42 MB | 3,342,471 |
| ast-recovery | 0.23 | 38 MB | 3,016,557 |
| declaration | 0.21 | 49 MB | 2,722,662 |
| deserialize | 0.01 | 25 MB | 6,564 |

The structure phase decomposes into, across 486 scheduler runs / 1,812 rounds:

| Pass | Seconds | Calls |
| --- | ---: | ---: |
| facts-derive | 9.43 | 1,812 |
| inline | 6.70 | 1,812 |
| fingerprint | 3.27 | 5,436 |
| remove-unnecessary-params | 0.31 | 1,812 |
| structure-jumps | 0.06 | 1,812 |
| structure-conditionals | 0.02 | 1,812 |
| dominators | 0.02 | 1,812 |

Two observations shape everything below.

**Peak memory is transient, not retained.** Live heap after all 486 functions
finish is 404 MB against a 3,134 MB peak. Roughly 2.7 GB is garbage churned
inside a single function's structuring and then freed. Reducing peak means
reducing per-function transient working set, not retaining less.

**Work is concentrated.** Function 479 costs 10.76 s and 9,950 MB of churn;
function 39 costs 8.65 s and 5,186 MB. Two of 486 functions account for 56% of
runtime. Cross-function parallelism is therefore capped near 2x regardless of
core count, and is scheduled last.

## Architecture

### Measurement Spine

Already landed at `e757179`.

`luau-lifter`'s `profiling` feature installs a tracking global allocator and
opens a phase scope inside `catch_phase`. Because every phase boundary already
exists for error attribution, profiling coverage follows those boundaries
rather than a second set of markers. Time and allocation are attributed
exclusively: a nested scope suspends its parent, so `structure` does not
absorb the `ssa` work it encloses.

`cfg::metrics` holds coarse scheduler counters, incremented per pass
invocation rather than per graph node, so overhead is immaterial.

The report includes live-byte checkpoints across the pipeline. That is what
distinguishes transient working set from retained memory, and it is the
measurement that redirected this design away from retention-focused fixes.

### Phase 1: Lazy Recovery Facts

`RecoveryFacts::derive` eagerly builds seven fields. Production reads one:

- `restructure/src/lib.rs:680` reads `candidate_regions`.
- `luau-lifter/src/lib.rs:1041` reads `function_id()` in a `debug_assert`.

`locals`, `statement_origins`, `edges`, `dominators`, `post_dominators`, and
`effects` are built 1,812 times and never read outside `cfg`'s own tests.
`derive_candidate_regions` computes from the function graph via
`kosaraju_scc` and does not consult the other fields.

`dominators` and `post_dominators` are `BTreeMap<NodeIndex, BTreeSet<NodeIndex>>`
— materialized dominator sets per node, quadratic in block count. These are
the leading suspect for the 3,134 MB peak.

Change: convert the six unread fields from eager `pub` fields to accessors
that compute on first call and memoize. The evidence surface stays available
to tests and future passes; production stops paying for it.

Accessors take `&self` and memoize, so `RecoveryFacts` gains interior
mutability for the cached slots. `RecoveryFacts` derives `Clone`, `Debug`, and
`PartialEq`; those impls must compare and clone the *logical* facts, not
cache-population state, or two equal fact sets could compare unequal purely
because one had been queried.

### Phase 2: Single Derivation Per Scheduler Run

`PassScheduler::run` derives facts once before the loop and again at the end of
every round that changed something. The early-return path returns the facts
derived at the *end of the previous round*, so every intermediate derivation
except the last is discarded — 1,812 derivations for 486 used results.

At the point of the early return, all passes in the round reported no change,
so the function is in the same state it was in when the previous round's
derivation ran. Deriving once at that point is therefore equivalent.

One behavioural caveat: `derive` returns `Result` and can fail with
`InvalidEntry`. Today a mid-loop failure aborts the run. Deriving once at exit
would defer that detection. `InvalidEntry` is a cheap structural check, so the
check stays per-round while the expensive construction moves to exit.

### Phase 3: Streaming Structural Fingerprint

`structural_fingerprint` writes the entire function — every block's full AST
via `Debug`, every edge's arguments via `format!` — into one `String`, then
hashes it. It is called three times per round: once by the scheduler and twice
by the `inline` pass to detect its own change.

Two changes:

1. Hash structure directly into a `Hasher` instead of materializing a `String`.
   The fingerprint's only contract is that equal structures hash equally and
   the value is stable within a run; it is never persisted or compared across
   builds.
2. Have `ssa::inline::inline` report whether it changed anything, removing two
   of the three fingerprint calls per round.

Change 2 alters a change-detection signal, which can alter round counts, which
can alter output. It ships as its own commit, gated on the output hash, and is
reverted if the hash moves.

### Phase 4: Allocation Churn

235 million allocations for a 7.8 MB input. `inline` (6.70 s) and `ssa`
(4.42 s, 53 M allocations) dominate what remains after phases 1-3.

The structural cause is that `Traverse` and `LocalRw` return `Vec` from every
method — `rvalues_mut`, `values_read`, `values_written` and friends — so every
AST node visit heap-allocates, across 176 call sites.

Change: return `SmallVec` with an inline capacity chosen from measured arity
distribution. `SmallVec` derefs to a slice and supports `into_iter`, so most
call sites are unchanged.

Evaluate `mimalloc` as global allocator separately and keep it only if it
measures.

Phase 4 is scoped after re-measurement, because phases 1-3 remove roughly a
third of total runtime and will reorder what remains.

### Phase 5: Cross-Function Parallelism

Deferred to last and explicitly bounded. With 56% of runtime in two functions,
the ceiling is near 2x. A bounded pool is required because peak memory is
per-function transient: an unbounded pool multiplies peak by the worker count,
trading the memory goal for the speed goal.

### Phase 6: Reconstruction Failure on `-g0` Round-Trip

`medal-full-v12-recompiled-g0.luac` fails after 26.5 s and 3.2 GB with
`reconstruction left internal goto, label, or set-list nodes`, emitting
nothing.

Work: reduce to the smallest failing prototype, identify which pass leaves the
internal nodes, fix, and confirm the stage-27 hash is unchanged. The minimized
prototype is retained as a corpus fixture.

This ships as its own commit, separate from all performance work, because it is
the one change permitted to alter output — for an input that currently produces
none.

## Test Corpus

Three tiers.

**Repository corpus.** The existing `tests/luau_corpus` cases across all
profiles, via `tools/run_luau_corpus.py`. Guards correctness.

**Generated stress corpus.** `tools/gen_stress_luau.py` emits deterministic
Luau at 1k, 2.5k, 5k, and 10k lines in shapes drawn from deobfuscated output:
deep closure nests, wide flat `if`/`elseif` dispatch chains, large table
constructors, long straight-line register churn, wide `and`/`or` chains, and
large numeric-for bodies. Compiled with the bundled
`.tools/luau-windows/luau-compile.exe` across `-O0/-O1/-O2` and `-g0/-g1/-g2`.

Sources are committed; bytecode is generated at benchmark time. This gives a
reproducible scaling curve that does not depend on files outside the
repository.

**Real captures.** Located through a `MEDAL_CAPTURE_ROOT` environment
variable, skipped when unset. Too large to commit, and the primary evidence for
whether the work succeeded. stage-27 with its recorded hash is the gate.

## Acceptance Gates

Every phase must pass all four before it lands:

1. `cargo +nightly test --workspace`
2. `python tools/run_luau_corpus.py --profiles all`
3. stage-27 output SHA-256 equals
   `4eda076821e7edfdccb6517e464aee9b2d97ece7365a010fd8979ca41a241544`
4. Generated stress corpus output hashes unchanged from the previous phase

Phase 6 is exempt from gate 3 only for the input it repairs; stage-27 must
still match.

Time and peak memory are recorded per phase. A phase whose measurement does not
justify its complexity is reverted.

## Success Criteria

- Wall clock on stage-27 below 15 s, from 33.9 s.
- Peak resident set on stage-27 below 1,200 MB, from 3,307 MB.
- Output byte-identical on every currently-succeeding input.
- `medal-full-v12-recompiled-g0.luac` produces parseable output.

The time target follows from phases 1-3 alone (~12 s of measured redundant
work) plus phase 5's bounded parallelism. The memory target assumes the
quadratic dominator sets are in fact the peak; if measurement after phase 1
contradicts that, the target is restated against evidence rather than pursued
by other means.
