# Bounded SSA Upvalue Analysis Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace exponential SSA upvalue path histories with deterministic,
bounded capture-epoch dataflow so the complete connected Luau chunk decompiles
correctly within five minutes and below 3 GiB peak memory.

**Architecture:** `cfg::ssa::upvalues` will assign compact IDs to reference
capture epochs, propagate one active epoch per original local with a deterministic
CFG worklist, and materialize the existing opening-location interface with one
canonical location per epoch. SSA construction will propagate fallible allocation
errors to the Luau decompiler boundary instead of allowing allocator aborts.

**Tech Stack:** Rust nightly, `petgraph`, `rustc_hash`, `rangemap`, the existing
Luau corpus runner, and the pinned Luau Windows compiler.

## Global Constraints

- Do not special-case chunk hashes, prototype IDs, instruction positions, or
  byte patterns.
- Do not split, stub, truncate, skip, or simplify output to reduce resource use.
- Preserve textual output for representative inputs that already decompile and
  semantic behavior for the complete trusted corpus.
- Treat 3 GiB as a hard peak-memory ceiling, not an operating target.
- Finish the current diagnostic chunk in at most five minutes.
- Never execute the recovered suspicious payload; validation is static,
  parse/recompile based, plus runtime checks only for the trusted corpus fixtures.

---

### Task 1: Lock the exponential CFG failure into a focused test

**Files:**
- Modify: `cfg/src/ssa/upvalues.rs`

**Interfaces:**
- Consumes: existing `UpvaluesOpen::new(&Function, FxHashMap<RcLocal, RcLocal>)`.
- Produces: a regression proving each materialized open state contains one
  canonical location rather than one entry per control-flow path.

- [ ] **Step 1: Add a diamond-CFG regression helper and assertion**

Add a `#[cfg(test)]` module to `cfg/src/ssa/upvalues.rs`. Build one initial
`Upvalue::Ref` capture followed by eight repeated diamonds:

```rust
fn capture(local: &RcLocal) -> ast::Statement {
    ast::Assign::new(
        vec![RcLocal::default().into()],
        vec![ast::Closure {
            function: Default::default(),
            upvalues: vec![ast::Upvalue::Ref(local.clone())],
        }
        .into()],
    )
    .into()
}

fn diamond_chain(depth: usize) -> (Function, RcLocal) {
    let captured = RcLocal::default();
    let mut function = Function::new(0);
    let entry = function.new_block();
    function.set_entry(entry);
    function.block_mut(entry).unwrap().push(capture(&captured));
    let mut tail = entry;

    for _ in 0..depth {
        let left = function.new_block();
        let right = function.new_block();
        let merge = function.new_block();
        function
            .graph_mut()
            .add_edge(tail, left, BlockEdge::new(BranchType::Then));
        function
            .graph_mut()
            .add_edge(tail, right, BlockEdge::new(BranchType::Else));
        function
            .graph_mut()
            .add_edge(left, merge, BlockEdge::default());
        function
            .graph_mut()
            .add_edge(right, merge, BlockEdge::default());
        tail = merge;
    }

    (function, captured)
}
```

Create the old-local identity map with `captured -> captured`, analyze the
function, and calculate the maximum stored location-vector length across
`UpvaluesOpen::open`. Assert that it is exactly `1`.

- [ ] **Step 2: Run the test and verify the current implementation fails**

Run:

```bash
cargo +nightly test -p cfg diamond_paths_keep_one_canonical_opening -- --nocapture
```

Expected: `FAIL`; the current vector length is greater than `1` because each
diamond concatenates duplicate path histories.

- [ ] **Step 3: Record representative pre-change output**

Run:

```bash
python tools/run_luau_corpus.py --profiles primary --case 16_closure_capture --output tests/luau_corpus/results/upvalue-before
python tools/run_luau_corpus.py --profiles primary --case 17_mutable_upvalue --output tests/luau_corpus/results/upvalue-before
```

Keep this ignored result directory for the post-change byte comparison.

- [ ] **Step 4: Commit only the failing regression**

```bash
git add cfg/src/ssa/upvalues.rs
git commit -m "test: reproduce SSA upvalue path explosion"
```

---

### Task 2: Replace path histories with capture epochs

**Files:**
- Modify: `cfg/src/ssa/upvalues.rs`
- Modify: `cfg/src/ssa/construct.rs`

**Interfaces:**
- Consumes: `Function`, original-local identities, reference captures, and
  `Close` statements.
- Produces: `UpvaluesOpen::try_new(...) -> Result<UpvaluesOpen, UpvalueAnalysisError>`
  and the existing `open` range map containing one canonical opening location.

- [ ] **Step 1: Add focused merge, loop, and close tests**

Add tests beside the diamond regression for these exact invariants:

```rust
assert_eq!(analysis.opening_location(merge, &captured, 0), Some(first_capture));
assert_eq!(analysis.opening_location(after_loop, &captured, 0), Some(first_capture));
assert_eq!(analysis.opening_location(after_close, &captured, 0), None);
```

The merge fixture must capture the same local on two branches. The loop fixture
must carry an open capture over a back edge. The close fixture must place
`ast::Close { locals: vec![captured.clone()] }` before its queried statement.

- [ ] **Step 2: Run the focused tests and verify the new API fails**

Run:

```bash
cargo +nightly test -p cfg ssa::upvalues::tests -- --nocapture
```

Expected: compile failure because `opening_location` and `try_new` are not
implemented.

- [ ] **Step 3: Implement epoch identities and deterministic union**

In `cfg/src/ssa/upvalues.rs`, add:

```rust
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct EpochId(usize);

#[derive(Debug, thiserror::Error)]
pub enum UpvalueAnalysisError {
    #[error("unable to reserve bounded upvalue state: {0}")]
    Resource(String),
}

#[derive(Default)]
struct EpochForest {
    parents: Vec<EpochId>,
    sites: Vec<(NodeIndex, usize)>,
}
```

Implement `create`, path-compressing `find`, and `union`. `union` must choose
the lexicographically smallest `(node.index(), statement)` site as the
canonical representative so graph traversal order cannot change output.

- [ ] **Step 4: Implement bounded worklist propagation**

Pre-scan reference captures and assign one epoch per `(original local, node,
statement)`. Use `VecDeque<NodeIndex>` ordered by `NodeIndex::index()` and keep
one `FxHashMap<RcLocal, EpochId>` entry state and exit state per block.

The transfer rules are:

```text
Ref capture with no active epoch -> activate its site epoch
Ref capture with an active epoch -> union site epoch with active epoch
Close(local)                     -> remove local from active state
CFG merge                        -> union all present epochs for each local
```

Requeue sorted successors only when their normalized entry or exit state
changes. Pre-reserve node maps, worklist storage, epoch arrays, and nested state
maps with `try_reserve`; map failures to `UpvalueAnalysisError::Resource`.

- [ ] **Step 5: Materialize the existing compressed range interface**

After the worklist reaches a fixed point, scan each block once from its final
entry state. Insert ranges containing `vec![canonical_site]`; never store
duplicate sites. Add:

```rust
fn opening_location(
    &self,
    node: NodeIndex,
    local: &RcLocal,
    statement: usize,
) -> Option<(NodeIndex, usize)>;
```

Update `SsaConstructor::mark_upvalues` to use `opening_location` instead of
indexing `open_locations.first()`.

- [ ] **Step 6: Run the focused tests**

Run:

```bash
cargo +nightly test -p cfg ssa::upvalues::tests -- --nocapture
```

Expected: all upvalue tests pass, and the diamond fixture stores one location
per state.

- [ ] **Step 7: Commit the bounded analysis**

```bash
git add cfg/src/ssa/upvalues.rs cfg/src/ssa/construct.rs
git commit -m "fix: bound SSA upvalue propagation"
```

---

### Task 3: Propagate resource errors without allocator aborts

**Files:**
- Modify: `cfg/src/ssa.rs`
- Modify: `cfg/src/ssa/construct.rs`
- Modify: `luau-lifter/src/lib.rs`
- Modify: `lua51-lifter/src/main.rs`
- Modify: `lua51-lifter/src/lifter.rs`

**Interfaces:**
- Consumes: `UpvalueAnalysisError` from Task 2.
- Produces: `cfg::ssa::construct(...) -> Result<SsaOutput, SsaError>` and a
  Luau `DecompileError` with phase `Ssa` for allocation failures.

- [ ] **Step 1: Add the failing Luau error-conversion test**

Add a focused test in `luau-lifter/src/lib.rs` that calls a small helper
converting this error:

```rust
cfg::ssa::SsaError::Upvalues(
    cfg::ssa::upvalues::UpvalueAnalysisError::Resource("capacity".into()),
)
```

Assert:

```rust
assert_eq!(error.phase, DecompilePhase::Ssa);
assert_eq!(error.invariant, "bounded SSA analysis");
assert!(error.detail.contains("capacity"));
```

- [ ] **Step 2: Run the test and verify it fails**

Run:

```bash
cargo +nightly test -p luau-lifter bounded_ssa_error_is_structured -- --nocapture
```

Expected: compile failure because `SsaError` and the conversion helper do not
exist.

- [ ] **Step 3: Add and propagate `SsaError`**

Define in `cfg/src/ssa.rs`:

```rust
#[derive(Debug, thiserror::Error)]
pub enum SsaError {
    #[error(transparent)]
    Upvalues(#[from] upvalues::UpvalueAnalysisError),
}
```

Change `SsaConstructor::mark_upvalues`, `SsaConstructor::construct`, and public
`cfg::ssa::construct` to return `Result`. Use `?` for `UpvaluesOpen::try_new`.

In `luau-lifter/src/lib.rs`, convert the error with:

```rust
DecompileError::new(
    DecompilePhase::Ssa,
    Some(function_id),
    None,
    "bounded SSA analysis",
    error.to_string(),
)
```

Update the Lua 5.1 CLI iterator to collect `anyhow::Result<FxHashMap<_, _>>`
and update its lifter test to call `.expect("SSA construction")`.

- [ ] **Step 4: Run all affected Rust packages**

Run:

```bash
cargo +nightly test -p cfg -p luau-lifter -p lua51-lifter -- --nocapture
```

Expected: all tests pass and allocation failures have a structured error path.

- [ ] **Step 5: Commit error propagation**

```bash
git add cfg/src/ssa.rs cfg/src/ssa/construct.rs luau-lifter/src/lib.rs lua51-lifter/src/main.rs lua51-lifter/src/lifter.rs
git commit -m "fix: report bounded SSA allocation failures"
```

---

### Task 4: Prove output stability and full-chunk resource bounds

**Files:**
- Generate, ignored: `tests/luau_corpus/results/upvalue-after/**`
- Generate, ignored: `tests/luau_corpus/results/connected-root-v1/connected-root.luau`

**Interfaces:**
- Consumes: the optimized `luau-lifter.exe`.
- Produces: textual-diff evidence, parse/recompile evidence, elapsed time, peak
  working set, and complete connected output.

- [ ] **Step 1: Build the current executable**

```bash
cargo +nightly build -p luau-lifter
```

- [ ] **Step 2: Compare representative output byte-for-byte**

```bash
python tools/run_luau_corpus.py --profiles primary --case 16_closure_capture --output tests/luau_corpus/results/upvalue-after --no-build
python tools/run_luau_corpus.py --profiles primary --case 17_mutable_upvalue --output tests/luau_corpus/results/upvalue-after --no-build
diff -ru tests/luau_corpus/results/upvalue-before tests/luau_corpus/results/upvalue-after
```

Expected: no source differences caused by the internal representation change.

- [ ] **Step 3: Decompile isolated prototype 31**

Run the current executable against:

```text
C:/Users/Admin/Desktop/Script/captures/sUNCm0m3n7-d7140e4f7546-hardened/stage-120-prototype-0031-parent-view-v1/prototype-view.luac
```

Capture output under `tests/luau_corpus/results/connected-root-v1/`. Poll the
Windows process working set at 100 ms intervals. Require exit `0`, elapsed time
under five minutes, and peak working set below 3,221,225,472 bytes.

- [ ] **Step 4: Decompile the complete connected chunk**

Use the same measurement for:

```text
C:/Users/Admin/Desktop/Script/captures/sUNCm0m3n7-d7140e4f7546-hardened/stage-128-recovered-source-package-v1/bytecode/connected-root.luac
```

Write stdout to
`tests/luau_corpus/results/connected-root-v1/connected-root.luau`. Require
exit `0`, no allocator abort, elapsed time under five minutes, and peak working
set below 3 GiB.

- [ ] **Step 5: Validate complete static structure**

Run:

```bash
.tools/luau-windows/luau-compile.exe --only-parse tests/luau_corpus/results/connected-root-v1/connected-root.luau
.tools/luau-windows/luau-compile.exe --binary -O1 -g1 tests/luau_corpus/results/connected-root-v1/connected-root.luau > tests/luau_corpus/results/connected-root-v1/recompiled.luac
```

Use the existing static v6 reader in `C:/Users/Admin/Desktop/Script/stage4` to
confirm both the input and recompiled chunks contain 380 prototypes and that
the decompiled source contains no diagnostic stubs, cut-edge markers, or
unsupported-operation placeholders.

- [ ] **Step 6: Commit only source changes if measurement required corrections**

Do not commit generated result artifacts. If no correction was required, make
no commit for this task.

---

### Task 5: Run the completion regression gates

**Files:**
- Generate, ignored: `tests/luau_corpus/results/bounded-upvalues-final/**`

**Interfaces:**
- Consumes: final optimized decompiler.
- Produces: workspace, harness, parse, and trusted semantic evidence for the
  correctness-spine completion audit.

- [ ] **Step 1: Run formatting and static diff checks**

```bash
cargo +nightly fmt --all -- --check
git diff --check
```

- [ ] **Step 2: Run the complete Rust and Python suites**

```bash
cargo +nightly test --workspace
python -m unittest discover -s tests/python -v
```

- [ ] **Step 3: Run all corpus profiles with trusted semantic probes**

```bash
python tools/run_luau_corpus.py --profiles all --semantic --no-build --output tests/luau_corpus/results/bounded-upvalues-final
```

Require successful parse/recompile for all supported profiles. Confirm exact
trusted results for:

```text
04_calls_multireturn -> q=5:4, 12
05_varargs           -> preserves every expected value
13_repeat_until      -> 10, 6
15_generic_for       -> returns the expected table
20_pcall_style_flow  -> 7, 8 for the successful path
21_state_machine     -> done, 1, 3
```

- [ ] **Step 4: Review the final source diff and branch state**

```bash
git status --short
git diff HEAD~3 --stat
git log -6 --oneline --decorate
```

Verify that only the approved SSA optimization, error propagation, tests, and
design/plan documentation are present.

If verification reveals a source or test defect, return to the task that owns
that file, repeat its red-green cycle, and amend it with a new focused commit.
Do not create an empty verification commit.
