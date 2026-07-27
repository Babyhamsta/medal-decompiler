# Luau Alias and Copy Elimination Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove bytecode-register aliases and copy chains from decompiled Luau when static data-flow evidence supports a more direct source form.

**Architecture:** Add a focused post-SSA AST pass that runs after CFG restructuring and before declaration placement. The pass performs local def-use analysis, replaces safe alias reads in evaluation order, removes dead copy assignments, and recurses through structured blocks without rewriting formatted text.

**Tech Stack:** Rust 2024, existing `ast`/`cfg`/`luau-lifter` crates, Luau compiler CLI under `.tools`, Python 3.11 corpus harness, GitHub branch/PR workflow.

## Global Constraints

- Arbitrary source or decompiled output must never be executed.
- All decisions must use SSA/AST identity, local reads and writes, evaluation order, closure capture, or side-effect evidence.
- No production rule may inspect corpus filenames, exact constants, URLs, register numbers, or formatted variable names.
- Preserve call order, metamethod-capable operations, upvalue snapshots, mutation visibility, and multiple-return behavior.
- Keep V4-V12 compatibility and all existing static round-trip gates green.
- Work on `agent/alias-copy-elimination`; publish one PR targeting `main`; merge only after user approval.

---

### Task 1: Lock the visible alias regression

**Files:**
- Modify: `luau-lifter/src/compatibility_tests.rs`

**Interfaces:**
- Consumes: existing `compile`, `source`, `PROFILES`, and `try_decompile_bytecode`.
- Produces: `trivial_local_alias_lines(source: &str) -> Vec<&str>` test helper and a failing V12 wonky-output regression.

- [ ] **Step 1: Add the identifier and alias-line test helpers**

Add these test-only helpers below `compile`:

```rust
fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn trivial_local_alias_lines(source: &str) -> Vec<&str> {
    source
        .lines()
        .filter(|line| {
            let Some(rest) = line.trim().strip_prefix("local ") else {
                return false;
            };
            let Some((left, right)) = rest.split_once(" = ") else {
                return false;
            };
            is_identifier(left) && is_identifier(right)
        })
        .collect()
}
```

- [ ] **Step 2: Add the failing representative-output test**

Add:

```rust
#[test]
fn wonky_v12_output_has_no_trivial_local_aliases() {
    if !compiler().is_file() {
        eprintln!("skipping: bundled Luau compiler is absent");
        return;
    }

    let compiled = compile("binary", &PROFILES[3], &source("24_wonky_integration"));
    assert!(compiled.status.success());
    let decompiled = crate::try_decompile_bytecode(&compiled.stdout, 1).unwrap();
    let aliases = trivial_local_alias_lines(&decompiled);

    assert!(aliases.is_empty(), "trivial aliases remained: {aliases:#?}\n{decompiled}");
}
```

- [ ] **Step 3: Run the focused test and verify RED**

Run:

```powershell
cargo +nightly test -p luau-lifter wonky_v12_output_has_no_trivial_local_aliases -- --nocapture
```

Expected: FAIL listing the generated local-to-local copy inside `Machine.new`. The failure must be an assertion failure, not a compiler or deserializer error.

- [ ] **Step 4: Commit the regression**

```powershell
git add luau-lifter/src/compatibility_tests.rs
git commit -m "test: expose redundant Luau aliases"
```

---

### Task 2: Add single-use AST alias propagation

**Files:**
- Create: `ast/src/alias_elimination.rs`
- Modify: `ast/src/lib.rs`

**Interfaces:**
- Consumes: `Block`, `Statement`, `Assign`, `LValue`, `RValue`, `LocalRw`, `SideEffects`, `Traverse`, and `RcLocal`.
- Produces: `pub fn eliminate_aliases(block: &mut Block) -> usize`.

- [ ] **Step 1: Add a failing pure-prefix unit test**

Create `ast/src/alias_elimination.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use crate::{Assign, Block, LValue, Literal, LocalRw, RValue, RcLocal, Return};

    use super::eliminate_aliases;

    #[test]
    fn eliminates_single_use_alias_after_pure_values() {
        let source = RcLocal::default();
        let alias = RcLocal::default();
        let mut block = Block(vec![
            Assign::new(
                vec![LValue::Local(alias.clone())],
                vec![RValue::Local(source.clone())],
            )
            .into(),
            Return::new(vec![
                Literal::String(b"prefix".to_vec()).into(),
                RValue::Local(alias.clone()),
            ])
            .into(),
        ]);

        assert_eq!(eliminate_aliases(&mut block), 1);
        assert_eq!(block.len(), 1);
        assert!(!block[0].values_read().contains(&&alias));
        assert!(block[0].values_read().contains(&&source));
    }
}
```

Expose the module and function in `ast/src/lib.rs`:

```rust
mod alias_elimination;
pub use alias_elimination::*;
```

- [ ] **Step 2: Run the unit test and verify RED**

Run:

```powershell
cargo +nightly test -p ast eliminates_single_use_alias_after_pure_values -- --nocapture
```

Expected: compilation FAIL because `eliminate_aliases` has not been defined.

- [ ] **Step 3: Implement same-block single-use propagation**

Implement:

```rust
use crate::{Block, LocalRw, LValue, RValue, RcLocal, Statement};

fn alias_assignment(statement: &Statement) -> Option<(RcLocal, RcLocal)> {
    let assign = statement.as_assign()?;
    if assign.left.len() != 1 || assign.right.len() != 1 {
        return None;
    }

    let LValue::Local(alias) = &assign.left[0] else {
        return None;
    };
    let RValue::Local(source) = &assign.right[0] else {
        return None;
    };

    Some((alias.clone(), source.clone()))
}

fn single_read_statement(block: &Block, start: usize, alias: &RcLocal) -> Option<usize> {
    let mut read_at = None;
    for (index, statement) in block.iter().enumerate().skip(start) {
        if statement.values_written().contains(&alias) {
            return None;
        }
        for read in statement.values_read() {
            if read == alias {
                if read_at.is_some() {
                    return None;
                }
                read_at = Some(index);
            }
        }
    }
    read_at
}

pub fn eliminate_aliases(block: &mut Block) -> usize {
    let mut removed = 0;
    let mut index = 0;
    while index < block.len() {
        let Some((alias, source)) = alias_assignment(&block[index]) else {
            index += 1;
            continue;
        };
        let Some(read_at) = single_read_statement(block, index + 1, &alias) else {
            index += 1;
            continue;
        };
        if block[index + 1..=read_at]
            .iter()
            .any(|statement| statement.values_written().contains(&&source))
        {
            index += 1;
            continue;
        }

        block[read_at].replace_values_read(&alias, &source);
        block.remove(index);
        removed += 1;
    }
    removed
}
```

This is the minimal GREEN implementation. It deliberately handles only one read and no source write. Evaluation-order barriers are added before integration in Task 3.

- [ ] **Step 4: Run the focused AST test**

Run:

```powershell
cargo +nightly test -p ast eliminates_single_use_alias_after_pure_values -- --nocapture
```

Expected: PASS.

- [ ] **Step 5: Commit the minimal pass**

```powershell
git add ast/src/alias_elimination.rs ast/src/lib.rs
git commit -m "feat: propagate single-use Luau aliases"
```

---

### Task 3: Protect snapshots, effects, and closure captures

**Files:**
- Modify: `ast/src/alias_elimination.rs`

**Interfaces:**
- Consumes: Task 2 `eliminate_aliases`.
- Produces: evaluation-order-aware propagation that rejects writes, calls before the use, indexed accesses before the use, and closure capture ambiguity.

- [ ] **Step 1: Add failing mutation and effect-barrier tests**

Add:

```rust
#[test]
fn keeps_alias_when_source_changes_before_use() {
    let source = RcLocal::default();
    let alias = RcLocal::default();
    let mut block = Block(vec![
        Assign::new(vec![alias.clone().into()], vec![source.clone().into()]).into(),
        Assign::new(
            vec![source.clone().into()],
            vec![Literal::Number(2.0).into()],
        )
        .into(),
        Return::new(vec![alias.clone().into()]).into(),
    ]);

    assert_eq!(eliminate_aliases(&mut block), 0);
    assert_eq!(block.len(), 3);
}

#[test]
fn keeps_snapshot_alias_when_call_runs_before_use() {
    let source = RcLocal::default();
    let alias = RcLocal::default();
    let call = crate::Call::new(crate::Global::from("mutate").into(), Vec::new());
    let mut block = Block(vec![
        Assign::new(vec![alias.clone().into()], vec![source.clone().into()]).into(),
        Return::new(vec![call.into(), alias.clone().into()]).into(),
    ]);

    assert_eq!(eliminate_aliases(&mut block), 0);
    assert!(block[0].values_written().contains(&&alias));
}
```

- [ ] **Step 2: Run both tests and verify RED**

Run:

```powershell
cargo +nightly test -p ast keeps_alias -- --nocapture
```

Expected: the mutation test may already pass; the call-before-use test must FAIL because Task 2 replaces the snapshot alias without checking expression order.

- [ ] **Step 3: Implement ordered replacement**

Add:

```rust
use crate::{PreOrPost, SideEffects, Traverse};
use itertools::Either;

fn replace_after_safe_prefix(
    statement: &mut Statement,
    alias: &RcLocal,
    source: &RcLocal,
) -> bool {
    let mut crossed_effect = false;
    statement
        .traverse_values(&mut |position, value| {
            let Either::Right(rvalue) = value else {
                return None;
            };
            if matches!(position, PreOrPost::Post) {
                if matches!(rvalue, RValue::Local(local) if local == alias) {
                    if crossed_effect {
                        return Some(false);
                    }
                    *rvalue = source.clone().into();
                    return Some(true);
                }
                if rvalue.has_side_effects()
                    && !matches!(rvalue, RValue::Global(_) | RValue::Literal(_))
                {
                    crossed_effect = true;
                }
            }
            None
        })
        .unwrap_or(false)
}
```

Replace Task 2's unconditional `replace_values_read` call with:

```rust
if !replace_after_safe_prefix(&mut block[read_at], &alias, &source) {
    index += 1;
    continue;
}
```

The `Global` exception reconstructs compiler-reordered global/import lookup without allowing a completed call, method call, or index operation to cross the snapshot.

- [ ] **Step 4: Add a closure-capture regression**

Add:

```rust
#[test]
fn keeps_alias_captured_by_reference() {
    let source = RcLocal::default();
    let alias = RcLocal::default();
    let holder = RcLocal::default();
    let closure = crate::Closure {
        function: by_address::ByAddress(triomphe::Arc::new(
            parking_lot::Mutex::new(crate::Function::default()),
        )),
        upvalues: vec![crate::Upvalue::Ref(alias.clone())],
    };
    let mut block = Block(vec![
        Assign::new(vec![alias.clone().into()], vec![source.clone().into()]).into(),
        Assign::new(vec![holder.clone().into()], vec![closure.into()]).into(),
        Assign::new(
            vec![source.clone().into()],
            vec![Literal::Number(2.0).into()],
        )
        .into(),
        Return::new(vec![holder.into()]).into(),
    ]);

    assert_eq!(eliminate_aliases(&mut block), 0);
    assert!(block[0].values_written().contains(&&alias));
}
```

This proves propagation does not redirect a reference capture from the alias
cell to the source cell.

- [ ] **Step 5: Run the full AST suite**

Run:

```powershell
cargo +nightly test -p ast -- --nocapture
```

Expected: all AST tests PASS.

- [ ] **Step 6: Commit the safety rules**

```powershell
git add ast/src/alias_elimination.rs
git commit -m "fix: preserve alias snapshot semantics"
```

---

### Task 4: Recurse through structured blocks and collapse copy chains

**Files:**
- Modify: `ast/src/alias_elimination.rs`

**Interfaces:**
- Consumes: safe same-block eliminator from Task 3.
- Produces: fixed-point elimination across every structured block and safe copy chains.

- [ ] **Step 1: Add failing nested-block and copy-chain tests**

```rust
#[test]
fn collapses_alias_chain_to_source() {
    let source = RcLocal::default();
    let alias_a = RcLocal::default();
    let alias_b = RcLocal::default();
    let mut block = Block(vec![
        Assign::new(vec![alias_a.clone().into()], vec![source.clone().into()]).into(),
        Assign::new(vec![alias_b.clone().into()], vec![alias_a.clone().into()]).into(),
        Return::new(vec![alias_b.into()]).into(),
    ]);

    assert_eq!(eliminate_aliases(&mut block), 2);
    assert_eq!(block.len(), 1);
    assert!(block[0].values_read().contains(&&source));
}

#[test]
fn eliminates_aliases_inside_structured_blocks() {
    let source = RcLocal::default();
    let alias = RcLocal::default();
    let nested = Block(vec![
        Assign::new(vec![alias.clone().into()], vec![source.clone().into()]).into(),
        Return::new(vec![alias.into()]).into(),
    ]);
    let mut block = Block(vec![
        crate::If::new(
            Literal::Boolean(true).into(),
            nested,
            Block::default(),
        )
        .into(),
    ]);

    assert_eq!(eliminate_aliases(&mut block), 1);
    let then_block = block[0].as_if().unwrap().then_block.lock();
    assert_eq!(then_block.len(), 1);
    assert!(then_block[0].values_read().contains(&&source));
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```powershell
cargo +nightly test -p ast alias_chain -- --nocapture
cargo +nightly test -p ast nested_alias -- --nocapture
```

Expected: at least the nested-block test FAILS because Task 3 only scans the supplied block.

- [ ] **Step 3: Add fixed-point and structured recursion**

Split the current body into `eliminate_block_once`. Implement public recursion:

```rust
pub fn eliminate_aliases(block: &mut Block) -> usize {
    let mut removed = 0;
    loop {
        let changed = eliminate_block_once(block);
        removed += changed;
        if changed == 0 {
            break;
        }
    }

    for statement in &mut block.0 {
        removed += match statement {
            Statement::If(value) => {
                eliminate_aliases(&mut value.then_block.lock())
                    + eliminate_aliases(&mut value.else_block.lock())
            }
            Statement::While(value) => eliminate_aliases(&mut value.block.lock()),
            Statement::Repeat(value) => eliminate_aliases(&mut value.block.lock()),
            Statement::NumericFor(value) => eliminate_aliases(&mut value.block.lock()),
            Statement::GenericFor(value) => eliminate_aliases(&mut value.block.lock()),
            _ => 0,
        };
    }
    removed
}
```

Do not recurse into closure function bodies here; each function is processed separately by `luau-lifter`.

- [ ] **Step 4: Run all AST tests**

Run:

```powershell
cargo +nightly test -p ast -- --nocapture
```

Expected: PASS.

- [ ] **Step 5: Commit recursive elimination**

```powershell
git add ast/src/alias_elimination.rs
git commit -m "feat: collapse nested Luau copy chains"
```

---

### Task 5: Integrate after SSA destruction

**Files:**
- Modify: `luau-lifter/src/lib.rs`
- Test: `luau-lifter/src/compatibility_tests.rs`

**Interfaces:**
- Consumes: `ast::eliminate_aliases(&mut Block) -> usize`.
- Produces: alias-cleaned function bodies before `LocalDeclarer` and naming.

- [ ] **Step 1: Insert the pass at the post-restructure boundary**

In `decompile_function`, replace:

```rust
let block = Arc::new(restructure::lift(function).into());
```

with:

```rust
let mut block: ast::Block = restructure::lift(function).into();
ast::eliminate_aliases(&mut block);
let block = Arc::new(block);
```

Keep the pass before `LocalDeclarer::declare_locals`.

- [ ] **Step 2: Run the original RED regression**

Run:

```powershell
cargo +nightly test -p luau-lifter wonky_v12_output_has_no_trivial_local_aliases -- --nocapture
```

Expected: PASS. The output must call `setmetatable` with the captured module table directly and contain no trivial local alias line.

- [ ] **Step 3: Run compatibility and malformed-input tests**

Run:

```powershell
cargo +nightly test -p luau-lifter -- --nocapture
```

Expected: all tests PASS, including V9-V12 compiler round trips and V4-V8 format fixtures.

- [ ] **Step 4: Commit integration**

```powershell
git add luau-lifter/src/lib.rs luau-lifter/src/compatibility_tests.rs
git commit -m "feat: remove post-SSA Luau aliases"
```

---

### Task 6: Report alias quality statically

**Files:**
- Modify: `tools/luau_corpus/model.py`
- Modify: `tools/luau_corpus/process.py`
- Modify: `tools/luau_corpus/report.py`
- Modify: `tests/python/test_luau_corpus.py`

**Interfaces:**
- Produces: `generated_aliases: int` on `CaseResult` and `count_trivial_aliases(source: str) -> int`.
- Consumes: generated Luau text only for diagnostics; production rewriting remains AST-based.

- [ ] **Step 1: Add failing metric tests**

Add:

```python
from tools.luau_corpus.process import count_trivial_aliases

def test_trivial_alias_metric_counts_identifier_copies(self) -> None:
    source = """\
local copy = original
local value = call()
table.slot = original
return copy
"""
    self.assertEqual(count_trivial_aliases(source), 1)
```

In the existing corpus-runner report test, add:

```python
self.assertEqual(payload["cases"][0]["generated_aliases"], 0)
self.assertIn(
    "| profile | case | version | compile | decompile | recompile | statements | locals | aliases | gotos |",
    markdown,
)
self.assertIn(
    "| test | 01_success | 66 | 0 | 0 | 0 | 2 | 1 | 0 | 0 |",
    markdown,
)
```

Replace the old header and row assertions so the test checks only the new
schema.

- [ ] **Step 2: Run Python tests and verify RED**

Run:

```powershell
python -m unittest discover -s tests/python -v
```

Expected: import or assertion FAIL because the metric does not exist.

- [ ] **Step 3: Implement the diagnostic metric**

In `process.py`, add:

```python
def _is_identifier(value: str) -> bool:
    return value.isidentifier()


def count_trivial_aliases(source: str) -> int:
    count = 0
    for line in source.splitlines():
        stripped = line.strip()
        if not stripped.startswith("local "):
            continue
        assignment = stripped.removeprefix("local ").split(" = ", maxsplit=1)
        if len(assignment) == 2 and all(_is_identifier(part) for part in assignment):
            count += 1
    return count
```

Add `generated_aliases: int` to `CaseResult`, populate it from successful
decompiler output, serialize it in `_case_payload`, and add it to the Markdown
table beside `generated_locals`.

- [ ] **Step 4: Run Python tests**

Run:

```powershell
python -m unittest discover -s tests/python -v
```

Expected: all tests PASS.

- [ ] **Step 5: Commit reporting**

```powershell
git add tools/luau_corpus tests/python/test_luau_corpus.py
git commit -m "feat: report generated Luau aliases"
```

---

### Task 7: Full static verification and PR

**Files:**
- Modify: `docs/decompiler-baseline-findings.md`
- Generated but ignored: `tests/luau_corpus/results/alias-elimination/`

**Interfaces:**
- Consumes: completed Section 1 implementation.
- Produces: verified branch and draft PR targeting `main`.

- [ ] **Step 1: Run formatting and workspace tests**

Run:

```powershell
cargo fmt --all -- --check
$env:RUSTFLAGS = "-Awarnings"
cargo +nightly test --workspace
python -m unittest discover -s tests/python -v
```

Expected: all commands PASS.

- [ ] **Step 2: Run the complete static corpus**

Run:

```powershell
python tools/run_luau_corpus.py --profiles all --output tests/luau_corpus/results/alias-elimination --no-build
```

Expected: 240 cases, zero compile failures, zero decompile failures, zero
recompile failures, and zero generated gotos. Do not invoke `luau.exe`.

- [ ] **Step 3: Compare representative output**

Inspect:

```text
tests/luau_corpus/results/alias-elimination/V12/24_wonky_integration.luau
tests/luau_corpus/results/alias-elimination/V12/23_register_pressure_aliases.luau
tests/luau_corpus/results/alias-elimination/O0_g1/24_wonky_integration.luau
```

Confirm the `Machine.new` alias is absent and record before/after alias and local
counts in `docs/decompiler-baseline-findings.md`.

- [ ] **Step 4: Run final diff checks**

Run:

```powershell
git diff --check
git status -sb
```

Expected: only intentional Section 1 files are modified; generated results and
`.tools` remain ignored.

- [ ] **Step 5: Commit documentation**

```powershell
git add docs/decompiler-baseline-findings.md docs/superpowers/plans/2026-07-27-luau-alias-copy-elimination.md
git commit -m "docs: record alias elimination results"
```

- [ ] **Step 6: Push and open a draft PR**

Create `.github/pr-body-alias-elimination.md` with `apply_patch`:

```markdown
## What changed

- removes statically proven single-use local aliases after SSA destruction
- collapses safe copy chains recursively through structured blocks
- preserves aliases across source writes, effectful evaluation prefixes, and reference captures
- reports generated alias counts in the static Luau corpus

## Verification

- AST, lifter, and workspace Rust tests: 30 passed
- Python corpus tests: 10 passed
- V4-V12 static round trips: 240/240
- representative alias count: 1 -> 0
- arbitrary scripts executed: no
```

These are the expected totals after the tests in this plan are added. If the
actual collected totals differ because the branch gains or loses tests, update
the body to the measured values before creating the PR. Then run:

```powershell
git push -u origin agent/alias-copy-elimination
gh pr create --repo Babyhamsta/medal-decompiler --base main --head agent/alias-copy-elimination --draft --title "Reduce redundant aliases in decompiled Luau" --body-file .github/pr-body-alias-elimination.md
```

Delete `.github/pr-body-alias-elimination.md` with `apply_patch` after PR
creation so it is never committed.

- [ ] **Step 7: Stop at the approval gate**

Report the PR URL and validation evidence. Do not mark ready or merge until the
user explicitly approves that PR.
