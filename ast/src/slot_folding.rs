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
//! | 2 | `T`'s binding is a table literal | [`table_bindings`], filtered in [`foldable_tables`] |
//! | 3 | write and read straight-line in one block | [`find_fold`] scans one block; [`blocks_window`] stops at structure |
//! | 4 | no intervening call or side effect | [`blocks_window`] |
//! | 5 | no computed-key write to `T` anywhere | [`SlotUses::computed_key_writes`] |
//! | 6 | `setmetatable` never applied to `T` | [`SlotUses::metatable_targets`] |
//! | 7 | the write is dead after the read | [`SlotUses::opaque`] plus [`slot_dead_after`] |

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

/// Whether the statement writes `table[key]`, redefining the slot.
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
    /// Precondition 5. Subsumed by `opaque` — a computed key is a
    /// non-constant index, which is an opaque use — and kept anyway, because
    /// a redundant check costs nothing and being wrong here is silent.
    computed_key_writes: FxHashSet<RcLocal>,
    /// Precondition 6. Also subsumed by `opaque`, for the same reason.
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
        if slot_key(&index.right).is_none() {
            // Precondition 5.
            uses.computed_key_writes.insert(table.clone());
            uses.opaque.insert(table.clone());
        }
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
            !uses.opaque.contains(table)
                && !uses.computed_key_writes.contains(table)
                && !uses.metatable_targets.contains(table)
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
    // Moving a value that reads the same table past the statements between
    // the write and the read would re-read it at a different point in the
    // sequence.
    if value.values_read().into_iter().any(|local| *local == table) {
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

fn apply_fold(block: &mut Block, fold: &Fold) -> bool {
    let mut replaced = false;
    block.0[fold.read].traverse_rvalues(&mut |rvalue| {
        if replaced {
            return;
        }
        if let RValue::Index(index) = rvalue
            && is_slot(index, &fold.table, &fold.key)
        {
            *rvalue = fold.value.clone();
            replaced = true;
        }
    });
    if !replaced {
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
            if !apply_fold(block, &fold) {
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

    /// Precondition 5: a computed key anywhere could collide with a constant
    /// one. The write here is after the read, so only the whole-function
    /// check can reject it.
    #[test]
    fn keeps_the_write_when_a_computed_key_is_written_anywhere() {
        let mut block = fixture_chain_with_computed_key_write();
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
