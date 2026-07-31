use rustc_hash::{FxHashMap, FxHashSet};

use crate::{Block, Do, LocalRw, RValue, RcLocal, Statement, Traverse};

/// Declarations one function may hold in scope at once before its locals are
/// grouped into `do ... end` scopes.
///
/// Luau gives a local a register for as long as it is in scope and releases it
/// at block exit, with a ceiling of 200 per function. The count that matters is
/// the whole chain of open blocks, not any single one, so the budget is spent
/// as it is handed down. It sits below 200 to leave room for parameters and for
/// the temporaries individual statements need.
const FUNCTION_BUDGET: usize = 100;

/// Declarations a `do ... end` scope must hold to be worth emitting.
///
/// Grouping is only reachable in blocks that would not otherwise compile, but
/// even there a scope around one or two declarations adds a nesting level for
/// almost no register relief.
const MIN_SCOPE_DECLARATIONS: usize = 2;

/// The smallest budget a nested block is given.
///
/// Ancestors can consume the whole function budget between them. Handing their
/// children nothing would stop the descent from splitting anything further,
/// which is the opposite of what an already-crowded chain needs.
const MINIMUM_BUDGET: usize = 8;

/// Nesting levels grouping may add to a block.
///
/// Each level frees the registers of the runs below it, but also indents the
/// code another step. The cap keeps a pathological block from nesting once per
/// declaration when its locals genuinely all overlap.
const MAX_ADDED_DEPTH: usize = 16;

/// Groups runs of statements in over-budget blocks into `do ... end` scopes so
/// the locals they declare release their registers early.
///
/// A run is only eligible when every local it declares is unreferenced
/// afterwards, so the grouping is purely lexical: no statement moves, no
/// expression changes, and every reference keeps resolving to the same
/// declaration.
pub fn narrow_local_scopes(block: &mut Block) {
    narrow(block, FUNCTION_BUDGET, 0);
}

/// Groups this block's declarations, then hands what is left of the budget to
/// the blocks nested inside it.
///
/// `depth` counts only the scopes this pass has added on the way here, so a
/// block already wrapped several times stops being wrapped again.
fn narrow(block: &mut Block, budget: usize, depth: usize) {
    let held = if depth < MAX_ADDED_DEPTH {
        group_declarations(block, budget)
    } else {
        block.iter().map(|s| declared_locals(s).len()).sum()
    };
    // A block that has spent its whole budget still has to leave its own nested
    // blocks something to work with, or they stop splitting entirely.
    let remaining = budget.saturating_sub(held).max(MINIMUM_BUDGET);
    for statement in block.iter_mut() {
        narrow_nested(statement, remaining, depth);
    }
}

fn narrow_nested(statement: &mut Statement, budget: usize, depth: usize) {
    match statement {
        Statement::If(r#if) => {
            narrow(&mut r#if.then_block.lock(), budget, depth);
            narrow(&mut r#if.else_block.lock(), budget, depth);
        }
        // Every `do` block in the tree at this point is one this pass added.
        Statement::Do(r#do) => narrow(&mut r#do.block.lock(), budget, depth + 1),
        Statement::While(r#while) => narrow(&mut r#while.block.lock(), budget, depth),
        Statement::Repeat(repeat) => narrow(&mut repeat.block.lock(), budget, depth),
        // The counter and the result locals live in the loop body's scope.
        Statement::NumericFor(r#for) => {
            narrow(&mut r#for.block.lock(), budget.saturating_sub(1), depth)
        }
        Statement::GenericFor(r#for) => narrow(
            &mut r#for.block.lock(),
            budget.saturating_sub(r#for.res_locals.len()),
            depth,
        ),
        _ => {}
    }
    statement.traverse_rvalues(&mut |rvalue| {
        if let RValue::Closure(closure) = rvalue {
            // A closure compiles into its own function, with its own registers.
            let mut function = closure.function.lock();
            let budget = FUNCTION_BUDGET.saturating_sub(function.parameters.len());
            narrow(&mut function.body, budget, 0);
        }
    });
}

/// The locals a statement declares in the block that holds it.
///
/// Loop counters and generic-for results belong to the loop body rather than
/// the enclosing block, so they are not reported here.
fn declared_locals(statement: &Statement) -> Vec<RcLocal> {
    match statement {
        Statement::Assign(assign) if assign.prefix => assign
            .left
            .iter()
            .filter_map(|target| target.as_local())
            .cloned()
            .collect(),
        Statement::Class(class) => vec![class.target.clone()],
        _ => Vec::new(),
    }
}

/// Every local a statement mentions, including inside nested blocks and inside
/// the bodies of closures it builds.
///
/// Upvalue linking rewrites a closure body to name the enclosing locals it
/// captures, so a body that reads one keeps that local alive at the point the
/// closure is constructed.
fn referenced_locals(statement: &Statement, out: &mut FxHashSet<RcLocal>) {
    out.extend(statement.values().into_iter().cloned());
    for rvalue in statement.rvalues() {
        referenced_in_rvalue(rvalue, out);
    }
    match statement {
        Statement::If(r#if) => {
            referenced_in_block(&r#if.then_block.lock(), out);
            referenced_in_block(&r#if.else_block.lock(), out);
        }
        Statement::Do(r#do) => referenced_in_block(&r#do.block.lock(), out),
        Statement::While(r#while) => referenced_in_block(&r#while.block.lock(), out),
        Statement::Repeat(repeat) => referenced_in_block(&repeat.block.lock(), out),
        Statement::NumericFor(r#for) => referenced_in_block(&r#for.block.lock(), out),
        Statement::GenericFor(r#for) => referenced_in_block(&r#for.block.lock(), out),
        _ => {}
    }
}

fn referenced_in_block(block: &Block, out: &mut FxHashSet<RcLocal>) {
    for statement in block.iter() {
        referenced_locals(statement, out);
    }
}

fn referenced_in_rvalue(rvalue: &RValue, out: &mut FxHashSet<RcLocal>) {
    out.extend(rvalue.values().into_iter().cloned());
    if let RValue::Closure(closure) = rvalue {
        referenced_in_block(&closure.function.lock().body, out);
    }
    for child in rvalue.rvalues() {
        referenced_in_rvalue(child, out);
    }
}

/// Whether a statement ends the block that holds it.
///
/// Luau only accepts these last in a block, so a group may end on one but
/// never contain one partway through.
fn is_terminator(statement: &Statement) -> bool {
    matches!(
        statement,
        Statement::Return(_) | Statement::Break(_) | Statement::Continue(_) | Statement::Goto(_)
    )
}

/// Whether grouping would change what the block means.
///
/// A `goto` and its label must stay in one scope, and an explicit upvalue close
/// is tied to the scope it was emitted for. Neither survives being wrapped, so
/// blocks holding them keep their declarations where they are.
fn resists_grouping(block: &Block) -> bool {
    let last = block.len().saturating_sub(1);
    block.iter().enumerate().any(|(index, statement)| {
        matches!(statement, Statement::Label(_) | Statement::Close(_))
            || (is_terminator(statement) && index != last)
    })
}

/// The last statement index that mentions each local declared in this block.
fn last_references(block: &Block) -> FxHashMap<RcLocal, usize> {
    let mut last_reference = FxHashMap::default();
    let mut referenced = FxHashSet::default();
    for (index, statement) in block.iter().enumerate() {
        referenced.clear();
        referenced_locals(statement, &mut referenced);
        for local in referenced.drain() {
            last_reference.insert(local, index);
        }
    }
    last_reference
}

/// Cuts a block into runs that each hold the whole live range of every local
/// they declare.
///
/// Locals in `kept` stay at block level: the statements declaring them become
/// run boundaries, and any other local whose live range would cross such a
/// boundary joins `kept` too. The result is the run list once that has settled.
fn partition(
    declared: &[Vec<RcLocal>],
    last_reference: &FxHashMap<RcLocal, usize>,
    kept: &mut FxHashSet<RcLocal>,
) -> Vec<(usize, usize)> {
    let count = declared.len();
    let reach = |index: usize, kept: &FxHashSet<RcLocal>| {
        declared[index]
            .iter()
            .filter(|local| !kept.contains(*local))
            .filter_map(|local| last_reference.get(local).copied())
            .fold(index, usize::max)
    };
    loop {
        let boundary = |index: usize| declared[index].iter().any(|local| kept.contains(local));
        // A local whose live range runs past the next boundary cannot sit in
        // any run, so it is kept and the split recomputed.
        let mut escaped = None;
        let mut segment_start = 0;
        for index in 0..count {
            if boundary(index) {
                for inner in segment_start..index {
                    for local in &declared[inner] {
                        if !kept.contains(local)
                            && last_reference.get(local).is_some_and(|last| *last >= index)
                        {
                            escaped = Some(local.clone());
                        }
                    }
                }
                segment_start = index + 1;
            }
        }
        if let Some(local) = escaped {
            kept.insert(local);
            continue;
        }

        let mut runs = Vec::new();
        let mut start = 0;
        let mut open_until = 0;
        for index in 0..count {
            if boundary(index) {
                if index > start {
                    runs.push((start, index - 1));
                }
                start = index + 1;
                open_until = start;
                continue;
            }
            open_until = open_until.max(reach(index, kept));
            if index >= open_until {
                runs.push((start, index));
                start = index + 1;
                open_until = start;
            }
        }
        if start < count {
            runs.push((start, count - 1));
        }
        return runs;
    }
}

fn declarations_in(declared: &[Vec<RcLocal>], run: (usize, usize)) -> usize {
    declared[run.0..=run.1].iter().map(Vec::len).sum()
}

/// Joins neighbouring runs until each holds enough declarations to be worth a
/// scope.
///
/// Two runs that touch can always be joined: every live range inside either one
/// already ends within it, so it ends within the join as well. Runs separated by
/// a block-level declaration do not touch and are left apart.
fn merge_runs(declared: &[Vec<RcLocal>], runs: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(runs.len());
    for run in runs {
        match merged.last_mut() {
            Some(last)
                if last.1 + 1 == run.0
                    && declarations_in(declared, *last) < MIN_SCOPE_DECLARATIONS =>
            {
                last.1 = run.1;
            }
            _ => merged.push(run),
        }
    }
    merged
}

/// Groups an over-budget block's declarations into one level of `do ... end`
/// scopes, and reports how many declarations it still holds at block level.
///
/// Scopes that are themselves over budget are narrowed again by the caller's
/// descent, which is what bounds the nesting this adds.
fn group_declarations(block: &mut Block, budget: usize) -> usize {
    let total: usize = block.iter().map(|s| declared_locals(s).len()).sum();
    if total <= budget || resists_grouping(block) {
        return total;
    }

    let declared = block.iter().map(declared_locals).collect::<Vec<_>>();
    let last_reference = last_references(block);
    let worth_wrapping = |run: &&(usize, usize)| {
        declarations_in(&declared, **run) >= MIN_SCOPE_DECLARATIONS
            && (run.0 > 0 || run.1 + 1 < declared.len())
    };

    // A local spanning the whole block admits no split at all, and one spanning
    // most of it leaves a scope nearly as crowded as the block. Keeping the
    // widest-ranged one at block level exposes the runs underneath it, so this
    // repeats until the scopes on offer fit the budget.
    let mut kept = FxHashSet::default();
    let runs = loop {
        let runs = merge_runs(&declared, partition(&declared, &last_reference, &mut kept));
        // Splitting further costs block-level registers for the locals held
        // back, so it is worth doing only while the scopes on offer are still
        // over budget.
        let largest = runs
            .iter()
            .filter(|run| worth_wrapping(run))
            .map(|run| declarations_in(&declared, *run))
            .max();
        if let Some(largest) = largest
            && (largest <= budget || kept.len() >= budget)
        {
            break runs;
        }
        let widest = declared
            .iter()
            .enumerate()
            .flat_map(|(index, locals)| locals.iter().map(move |local| (index, local)))
            .filter(|(_, local)| !kept.contains(*local))
            .max_by_key(|(index, local)| {
                last_reference.get(*local).copied().unwrap_or(*index) - index
            })
            .map(|(_, local)| local.clone());
        match widest {
            Some(local) => kept.insert(local),
            None => break runs,
        };
    };

    let wrapped = runs
        .iter()
        .filter(|run| worth_wrapping(run))
        .map(|run| declarations_in(&declared, *run))
        .sum::<usize>();
    for run in runs.into_iter().rev().filter(|run| worth_wrapping(&run)) {
        let body = Block(block.0.drain(run.0..=run.1).collect());
        block.0.insert(run.0, Do::new(body).into());
    }
    total - wrapped
}

#[cfg(test)]
mod tests {
    use super::{FUNCTION_BUDGET, narrow_local_scopes};
    use crate::{
        Assign, Block, Call, Global, Literal, Local, RValue, RcLocal, Return, Statement,
    };

    fn local(name: &str) -> RcLocal {
        RcLocal::new(Local::new(Some(name.to_owned())))
    }

    /// `local <name> = <name>_source()` followed by `sink(<name>)`.
    fn declare_then_consume(name: &str) -> Vec<Statement> {
        let target = local(name);
        let mut declaration = Assign::new(
            vec![target.clone().into()],
            vec![Call::new(Global::new(format!("{name}_source").into_bytes()).into(), vec![]).into()],
        );
        declaration.prefix = true;
        vec![
            declaration.into(),
            Call::new(Global::new(b"sink".to_vec()).into(), vec![target.into()]).into(),
        ]
    }

    fn scopes(block: &Block) -> usize {
        block
            .iter()
            .filter(|s| matches!(s, Statement::Do(_)))
            .count()
    }

    fn declarations(block: &Block) -> usize {
        block
            .iter()
            .filter_map(Statement::as_assign)
            .filter(|assign| assign.prefix)
            .map(|assign| assign.left.len())
            .sum()
    }

    #[test]
    fn a_block_within_budget_is_left_alone() {
        let mut block = Block(
            (0..FUNCTION_BUDGET)
                .flat_map(|i| declare_then_consume(&format!("v{i}")))
                .collect(),
        );

        narrow_local_scopes(&mut block);

        assert_eq!(scopes(&block), 0);
        assert_eq!(declarations(&block), FUNCTION_BUDGET);
    }

    #[test]
    fn an_over_budget_block_groups_its_declarations() {
        let count = FUNCTION_BUDGET + 40;
        let mut block = Block(
            (0..count)
                .flat_map(|i| declare_then_consume(&format!("v{i}")))
                .collect(),
        );

        narrow_local_scopes(&mut block);

        assert!(scopes(&block) > 0);
        // What is left at block level fits the budget. A trailing run too small
        // to be worth a scope of its own may stay behind.
        assert!(declarations(&block) < FUNCTION_BUDGET);
    }

    #[test]
    fn a_local_read_after_a_run_keeps_that_run_open() {
        let count = FUNCTION_BUDGET + 40;
        let carried = local("carried");
        let mut declaration = Assign::new(
            vec![carried.clone().into()],
            vec![Call::new(Global::new(b"source".to_vec()).into(), vec![]).into()],
        );
        declaration.prefix = true;

        let mut statements = vec![Statement::from(declaration)];
        statements.extend((0..count).flat_map(|i| declare_then_consume(&format!("v{i}"))));
        statements.push(Return::new(vec![carried.clone().into()]).into());
        let mut block = Block(statements);

        narrow_local_scopes(&mut block);

        // `carried` is read by the final return, so no scope may close over it.
        assert!(declarations(&block) < FUNCTION_BUDGET);
        let first = block[0].as_assign().unwrap();
        assert!(first.prefix);
        assert_eq!(first.left[0].as_local(), Some(&carried));
    }

    #[test]
    fn a_label_stops_the_block_from_being_grouped() {
        let count = FUNCTION_BUDGET + 40;
        let mut statements = vec![Statement::from(crate::Label("resume".to_owned()))];
        statements.extend((0..count).flat_map(|i| declare_then_consume(&format!("v{i}"))));
        let mut block = Block(statements);

        narrow_local_scopes(&mut block);

        assert_eq!(scopes(&block), 0);
        assert_eq!(declarations(&block), count);
    }

    #[test]
    fn a_closure_capturing_a_local_keeps_its_run_open() {
        let count = FUNCTION_BUDGET + 40;
        let captured = local("captured");
        let mut declaration = Assign::new(
            vec![captured.clone().into()],
            vec![RValue::Literal(Literal::Number(1.0))],
        );
        declaration.prefix = true;

        let mut statements = vec![Statement::from(declaration)];
        statements.extend((0..count).flat_map(|i| declare_then_consume(&format!("v{i}"))));
        statements.push(
            Call::new(
                Global::new(b"register".to_vec()).into(),
                vec![crate::Closure {
                    function: by_address::ByAddress(triomphe::Arc::new(parking_lot::Mutex::new(
                        crate::Function {
                            body: Block(vec![Return::new(vec![captured.clone().into()]).into()]),
                            ..Default::default()
                        },
                    ))),
                    upvalues: vec![],
                }
                .into()],
            )
            .into(),
        );
        let mut block = Block(statements);

        narrow_local_scopes(&mut block);

        // The closure body reads `captured`, so its declaration stays visible.
        assert!(declarations(&block) < FUNCTION_BUDGET);
        assert_eq!(
            block[0].as_assign().unwrap().left[0].as_local(),
            Some(&captured)
        );
    }
}
