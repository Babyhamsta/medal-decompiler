//! Folds a table used as a register array back into expressions.
//!
//! Obfuscated output routes almost every value through numbered slots of a
//! scratch table. Reading it means tracking those slots by hand. This pass
//! substitutes a slot's value into its single read when doing so cannot
//! change what the program does.
//!
//! The preconditions in `docs/superpowers/specs/2026-07-30-decompile-
//! readability-design.md` are the contract. They are conditions, not
//! heuristics: any one of them failing abandons the fold entirely.
//!
//! Where each precondition lives:
//!
//! | # | Condition | Enforced by |
//! | --- | --- | --- |
//! | 1 | `T` local, `K` constant literal | [`slot_write`] and [`slot_key`] |
//! | 2 | `T`'s binding is a table literal | [`collect_table_bindings`], filtered in [`foldable_tables`] |
//! | 3 | write and read straight-line in one block | [`find_fold`] scans one block; [`is_structured`] stops it |
//! | 4 | no intervening call or side effect | [`blocks_window`] |
//! | 5 | *removed* — see below | — |
//! | 6 | `setmetatable` never applied to `T` | [`SlotUses::metatable_targets`] |
//! | 7 | the write is dead after the read | [`SlotUses::opaque`] plus [`slot_dead_after`] |
//!
//! # Why precondition 5 is gone
//!
//! A computed-key write to `T` does not disqualify the table. Requiring every
//! write to use a constant key would be a cheap guard, but it rejects the
//! majority of real candidates: obfuscated output routinely opens a function by
//! copying varargs into its register table through a loop index, which makes
//! every such table ineligible for no benefit. The guard is also redundant —
//! the preconditions below already rule out the folds it would have blocked.
//!
//! The argument, restated for the pass as it now stands. `T` never escapes
//! ([`SlotUses::opaque`]), and every read of `T` uses a constant key, so the
//! only things that can touch `T[K]` are the `T[constant]` expressions in
//! this function plus computed-key *writes*. Take a fold of `T[K] = E` at
//! `w` into a read at `r`. A computed-key write sits somewhere, and there
//! are only four places it can sit:
//!
//! - **Before `w`.** Whatever it left in `T[K]` is overwritten by `w` before
//!   anything in the window reads the slot. On a loop back-edge it is still
//!   ordered before `w` within the iteration, so a read positioned ahead of
//!   `w` sees the same value with or without the fold.
//! - **Between `w` and `r`, not carrying the read.** It is an assignment
//!   through an index, so [`blocks_window`] ends the window and no fold
//!   happens. This is the case precondition 5 was protecting, and
//!   precondition 4 already covers it.
//! - **Carrying the read**, as in `T[i] = T[K]`. The read is taken from the
//!   blocking statement, then the scan stops. The statement may clobber
//!   `T[K]` when `i == K`, so it does not count as redefining the slot —
//!   [`writes_slot`] only recognises a constant-key target. That forces
//!   [`slot_dead_after`] onto its second arm, which requires `T[K]` to have
//!   no other read in the function, so the divergence is unobservable.
//! - **After `r`.** It cannot change a value already read, and any later
//!   read of `T[K]` is what [`slot_dead_after`] is already checking.
//!
//! A computed-key *read* is not covered by any of this: an unknown key could
//! name the folded slot and make the read counts wrong. Those still mark the
//! table opaque.
//!
//! # Two coupled guards
//!
//! Both of these look independent and are not.
//!
//! [`writes_slot`] must keep answering `false` for a computed-key target. It
//! is what stops the third case above from folding. Its own doc comment says
//! so; this is the second place, because the coupling is easy to miss.
//!
//! The rule that a folded value may read a *sibling* slot depends on the
//! table never escaping, which in turn depends on a closure capture marking
//! the table opaque ([`scan_rvalue`]'s `RValue::Closure` arm). Without it a
//! captured table would still be foldable, and a closure could observe the
//! sibling slot between the write and the read. Relaxing the value rule while
//! dropping the capture check would be unsound; they were introduced one
//! commit apart and have to stay together.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    Block, Call, Index, LValue, Literal, LocalRw, RValue, RcLocal, Select, SideEffects, Statement,
    Traverse,
};

/// How far forward a fold looks for its read.
///
/// The cap keeps the scan linear in block length. Folds beyond this distance
/// are rare and not worth a quadratic worst case in a phase measured in
/// milliseconds.
const SLOT_SCAN_LIMIT: usize = 64;

/// A constant slot key. Only these can be matched between a write and a read.
///
/// Integral numbers collapse onto `Integer` so that a write of `t[1]` and a
/// read of `t[1]` name the same slot regardless of how the literal was
/// spelled. Every other literal shape — non-integral, infinite, `NaN`, the
/// `i`-suffixed integer literal, booleans, `nil` — is deliberately absent, so
/// [`slot_key`] reports it as non-constant and the table is dropped rather
/// than folded under a key whose runtime identity is not obvious.
#[derive(PartialEq, Eq, Hash, Clone, Debug)]
enum SlotKey {
    Integer(i64),
    Name(Vec<u8>),
}

/// The largest magnitude at which `f64` still counts integers exactly.
const EXACT_INTEGER_LIMIT: f64 = 9_007_199_254_740_992.0;

fn slot_key(value: &RValue) -> Option<SlotKey> {
    match value {
        RValue::Literal(Literal::Number(key))
            if key.is_finite() && key.fract() == 0.0 && key.abs() < EXACT_INTEGER_LIMIT =>
        {
            Some(SlotKey::Integer(*key as i64))
        }
        RValue::Literal(Literal::String(key)) => Some(SlotKey::Name(key.clone())),
        _ => None,
    }
}

/// Whether `index` reads or writes `table[key]` exactly.
fn is_slot(index: &Index, table: &RcLocal, key: &SlotKey) -> bool {
    matches!(index.left.as_ref(), RValue::Local(local) if local == table)
        && slot_key(&index.right).as_ref() == Some(key)
}

/// Splits an assignment into `(table, key, value)` when it writes one
/// constant slot of one local table. Precondition 1.
fn slot_write(statement: &Statement) -> Option<(RcLocal, SlotKey, &RValue)> {
    let assign = statement.as_assign()?;
    if assign.prefix || assign.parallel || assign.left.len() != 1 || assign.right.len() != 1 {
        return None;
    }
    let LValue::Index(index) = &assign.left[0] else {
        return None;
    };
    let RValue::Local(table) = index.left.as_ref() else {
        return None;
    };
    Some((table.clone(), slot_key(&index.right)?, &assign.right[0]))
}

/// Whether the statement provably writes `table[key]`, redefining the slot.
///
/// **Do not broaden this to accept a computed key.** It looks like an
/// improvement — `T[i]` may well write `T[K]` — and it is not one. Removing
/// precondition 5 left this function carrying the soundness of the case where
/// the read is taken from a statement that also writes the table through a
/// computed key:
///
/// ```lua
/// T[1] = source
/// T[i] = T[1]     -- carries the read; may or may not write slot 1
/// sink = T[1]     -- reads `source` today, `nil` after a wrong fold
/// ```
///
/// Answering `true` for `T[i]` here lets [`slot_dead_after`] take its first
/// arm, which folds the write away and breaks the third statement. Answering
/// `false` forces the second arm, which demands the slot have no other read
/// and so rejects this program. `keeps_the_write_when_a_computed_key_carries_the_read`
/// fails if this is broadened.
fn writes_slot(statement: &Statement, table: &RcLocal, key: &SlotKey) -> bool {
    let Some(assign) = statement.as_assign() else {
        return false;
    };
    assign.left.iter().any(|lvalue| match lvalue {
        LValue::Index(index) => is_slot(index, table, key),
        _ => false,
    })
}

/// Runs `visit` over every block nested directly inside `statement`.
fn for_each_nested_block(statement: &Statement, visit: &mut impl FnMut(&Block)) {
    match statement {
        Statement::If(value) => {
            visit(&value.then_block.lock());
            visit(&value.else_block.lock());
        }
        Statement::While(value) => visit(&value.block.lock()),
        Statement::Repeat(value) => visit(&value.block.lock()),
        Statement::NumericFor(value) => visit(&value.block.lock()),
        Statement::GenericFor(value) => visit(&value.block.lock()),
        _ => {}
    }
}

/// Runs `visit` over every block nested directly inside `statement`, mutably.
fn for_each_nested_block_mut(statement: &mut Statement, visit: &mut impl FnMut(&mut Block)) {
    match statement {
        Statement::If(value) => {
            visit(&mut value.then_block.lock());
            visit(&mut value.else_block.lock());
        }
        Statement::While(value) => visit(&mut value.block.lock()),
        Statement::Repeat(value) => visit(&mut value.block.lock()),
        Statement::NumericFor(value) => visit(&mut value.block.lock()),
        Statement::GenericFor(value) => visit(&mut value.block.lock()),
        _ => {}
    }
}

/// Locals bound exactly once, by a table literal.
///
/// Precondition 2 asks that `T`'s binding in this function be a table
/// literal. `Assign::prefix` does not answer that here: declarations are
/// still unmarked at this phase, `local_declarations` runs later. What
/// answers it is the write set — a local written exactly once, by a table
/// literal, has no other provenance. A parameter or an upvalue is never
/// written by a table literal in the body, so both fall out of the set
/// without a separate test.
#[derive(Default)]
struct TableBindings {
    writes: FxHashMap<RcLocal, usize>,
    literal_bindings: FxHashSet<RcLocal>,
}

fn collect_table_bindings(block: &Block, bindings: &mut TableBindings) {
    for statement in &block.0 {
        for written in statement.values_written() {
            *bindings.writes.entry(written.clone()).or_default() += 1;
        }

        if let Statement::Assign(assign) = statement
            && !assign.parallel
            && let ([LValue::Local(target)], [RValue::Table(_)]) =
                (assign.left.as_slice(), assign.right.as_slice())
        {
            bindings.literal_bindings.insert(target.clone());
        }

        for_each_nested_block(statement, &mut |nested| {
            collect_table_bindings(nested, bindings)
        });
    }
}

/// Everything the fold needs to know about how a candidate table is used.
#[derive(Default)]
struct SlotUses {
    /// Tables used in any way other than as the base of `T[constant]`.
    ///
    /// This is what makes precondition 7 checkable. A table that never
    /// escapes cannot be read or written by anything except the explicit
    /// `T[constant]` expressions in this function, so "the write is dead
    /// afterwards" becomes a question about those expressions alone. It also
    /// closes the hole the spec flags under precondition 6: `setmetatable`
    /// cannot be applied to a table that is never named as a value.
    opaque: FxHashSet<RcLocal>,
    /// Precondition 6. Subsumed by `opaque` — `setmetatable(T, …)` has to
    /// name `T` as a bare value — and kept anyway, because a redundant check
    /// costs nothing and being wrong here is silent.
    metatable_targets: FxHashSet<RcLocal>,
    /// How often each slot is read across the whole function.
    reads: FxHashMap<(RcLocal, SlotKey), usize>,
}

impl SlotUses {
    fn record_read(&mut self, table: &RcLocal, key: SlotKey) {
        *self.reads.entry((table.clone(), key)).or_default() += 1;
    }
}

fn is_setmetatable(callee: &RValue) -> bool {
    match callee {
        RValue::Global(global) => global.name() == b"setmetatable",
        RValue::Index(index) => matches!(
            index.right.as_ref(),
            RValue::Literal(Literal::String(name)) if name.as_slice() == b"setmetatable"
        ),
        _ => false,
    }
}

/// Precondition 6: any local handed to `setmetatable` loses its known
/// metatable, and `__index`/`__newindex` could intercept every slot.
fn reject_metatable_targets(call: &Call, candidates: &FxHashSet<RcLocal>, uses: &mut SlotUses) {
    if !is_setmetatable(&call.value) {
        return;
    }
    for argument in &call.arguments {
        if let RValue::Local(local) = argument
            && candidates.contains(local)
        {
            uses.metatable_targets.insert(local.clone());
        }
    }
}

fn scan_rvalue(value: &RValue, candidates: &FxHashSet<RcLocal>, uses: &mut SlotUses) {
    match value {
        RValue::Local(local) => {
            if candidates.contains(local) {
                uses.opaque.insert(local.clone());
            }
            return;
        }
        RValue::Index(index) => {
            if let RValue::Local(table) = index.left.as_ref()
                && candidates.contains(table)
            {
                match slot_key(&index.right) {
                    Some(key) => uses.record_read(table, key),
                    None => {
                        uses.opaque.insert(table.clone());
                    }
                }
                // The key may still name other tables.
                scan_rvalue(&index.right, candidates, uses);
                return;
            }
        }
        RValue::Closure(closure) => {
            // A closure body is a separate function this pass never walks, so
            // a captured table's slots could be read or written out of sight.
            // `Upvalue::Copy` shares the table object just as `Ref` does —
            // only the binding differs — so both forms escape.
            for captured in closure.values_read() {
                if candidates.contains(captured) {
                    uses.opaque.insert(captured.clone());
                }
            }
            return;
        }
        RValue::Call(call) | RValue::Select(Select::Call(call)) => {
            reject_metatable_targets(call, candidates, uses);
        }
        _ => {}
    }

    for child in value.rvalues() {
        scan_rvalue(child, candidates, uses);
    }
}

fn scan_lvalue(lvalue: &LValue, candidates: &FxHashSet<RcLocal>, uses: &mut SlotUses) {
    let LValue::Index(index) = lvalue else {
        return;
    };
    if let RValue::Local(table) = index.left.as_ref()
        && candidates.contains(table)
    {
        // A computed-key *write* does not disqualify the table. It cannot
        // reach a fold: one between the write and the read blocks the window
        // under precondition 4, one before the write is overwritten by it,
        // and one after the read cannot change a value already read. See the
        // module comment.
        //
        // A computed-key *read* is a different matter and falls through to
        // `scan_rvalue`, which marks the table opaque — an unknown key could
        // name the folded slot, which would make the read counts wrong.
        scan_rvalue(&index.right, candidates, uses);
        return;
    }
    scan_rvalue(&index.left, candidates, uses);
    scan_rvalue(&index.right, candidates, uses);
}

/// Marks every candidate the statement touches as opaque.
///
/// Used for statement kinds whose reads this pass does not model exactly.
/// Guessing wrong about them would silently lose a use; refusing to fold the
/// tables they mention costs nothing but folds.
fn reject_all_mentioned(statement: &Statement, candidates: &FxHashSet<RcLocal>, uses: &mut SlotUses) {
    for local in statement.values_read().into_iter().chain(statement.values_written()) {
        if candidates.contains(local) {
            uses.opaque.insert(local.clone());
        }
    }
}

fn collect_slot_uses(block: &Block, candidates: &FxHashSet<RcLocal>, uses: &mut SlotUses) {
    for statement in &block.0 {
        match statement {
            // Kinds whose every local read is reachable from `rvalues` and,
            // for `Assign`, the left-hand side.
            Statement::Assign(assign) => {
                for lvalue in &assign.left {
                    scan_lvalue(lvalue, candidates, uses);
                }
                for value in &assign.right {
                    scan_rvalue(value, candidates, uses);
                }
            }
            Statement::Call(call) => {
                reject_metatable_targets(call, candidates, uses);
                for value in statement.rvalues() {
                    scan_rvalue(value, candidates, uses);
                }
            }
            Statement::MethodCall(_)
            | Statement::Return(_)
            | Statement::If(_)
            | Statement::While(_)
            | Statement::Repeat(_)
            | Statement::NumericFor(_)
            | Statement::GenericFor(_)
            | Statement::Empty(_)
            | Statement::Comment(_)
            | Statement::Break(_)
            | Statement::Continue(_)
            | Statement::Goto(_)
            | Statement::Label(_) => {
                for value in statement.rvalues() {
                    scan_rvalue(value, candidates, uses);
                }
            }
            _ => reject_all_mentioned(statement, candidates, uses),
        }

        for_each_nested_block(statement, &mut |nested| {
            collect_slot_uses(nested, candidates, uses)
        });
    }
}

/// Locals that are provably plain tables created here, with every use
/// accounted for.
fn foldable_tables(block: &Block, protected: &FxHashSet<RcLocal>) -> (FxHashSet<RcLocal>, SlotUses) {
    let mut bindings = TableBindings::default();
    collect_table_bindings(block, &mut bindings);

    let candidates = bindings
        .literal_bindings
        .iter()
        .filter(|table| {
            !protected.contains(*table) && bindings.writes.get(*table).copied() == Some(1)
        })
        .cloned()
        .collect::<FxHashSet<_>>();

    if candidates.is_empty() {
        return (candidates, SlotUses::default());
    }

    let mut uses = SlotUses::default();
    collect_slot_uses(block, &candidates, &mut uses);

    let foldable = candidates
        .into_iter()
        .filter(|table| {
            !uses.opaque.contains(table) && !uses.metatable_targets.contains(table)
        })
        .collect();
    (foldable, uses)
}

/// Whether a statement could observe a table slot written before it, or
/// change the value being moved across it.
///
/// Any call may run a closure that captured something the value reads, and
/// any write through an index may target a table the value reads. Both end
/// the window without needing to prove that either actually happened. The
/// shape is a whitelist rather than a blacklist so that a statement kind
/// added later blocks by default instead of leaking through.
fn blocks_window(statement: &Statement) -> bool {
    match statement {
        Statement::Empty(_) | Statement::Comment(_) => false,
        Statement::Assign(assign) => {
            assign
                .left
                .iter()
                .any(|lvalue| !matches!(lvalue, LValue::Local(_)))
                || assign.has_side_effects()
        }
        _ => true,
    }
}

fn is_structured(statement: &Statement) -> bool {
    matches!(
        statement,
        Statement::If(_)
            | Statement::While(_)
            | Statement::Repeat(_)
            | Statement::NumericFor(_)
            | Statement::GenericFor(_)
    )
}

fn count_slot_reads_rvalue(value: &RValue, table: &RcLocal, key: &SlotKey) -> usize {
    if let RValue::Index(index) = value
        && is_slot(index, table, key)
    {
        return 1;
    }
    value
        .rvalues()
        .into_iter()
        .map(|child| count_slot_reads_rvalue(child, table, key))
        .sum()
}

/// Reads of `table[key]` in one statement, not counting nested blocks.
///
/// Nested blocks never matter here: [`find_fold`] stops at a structured
/// statement before it counts, so no statement this sees has one.
fn count_slot_reads(statement: &Statement, table: &RcLocal, key: &SlotKey) -> usize {
    let mut count = 0;
    if let Statement::Assign(assign) = statement {
        for lvalue in &assign.left {
            let LValue::Index(index) = lvalue else {
                continue;
            };
            if is_slot(index, table, key) {
                // Writing the slot is not reading it.
                continue;
            }
            count += count_slot_reads_rvalue(&index.left, table, key);
            count += count_slot_reads_rvalue(&index.right, table, key);
        }
        for value in &assign.right {
            count += count_slot_reads_rvalue(value, table, key);
        }
        return count;
    }
    for value in statement.rvalues() {
        count += count_slot_reads_rvalue(value, table, key);
    }
    count
}

/// Precondition 7: the value written at the fold's write must not be
/// observable after its read.
///
/// The candidate table never escapes, so the only things that can observe
/// `table[key]` are the `table[key]` expressions in this function. Two
/// situations settle it without a liveness pass:
///
/// - the read statement writes the slot itself, so the folded value is
///   overwritten in the same step that consumed it, and nothing downstream
///   — including a later loop iteration — can reach the old value;
/// - the read is the slot's only read in the whole function, so after the
///   substitution nothing reads it at all.
///
/// Anything else abandons the fold.
fn slot_dead_after(
    read_statement: &Statement,
    table: &RcLocal,
    key: &SlotKey,
    uses: &SlotUses,
) -> bool {
    if writes_slot(read_statement, table, key) {
        return true;
    }
    uses.reads.get(&(table.clone(), key.clone())).copied() == Some(1)
}

struct Fold {
    write: usize,
    read: usize,
    table: RcLocal,
    key: SlotKey,
    value: RValue,
}

fn find_fold(block: &Block, start: usize, foldable: &FxHashSet<RcLocal>, uses: &SlotUses) -> Option<Fold> {
    let (table, key, value) = slot_write(&block.0[start])?;
    if !foldable.contains(&table) {
        return None;
    }
    // A value that reads the slot it is being written to cannot move: after
    // the write is removed, that read would see whatever the slot held
    // before, not the value the fold is carrying.
    //
    // Reading a *different* slot of the same table is fine. Only an `Empty`,
    // a `Comment`, or an assignment to plain locals with a side-effect-free
    // value may sit between the write and the read, and none of those can
    // write a table, so every other slot holds the same value at the read as
    // it did at the write. When the read statement is itself an assignment,
    // Luau evaluates all of its expressions before performing any store, so
    // the moved value still observes pre-statement slots.
    //
    // The table is never opaque and never indexed by a computed key when
    // read, so every read of it inside the value names one constant slot and
    // this check sees all of them.
    if count_slot_reads_rvalue(value, &table, &key) > 0 {
        return None;
    }

    let effectful = value.has_side_effects();
    let limit = (start + SLOT_SCAN_LIMIT).min(block.0.len().saturating_sub(1));
    let mut read_at = None;
    for index in (start + 1)..=limit {
        let statement = &block.0[index];
        // Precondition 3: a branch, loop, or label boundary ends the window,
        // and the read is never taken from inside one.
        if is_structured(statement) {
            break;
        }
        // The read may be the very statement that also blocks the window, so
        // count before deciding to stop.
        let reads = count_slot_reads(statement, &table, &key);
        if reads > 1 {
            return None;
        }
        if reads == 1 {
            read_at = Some(index);
            break;
        }
        // Precondition 4.
        if blocks_window(statement) {
            break;
        }
        // Moving an effectful value past a statement that touches any local
        // changes what that value observes.
        if effectful && !statement.values().is_empty() {
            return None;
        }
    }

    let read = read_at?;
    // The value is evaluated at the read instead of here, so every local it
    // reads has to still hold the same thing there. `blocks_window` lets a
    // plain local assignment through, and one of those can overwrite exactly
    // such a local.
    let sources = value.values_read();
    if !sources.is_empty()
        && block.0[start + 1..=read].iter().any(|statement| {
            statement
                .values_written()
                .into_iter()
                .any(|written| sources.contains(&written))
        })
    {
        return None;
    }
    if !slot_dead_after(&block.0[read], &table, &key, uses) {
        return None;
    }

    Some(Fold {
        write: start,
        read,
        table,
        key,
        value: value.clone(),
    })
}

/// Whether evaluating this expression is something the program can observe.
///
/// Two shapes are exempt. A compiler-import global is a constant lookup the
/// compiler proved safe. And a constant-key read of a *foldable* table is a
/// plain memory read: such a table provably has no metatable and never
/// escapes, so indexing it cannot run `__index` and nothing else can be
/// holding a reference that would notice the order. `Index::has_side_effects`
/// answers `true` for every index because in general it can invoke a
/// metamethod; here we know better, and treating these as effects would stop
/// the register-array chains this pass exists to fold.
fn has_observable_effect(value: &RValue, foldable: &FxHashSet<RcLocal>) -> bool {
    if let RValue::Index(index) = value
        && matches!(index.left.as_ref(), RValue::Local(table) if foldable.contains(table))
        && slot_key(&index.right).is_some()
    {
        return false;
    }
    value.has_side_effects()
        && !matches!(
            value,
            RValue::Global(global) if global.origin() == crate::GlobalOrigin::CompilerImport
        )
}

/// Whether moving this value later in the evaluation order can be observed.
///
/// A value that neither causes an effect nor reads a local evaluates to the
/// same thing wherever it lands. Anything else has to keep its position
/// relative to the effects around it.
fn is_order_sensitive(value: &RValue) -> bool {
    value.has_side_effects() || !value.values_read().is_empty()
}

/// Replaces the slot read with the folded value, walking in evaluation order.
///
/// Returns `false` when the value would have to cross an observable effect to
/// reach its read. The fold moves the value from its own statement into the
/// middle of a later one, so anything the read statement evaluates first
/// would end up running before it — reordering the two.
fn place_value(
    target: &mut RValue,
    fold: &Fold,
    foldable: &FxHashSet<RcLocal>,
    guarded: bool,
    crossed: &mut bool,
) -> Option<bool> {
    if let RValue::Index(index) = target
        && is_slot(index, &fold.table, &fold.key)
    {
        if guarded && *crossed {
            return Some(false);
        }
        *target = fold.value.clone();
        return Some(true);
    }

    match target {
        // The right operand of a short circuit, and both arms of a
        // conditional, may not be evaluated at all. A value that landed there
        // could have its effect skipped entirely.
        RValue::Binary(binary)
            if matches!(
                binary.operation,
                crate::BinaryOperation::And | crate::BinaryOperation::Or
            ) =>
        {
            if let Some(placed) = place_value(&mut binary.left, fold, foldable, guarded, crossed) {
                return Some(placed);
            }
            *crossed = true;
            return place_value(&mut binary.right, fold, foldable, guarded, crossed);
        }
        RValue::Conditional(conditional) => {
            if let Some(placed) = place_value(&mut conditional.condition, fold, foldable, guarded, crossed) {
                return Some(placed);
            }
            *crossed = true;
            if let Some(placed) = place_value(&mut conditional.then_value, fold, foldable, guarded, crossed) {
                return Some(placed);
            }
            return place_value(&mut conditional.else_value, fold, foldable, guarded, crossed);
        }
        // The method lookup runs between the object and the arguments, and it
        // can invoke `__index`.
        RValue::MethodCall(method_call) | RValue::Select(Select::MethodCall(method_call)) => {
            if let Some(placed) = place_value(&mut method_call.value, fold, foldable, guarded, crossed) {
                return Some(placed);
            }
            *crossed = true;
            for argument in &mut method_call.arguments {
                if let Some(placed) = place_value(argument, fold, foldable, guarded, crossed) {
                    return Some(placed);
                }
            }
            return None;
        }
        _ => {}
    }

    for child in target.rvalues_mut() {
        if let Some(placed) = place_value(child, fold, foldable, guarded, crossed) {
            return Some(placed);
        }
    }
    // The node's own effect happens after its children are evaluated.
    if has_observable_effect(target, foldable) {
        *crossed = true;
    }
    None
}

fn apply_fold(block: &mut Block, fold: &Fold, foldable: &FxHashSet<RcLocal>) -> bool {
    let guarded = is_order_sensitive(&fold.value);
    let mut crossed = false;
    let statement = &mut block.0[fold.read];

    let placed = if let Statement::Assign(assign) = statement {
        // Luau does not specify whether an assignment evaluates its targets'
        // subexpressions before or after its values. Refuse to place a value
        // whose position matters into a target, and treat any effect in a
        // target as already crossed when placing into a value.
        let in_target = assign.left.iter().any(|lvalue| match lvalue {
            LValue::Index(index) => {
                !is_slot(index, &fold.table, &fold.key)
                    && (count_slot_reads_rvalue(&index.left, &fold.table, &fold.key) > 0
                        || count_slot_reads_rvalue(&index.right, &fold.table, &fold.key) > 0)
            }
            _ => false,
        });
        if in_target && guarded {
            return false;
        }
        for lvalue in &assign.left {
            if let LValue::Index(index) = lvalue
                && (has_observable_effect(&index.left, foldable)
                    || has_observable_effect(&index.right, foldable))
            {
                crossed = true;
            }
        }
        let mut placed = None;
        if in_target {
            for lvalue in &mut assign.left {
                if let LValue::Index(index) = lvalue {
                    if let Some(result) = place_value(&mut index.left, fold, foldable, guarded, &mut crossed)
                        .or_else(|| {
                            place_value(&mut index.right, fold, foldable, guarded, &mut crossed)
                        })
                    {
                        placed = Some(result);
                        break;
                    }
                }
            }
        } else {
            for value in &mut assign.right {
                if let Some(result) = place_value(value, fold, foldable, guarded, &mut crossed) {
                    placed = Some(result);
                    break;
                }
            }
        }
        placed
    } else {
        let mut placed = None;
        for value in statement.rvalues_mut() {
            if let Some(result) = place_value(value, fold, foldable, guarded, &mut crossed) {
                placed = Some(result);
                break;
            }
        }
        placed
    };

    if placed != Some(true) {
        return false;
    }
    block.0.remove(fold.write);
    true
}

fn fold_block(block: &mut Block, foldable: &FxHashSet<RcLocal>, uses: &mut SlotUses) -> usize {
    let mut folds = 0;
    let mut index = block.0.len();
    // Backwards, retrying the same index after a fold: the statement that
    // takes the removed write's place is the merged read, which is often the
    // next link of the same chain.
    while index > 0 {
        index -= 1;
        while index < block.0.len()
            && let Some(fold) = find_fold(block, index, foldable, uses)
        {
            if !apply_fold(block, &fold, foldable) {
                break;
            }
            if let Some(count) = uses.reads.get_mut(&(fold.table.clone(), fold.key.clone())) {
                *count -= 1;
            }
            folds += 1;
        }
    }
    folds
}

fn fold_tree(block: &mut Block, foldable: &FxHashSet<RcLocal>, uses: &mut SlotUses) -> usize {
    let mut folds = 0;
    for statement in &mut block.0 {
        for_each_nested_block_mut(statement, &mut |nested| {
            folds += fold_tree(nested, foldable, uses);
        });
    }
    folds + fold_block(block, foldable, uses)
}

/// Folds constant table slots into their single read.
///
/// `protected` names locals this block cannot see every write to — incoming
/// upvalues and parameters. They are never folded.
pub fn fold_table_slots(block: &mut Block, protected: &[RcLocal]) -> usize {
    let mut protected = protected.iter().cloned().collect::<FxHashSet<_>>();
    crate::alias_elimination::collect_reference_captures(block, &mut protected);

    let (foldable, mut uses) = foldable_tables(block, &protected);
    if foldable.is_empty() {
        return 0;
    }
    fold_tree(block, &foldable, &mut uses)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Assign, Global, If, Local, Table};

    fn local(name: Option<&str>) -> RcLocal {
        RcLocal::new(Local::new(name.map(str::to_owned)))
    }

    fn slot(table: &RcLocal, key: f64) -> LValue {
        LValue::Index(Index::new(
            table.clone().into(),
            Literal::Number(key).into(),
        ))
    }

    fn slot_read(table: &RcLocal, key: f64) -> RValue {
        RValue::Index(Index::new(
            table.clone().into(),
            Literal::Number(key).into(),
        ))
    }

    fn declaration(table: &RcLocal) -> Statement {
        let mut assign = Assign::new(vec![table.clone().into()], vec![Table::default().into()]);
        assign.prefix = true;
        assign.into()
    }

    /// local registers = {}
    /// registers[1] = source
    /// registers[2] = registers[1]
    fn fixture_simple_chain() -> (RcLocal, RcLocal, Block) {
        let registers = local(Some("registers"));
        let source = local(Some("source"));
        let block = Block(vec![
            declaration(&registers),
            Assign::new(vec![slot(&registers, 1.0)], vec![source.clone().into()]).into(),
            Assign::new(
                vec![slot(&registers, 2.0)],
                vec![slot_read(&registers, 1.0)],
            )
            .into(),
        ]);
        (registers, source, block)
    }

    fn fixture_chain_with_intervening_call() -> Block {
        let (_, _, mut block) = fixture_simple_chain();
        block.0.insert(
            2,
            Call::new(local(Some("observe")).into(), Vec::new()).into(),
        );
        block
    }

    fn fixture_chain_on_parameter_table() -> Block {
        let (_, _, mut block) = fixture_simple_chain();
        block.0.remove(0);
        block
    }

    fn fixture_chain_with_computed_key_write() -> Block {
        let (registers, source, mut block) = fixture_simple_chain();
        let key = local(Some("index"));
        block.0.push(
            Assign::new(
                vec![LValue::Index(Index::new(
                    registers.into(),
                    key.into(),
                ))],
                vec![source.into()],
            )
            .into(),
        );
        block
    }

    fn fixture_chain_with_setmetatable() -> Block {
        let (registers, _, mut block) = fixture_simple_chain();
        block.0.insert(
            1,
            Call::new(
                Global::new(b"setmetatable".to_vec()).into(),
                vec![registers.into(), Table::default().into()],
            )
            .into(),
        );
        block
    }

    fn fixture_chain_with_intervening_index_write() -> Block {
        let (_, source, mut block) = fixture_simple_chain();
        let alias = local(Some("alias"));
        block.0.insert(
            2,
            Assign::new(vec![slot(&alias, 1.0)], vec![source.into()]).into(),
        );
        block
    }

    fn fixture_chain_with_two_reads() -> Block {
        let (registers, _, mut block) = fixture_simple_chain();
        let sink = local(Some("sink"));
        block
            .0
            .push(Assign::new(vec![sink.into()], vec![slot_read(&registers, 1.0)]).into());
        block
    }

    fn fixture_chain_across_an_if() -> Block {
        let (_, _, mut block) = fixture_simple_chain();
        let guarded = block.0.pop().unwrap();
        block.0.push(
            If::new(
                local(Some("flag")).into(),
                Block(vec![guarded]),
                Block::default(),
            )
            .into(),
        );
        block
    }

    /// The positive case: the write disappears and its value lands in the read.
    #[test]
    fn folds_a_constant_slot_into_its_only_read() {
        let (_, source, mut block) = fixture_simple_chain();

        assert_eq!(fold_table_slots(&mut block, &[]), 1);
        assert_eq!(block.0.len(), 2);
        let merged = block.0[1].as_assign().unwrap();
        assert_eq!(merged.right, vec![RValue::Local(source)]);
    }

    /// Precondition 4: a call between the write and the read could run a
    /// closure that observes the slot.
    #[test]
    fn keeps_the_write_when_a_call_intervenes() {
        let mut block = fixture_chain_with_intervening_call();
        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    /// Precondition 2: without a table-literal binding the provenance — and
    /// therefore the metatable — is unknown.
    #[test]
    fn keeps_the_write_when_the_table_is_a_parameter() {
        let mut block = fixture_chain_on_parameter_table();
        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    /// Precondition 5 removed: a computed-key write *after* the read cannot
    /// change a value already read, so it does not disqualify the table.
    #[test]
    fn folds_when_a_computed_key_is_written_after_the_read() {
        let mut block = fixture_chain_with_computed_key_write();
        assert_eq!(fold_table_slots(&mut block, &[]), 1);
    }

    /// What keeps the removal of precondition 5 sound: a computed-key write
    /// *between* the write and the read may collide with the folded slot, and
    /// `blocks_window` ends the window at it. Precondition 4 carries the
    /// weight precondition 5 used to.
    #[test]
    fn keeps_the_write_when_a_computed_key_write_intervenes() {
        let registers = local(Some("registers"));
        let source = local(Some("source"));
        let other = local(Some("other"));
        let key = local(Some("index"));
        let mut block = Block(vec![
            declaration(&registers),
            Assign::new(vec![slot(&registers, 1.0)], vec![source.into()]).into(),
            Assign::new(
                vec![LValue::Index(Index::new(
                    registers.clone().into(),
                    key.into(),
                ))],
                vec![other.into()],
            )
            .into(),
            Assign::new(
                vec![slot(&registers, 2.0)],
                vec![slot_read(&registers, 1.0)],
            )
            .into(),
        ]);

        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    /// A computed-key *read* still disqualifies the table: an unknown key
    /// could name the folded slot, which would make the read counts wrong.
    #[test]
    fn keeps_the_write_when_a_computed_key_is_read() {
        let registers = local(Some("registers"));
        let source = local(Some("source"));
        let sink = local(Some("sink"));
        let key = local(Some("index"));
        let mut block = Block(vec![
            declaration(&registers),
            Assign::new(vec![slot(&registers, 1.0)], vec![source.into()]).into(),
            Assign::new(
                vec![slot(&registers, 2.0)],
                vec![slot_read(&registers, 1.0)],
            )
            .into(),
            Assign::new(
                vec![sink.into()],
                vec![RValue::Index(Index::new(registers.into(), key.into()))],
            )
            .into(),
        ]);

        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    /// The read is taken from the statement that also ends the window. This
    /// is the capture's dominant shape — `value[1] = registers[1]` — where
    /// the read lives inside an index assignment.
    #[test]
    fn folds_into_a_blocking_index_assignment() {
        let registers = local(Some("registers"));
        let source = local(Some("source"));
        let destination = local(Some("destination"));
        let mut block = Block(vec![
            declaration(&registers),
            Assign::new(vec![slot(&registers, 1.0)], vec![source.clone().into()]).into(),
            Assign::new(
                vec![slot(&destination, 1.0)],
                vec![slot_read(&registers, 1.0)],
            )
            .into(),
        ]);

        assert_eq!(fold_table_slots(&mut block, &[]), 1);
        assert_eq!(block.0.len(), 2);
        let merged = block.0[1].as_assign().unwrap();
        assert_eq!(merged.right, vec![RValue::Local(source)]);
    }

    /// Pins the constant-key restriction in `writes_slot`. Broadening it to
    /// treat `T[i]` as a redefinition makes this fold, and the third
    /// statement then reads `nil`.
    #[test]
    fn keeps_the_write_when_a_computed_key_carries_the_read() {
        let registers = local(Some("registers"));
        let source = local(Some("source"));
        let sink = local(Some("sink"));
        let key = local(Some("index"));
        let mut block = Block(vec![
            declaration(&registers),
            Assign::new(vec![slot(&registers, 1.0)], vec![source.into()]).into(),
            Assign::new(
                vec![LValue::Index(Index::new(
                    registers.clone().into(),
                    key.into(),
                ))],
                vec![slot_read(&registers, 1.0)],
            )
            .into(),
            Assign::new(vec![sink.into()], vec![slot_read(&registers, 1.0)]).into(),
        ]);

        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    /// Finding 3: the folded value must not be reordered past an effect that
    /// the read statement evaluates before it.
    #[test]
    fn keeps_an_effectful_value_out_of_a_later_position() {
        let registers = local(Some("registers"));
        let sink = local(Some("sink"));
        let produce = Call::new(Global::new(b"produce".to_vec()).into(), Vec::new());
        let observe = Call::new(Global::new(b"observe".to_vec()).into(), Vec::new());
        let consumer = crate::Binary::new(
            observe.into(),
            slot_read(&registers, 1.0),
            crate::BinaryOperation::Add,
        );
        let mut block = Block(vec![
            declaration(&registers),
            Assign::new(vec![slot(&registers, 1.0)], vec![produce.into()]).into(),
            Assign::new(vec![sink.into()], vec![consumer.into()]).into(),
        ]);

        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    /// The mirror image: the read comes first in evaluation order, so the
    /// value keeps its position relative to the call and folds.
    #[test]
    fn folds_an_effectful_value_that_still_runs_first() {
        let registers = local(Some("registers"));
        let sink = local(Some("sink"));
        let produce = Call::new(Global::new(b"produce".to_vec()).into(), Vec::new());
        let observe = Call::new(Global::new(b"observe".to_vec()).into(), Vec::new());
        let consumer = crate::Binary::new(
            slot_read(&registers, 1.0),
            observe.into(),
            crate::BinaryOperation::Add,
        );
        let mut block = Block(vec![
            declaration(&registers),
            Assign::new(vec![slot(&registers, 1.0)], vec![produce.into()]).into(),
            Assign::new(vec![sink.into()], vec![consumer.into()]).into(),
        ]);

        assert_eq!(fold_table_slots(&mut block, &[]), 1);
    }

    /// A local the value reads is overwritten before the read. The window
    /// admits a plain local assignment, so nothing else catches this.
    #[test]
    fn keeps_the_write_when_the_values_source_is_overwritten() {
        let registers = local(Some("registers"));
        let source = local(Some("source"));
        let sink = local(Some("sink"));
        let mut block = Block(vec![
            declaration(&registers),
            Assign::new(vec![slot(&registers, 1.0)], vec![source.clone().into()]).into(),
            Assign::new(vec![source.into()], vec![Literal::Number(5.0).into()]).into(),
            Assign::new(vec![sink.into()], vec![slot_read(&registers, 1.0)]).into(),
        ]);

        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    /// A value may read a sibling slot: nothing that can sit between the
    /// write and the read is able to modify one. This is the shape corpus
    /// case `27_register_array_vm` is built from.
    #[test]
    fn folds_a_value_that_reads_another_slot() {
        let registers = local(Some("registers"));
        let source = local(Some("source"));
        let halved = crate::Binary::new(
            slot_read(&registers, 1.0),
            Literal::Number(2.0).into(),
            crate::BinaryOperation::Div,
        );
        let mut block = Block(vec![
            declaration(&registers),
            Assign::new(vec![slot(&registers, 1.0)], vec![source.clone().into()]).into(),
            Assign::new(vec![slot(&registers, 3.0)], vec![halved.into()]).into(),
            Assign::new(vec![slot(&registers, 3.0)], vec![slot_read(&registers, 3.0)]).into(),
        ]);

        // Two folds chain: `registers[3] = registers[1] / 2` moves into the
        // statement below it, which overwrites the same slot, and then
        // `registers[1] = source` moves into what is left.
        assert_eq!(fold_table_slots(&mut block, &[]), 2);
        assert_eq!(block.0.len(), 2);
        let merged = block.0[1].as_assign().unwrap();
        let RValue::Binary(binary) = &merged.right[0] else {
            panic!("expected the folded value in a binary operation");
        };
        assert_eq!(*binary.left, RValue::Local(source));
    }

    /// But not its own slot: after the write is removed that read would see
    /// whatever the slot held beforehand.
    #[test]
    fn keeps_the_write_when_the_value_reads_its_own_slot() {
        let registers = local(Some("registers"));
        let sink = local(Some("sink"));
        let increment = crate::Binary::new(
            slot_read(&registers, 1.0),
            Literal::Number(1.0).into(),
            crate::BinaryOperation::Add,
        );
        let mut block = Block(vec![
            declaration(&registers),
            Assign::new(vec![slot(&registers, 1.0)], vec![increment.into()]).into(),
            Assign::new(vec![sink.into()], vec![slot_read(&registers, 1.0)]).into(),
        ]);

        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    /// A closure captures the table by copy. The binding is copied but the
    /// table object is shared, so the closure body — which this pass never
    /// walks — can read the slot.
    #[test]
    fn keeps_the_write_when_the_table_is_captured_by_a_closure() {
        let registers = local(Some("registers"));
        let source = local(Some("source"));
        let holder = local(Some("holder"));
        let closure = crate::Closure {
            function: by_address::ByAddress(triomphe::Arc::new(parking_lot::Mutex::new(
                crate::Function::default(),
            ))),
            upvalues: vec![crate::Upvalue::Copy(registers.clone())],
        };
        let mut block = Block(vec![
            declaration(&registers),
            Assign::new(vec![slot(&registers, 1.0)], vec![source.into()]).into(),
            Assign::new(
                vec![slot(&registers, 2.0)],
                vec![slot_read(&registers, 1.0)],
            )
            .into(),
            Assign::new(vec![holder.into()], vec![closure.into()]).into(),
        ]);

        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    /// Precondition 6: `__index`/`__newindex` could intercept the slots.
    #[test]
    fn keeps_the_write_when_setmetatable_is_applied_to_the_table() {
        let mut block = fixture_chain_with_setmetatable();
        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    /// Precondition 4: a write through an index may target the same table
    /// under another name.
    #[test]
    fn keeps_the_write_when_an_index_assignment_intervenes() {
        let mut block = fixture_chain_with_intervening_index_write();
        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    /// Precondition 7: a second read still needs the slot to hold the value.
    #[test]
    fn keeps_the_write_when_the_slot_is_read_twice() {
        let mut block = fixture_chain_with_two_reads();
        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    /// Precondition 3: the read is behind a branch boundary.
    #[test]
    fn keeps_the_write_when_a_structured_statement_intervenes() {
        let mut block = fixture_chain_across_an_if();
        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    /// Precondition 3, isolated: the read sits in a branch condition, which
    /// `blocks_window` alone would let through because the condition is
    /// evaluated before the branch is taken.
    #[test]
    fn keeps_the_write_when_the_read_is_a_branch_condition() {
        let registers = local(Some("registers"));
        let source = local(Some("source"));
        let mut block = Block(vec![
            declaration(&registers),
            Assign::new(vec![slot(&registers, 1.0)], vec![source.into()]).into(),
            If::new(slot_read(&registers, 1.0), Block::default(), Block::default()).into(),
        ]);

        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    /// Precondition 2, isolated: the table is bound once, but by a call, so
    /// its metatable is unknown. The `fixture_chain_on_parameter_table` case
    /// has no binding at all and so never reaches this test.
    #[test]
    fn keeps_the_write_when_the_table_comes_from_a_call() {
        let registers = local(Some("registers"));
        let source = local(Some("source"));
        let produce = Call::new(Global::new(b"produce".to_vec()).into(), Vec::new());
        let mut block = Block(vec![
            Assign::new(vec![registers.clone().into()], vec![produce.into()]).into(),
            Assign::new(vec![slot(&registers, 1.0)], vec![source.into()]).into(),
            Assign::new(
                vec![slot(&registers, 2.0)],
                vec![slot_read(&registers, 1.0)],
            )
            .into(),
        ]);

        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    /// Precondition 7: the write is not dead just because its read was
    /// consumed. Here the table is handed to a call afterwards, which can
    /// read every slot, so removing the write would show that call a `nil`.
    #[test]
    fn keeps_the_write_when_the_table_escapes_after_the_read() {
        let registers = local(Some("registers"));
        let source = local(Some("source"));
        let observe = Call::new(
            Global::new(b"observe".to_vec()).into(),
            vec![registers.clone().into()],
        );
        let mut block = Block(vec![
            declaration(&registers),
            Assign::new(vec![slot(&registers, 1.0)], vec![source.into()]).into(),
            Assign::new(
                vec![slot(&registers, 2.0)],
                vec![slot_read(&registers, 1.0)],
            )
            .into(),
            observe.into(),
        ]);

        assert_eq!(fold_table_slots(&mut block, &[]), 0);
    }

    /// Precondition 7's other arm: the read overwrites the slot it reads, so
    /// the folded value dies in the same statement that consumed it. The slot
    /// is read twice more afterwards, so nothing but that redefinition can
    /// justify the fold.
    #[test]
    fn folds_into_a_read_that_overwrites_the_same_slot() {
        let registers = local(Some("registers"));
        let source = local(Some("source"));
        let sink = local(Some("sink"));
        let increment = crate::Binary::new(
            slot_read(&registers, 2.0),
            Literal::Number(1.0).into(),
            crate::BinaryOperation::Add,
        );
        let doubled = crate::Binary::new(
            slot_read(&registers, 2.0),
            slot_read(&registers, 2.0),
            crate::BinaryOperation::Add,
        );
        let mut block = Block(vec![
            declaration(&registers),
            Assign::new(vec![slot(&registers, 2.0)], vec![source.clone().into()]).into(),
            Assign::new(vec![slot(&registers, 2.0)], vec![increment.into()]).into(),
            Assign::new(vec![sink.into()], vec![doubled.into()]).into(),
        ]);

        assert_eq!(fold_table_slots(&mut block, &[]), 1);
        assert_eq!(block.0.len(), 3);
        let merged = block.0[1].as_assign().unwrap();
        let RValue::Binary(binary) = &merged.right[0] else {
            panic!("expected the folded value in a binary operation");
        };
        assert_eq!(*binary.left, RValue::Local(source));
    }
}
