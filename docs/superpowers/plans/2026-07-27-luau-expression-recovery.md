# Luau Expression Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Recover source-like Luau expressions from structured AST evidence without moving observable work, changing call multiplicity, or crossing multi-return boundaries.

**Architecture:** Add a first-class conditional-expression node plus one post-SSA AST pass that folds only locally provable assignment, short-circuit, and single-use temporary shapes. Keep compound-assignment selection in the formatter because it changes syntax, not AST semantics. Run expression recovery after alias elimination and before local declaration placement.

**Tech Stack:** Rust nightly, existing AST/CFG/restructure crates, bundled Luau compiler, Python static corpus harness.

## Global Constraints

- Never execute source or decompiled scripts.
- Preserve left-to-right evaluation and the exact number of calls and metamethod-capable operations.
- Preserve local and upvalue snapshots across calls and mutations.
- Preserve single-result selection versus open multiple returns.
- Do not inspect corpus filenames, constants, URLs, register numbers, or generated names.
- Keep V4-V12 compatibility intact.
- Use focused behavioral tests; do not add broad golden-output fixtures.

---

### Task 1: Conditional Expression AST

**Files:**
- Create: `ast/src/conditional.rs`
- Modify: `ast/src/lib.rs`
- Modify: `ast/src/formatter.rs`
- Test: `ast/src/conditional.rs`

**Interfaces:**
- Produces: `Conditional::new(condition, then_value, else_value) -> Conditional`.
- Produces: `RValue::Conditional(Conditional)`.
- Preserves: recursive `LocalRw`, `Traverse`, and `SideEffects` behavior.

- [x] **Step 1: Write failing formatter and traversal tests**

Construct literal and local-backed conditional expressions and assert:

```rust
assert_eq!(
    Conditional::new(condition.into(), yes.into(), no.into()).to_string(),
    "if v1 then \"yes\" else \"no\""
);
```

Also assert that all three operands contribute reads and side effects.

- [x] **Step 2: Run the focused test and verify the missing node fails**

Run:

```bash
cargo +nightly test -p ast conditional --offline
```

Expected: compile failure because `Conditional` and `RValue::Conditional` do not exist.

- [x] **Step 3: Implement the node and formatter**

Add boxed `condition`, `then_value`, and `else_value` fields. Format nested else-side conditional expressions using `elseif`; otherwise emit:

```luau
if condition then thenValue else elseValue
```

Treat the expression as lower precedence than binary operators so embedding remains unambiguous.

- [x] **Step 4: Run focused and AST tests**

Run:

```bash
cargo +nightly test -p ast --offline
```

Expected: all AST tests pass.

- [x] **Step 5: Commit**

```bash
git add ast/src/conditional.rs ast/src/lib.rs ast/src/formatter.rs
git commit -m "feat: add Luau conditional expressions"
```

---

### Task 2: Conditional Assignment and Short-Circuit Folding

**Files:**
- Create: `ast/src/expression_recovery.rs`
- Modify: `ast/src/lib.rs`
- Modify: `luau-lifter/src/lib.rs`
- Test: `ast/src/expression_recovery.rs`

**Interfaces:**
- Produces: `recover_expressions_with_protected(block: &mut Block, protected: &[RcLocal]) -> ExpressionRecoveryStats`.
- Produces: counters for recovered conditionals, short circuits, and inlined temporaries.
- Consumes: incoming upvalues supplied by `decompile_function`.

- [x] **Step 1: Write a failing conditional-assignment test**

Build:

```luau
if condition then
    result = false
else
    result = fallback()
end
```

Assert the pass produces one assignment whose right side is:

```luau
if condition then false else fallback()
```

The falsy branch proves this is a real conditional expression, not the unsafe `condition and false or fallback()` encoding.

- [x] **Step 2: Write failing safety tests**

Assert no fold when:

- either branch has an extra statement;
- branches assign different locals;
- a branch omits the assignment;
- a branch expression captures or reads the assigned local during a declaration-sensitive initialization.

- [x] **Step 3: Run and verify the focused failures**

Run:

```bash
cargo +nightly test -p ast expression_recovery --offline
```

Expected: compile failure because the pass does not exist.

- [x] **Step 4: Implement conditional folding**

Recursively visit structured blocks. Replace an `If` only when both branches contain exactly one one-local/one-value assignment to the same local. Move the condition and branch values into `RValue::Conditional`, and keep the local identity unchanged for later declaration placement.

- [x] **Step 5: Write and verify a failing short-circuit-chain test**

Build:

```luau
result = first and second
if result then
    result = third
end
```

Assert recovery produces:

```luau
result = first and second and third
```

Add negative tests for a non-empty else branch, a different target, a target read inside `third`, and reference-captured/incoming upvalues.

- [x] **Step 6: Implement short-circuit folding**

Require adjacent statements, exact target identity, an empty else block, and no target read or protected capture in the appended expression. Preserve operand order by appending the expression to the right of the existing value.

- [x] **Step 7: Integrate the pass**

Call expression recovery immediately after:

```rust
ast::eliminate_aliases_with_protected(&mut block, &upvalues_in);
```

and before `LocalDeclarer`.

- [x] **Step 8: Run focused and workspace tests**

Run:

```bash
cargo +nightly test -p ast expression_recovery --offline
cargo +nightly test --workspace --offline
```

Expected: all tests pass.

- [x] **Step 9: Commit**

```bash
git add ast/src/expression_recovery.rs ast/src/lib.rs luau-lifter/src/lib.rs
git commit -m "feat: recover Luau conditional expressions"
```

---

### Task 3: Single-Use Expression and Call-Result Inlining

**Files:**
- Modify: `ast/src/expression_recovery.rs`
- Modify: `ast/src/formatter.rs`
- Test: `ast/src/expression_recovery.rs`

**Interfaces:**
- Extends: `recover_expressions_with_protected`.
- Preserves: `RValue::Select` when an assignment captured exactly one call result.

- [x] **Step 1: Write failing direct-use tests**

Cover:

```luau
local temporary = arithmetic
return temporary
```

and:

```luau
local temporary = (produce())
return temporary
```

Assert the first becomes `return arithmetic`; assert the second becomes `return (produce())`, not `return produce()`.

- [x] **Step 2: Write failing ordering tests**

Keep the temporary when inlining would move it after:

- another call;
- an index operation;
- a metamethod-capable unary or binary expression;
- a write to any local read by the candidate;
- an incoming or reference-captured upvalue boundary.

- [x] **Step 3: Run and verify the failures**

Run:

```bash
cargo +nightly test -p ast expression_recovery --offline
```

Expected: assertions fail because non-alias expressions are not inlined.

- [x] **Step 4: Implement ordered single-use inlining**

Require one local target, one later read, no target overwrite, no source-local write, and no structured-control boundary. Move the expression only across effect-free statements and only into the consumer prefix before any observable expression. Preserve the original `Call` versus `Select::Call` variant.

- [x] **Step 5: Make return formatting preserve single-result selection**

Wrap a final `RValue::Select` in a return list:

```luau
return (produce())
```

Do not wrap non-final values because Luau already selects one result there.

- [x] **Step 6: Run focused and AST tests**

Run:

```bash
cargo +nightly test -p ast --offline
```

Expected: all AST tests pass.

- [x] **Step 7: Commit**

```bash
git add ast/src/expression_recovery.rs ast/src/formatter.rs
git commit -m "feat: inline single-use Luau expressions"
```

---

### Task 4: Compound Assignment Formatting

**Files:**
- Modify: `ast/src/formatter.rs`
- Test: `ast/src/formatter.rs`

**Interfaces:**
- Preserves: `Assign` as the semantic AST node.
- Produces: compound syntax for `Add`, `Sub`, `Mul`, `Div`, `IDiv`, `Mod`, `Pow`, and `Concat`.

- [x] **Step 1: Write failing local and indexed formatting tests**

Assert:

```luau
value = value + increment
```

formats as:

```luau
value += increment
```

and a structurally identical local-backed `table[key]` update formats with `+=`.

- [x] **Step 2: Write failing rejection tests**

Keep ordinary assignment for:

- multiple left or right values;
- a different left operand;
- `other + value`;
- `and` and `or`;
- indexed object/key expressions containing calls, indexing, or metamethod-capable calculations.

- [x] **Step 3: Run and verify the focused failures**

Run:

```bash
cargo +nightly test -p ast formatter::tests::compound --offline
```

Expected: existing formatter emits `=`.

- [x] **Step 4: Implement formatter detection**

Match only `left = left <op> right`. Local targets are safe. Indexed targets require structurally equal reads and effect-free local/global/literal object and key components so compound syntax does not change evaluation count.

- [x] **Step 5: Run AST tests and compile a syntax probe**

Run:

```bash
cargo +nightly test -p ast --offline
.tools/luau-windows/luau-compile.exe --null tests/luau_corpus/cases/24_wonky_integration.luau
```

Expected: tests and static compilation pass.

- [x] **Step 6: Commit**

```bash
git add ast/src/formatter.rs
git commit -m "feat: format safe Luau compound assignments"
```

---

### Task 5: Corpus Proof and Pull Request

**Files:**
- Modify: `docs/decompiler-baseline-findings.md`
- Generated and ignored: `tests/luau_corpus/results/expression-after/**`

**Interfaces:**
- Consumes: the same 240 source/profile pairs as the alias baseline.
- Produces: exact before/after counts and representative source/output comparisons.

- [x] **Step 1: Run focused corpus cases**

Run:

```bash
python tools/run_luau_corpus.py --profiles all --case 10_short_circuit --no-build --output tests/luau_corpus/results/expression-focused-short
python tools/run_luau_corpus.py --profiles all --case 11_conditional_expression --no-build --output tests/luau_corpus/results/expression-focused-conditional
python tools/run_luau_corpus.py --profiles all --case 24_wonky_integration --no-build --output tests/luau_corpus/results/expression-focused-wonky
```

Expected: 30/30 compile, decompile, and recompile checks pass.

- [x] **Step 2: Run the full static matrix**

Run:

```bash
cargo +nightly test --workspace --offline
python -m unittest discover -s tests/python -v
python tools/run_luau_corpus.py --profiles all --no-build --output tests/luau_corpus/results/expression-after
```

Expected: all Rust/Python tests pass; 240/240 corpus rows pass; generated gotos remain zero.

- [x] **Step 3: Compare output**

Record total locals/statements/lines plus counts of recovered conditional expressions, compound assignments, and split short-circuit assignment/`if` pairs. Review cases 02, 04, 10, 11, and 24 across O0/g1, O1/g1, O2/g1, O2/g0, V9, and V12.

- [x] **Step 4: Document evidence**

Add only comparable before/after counts, safety boundaries, remaining compiler-erased ambiguity, and representative snippets to `docs/decompiler-baseline-findings.md`.

- [x] **Step 5: Final review**

Review the complete diff for corpus-specific logic, evaluation-order changes, closure/upvalue capture changes, and multi-return regressions. Repair every Critical or Important finding and repeat the affected gates.

- [ ] **Step 6: Commit, push, and open a draft stacked PR**

```bash
git add docs/decompiler-baseline-findings.md
git commit -m "docs: record expression recovery results"
git push -u origin agent/expression-recovery
```

Open the PR against `agent/alias-copy-elimination`. Keep it draft and unmerged until PR #2 is approved, merged, and the expression PR is retargeted and explicitly approved.
