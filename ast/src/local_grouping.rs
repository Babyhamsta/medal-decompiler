use rustc_hash::FxHashSet;

use crate::{Assign, Block, LValue, LocalRw, RValue, RcLocal, Statement, Traverse};

/// Targets one recombined declaration may bind.
///
/// Source that writes a declaration list at all writes a short one; a long
/// run of locals is written a line at a time even when it could be joined.
const MAX_GROUP: usize = 3;

/// Joins runs of adjacent single-name declarations into one declaration list,
/// so `local a = x` followed by `local b = y` is written `local a, b = x, y`.
pub fn combine_local_declarations(block: &mut Block) {
    let mut index = 0;
    while index < block.len() {
        let end = group_end(block, index);
        if end > index + 1 {
            merge(block, index, end);
        }
        index += 1;
    }
    for statement in block.iter_mut() {
        combine_nested(statement);
    }
}

fn combine_nested(statement: &mut Statement) {
    match statement {
        Statement::If(r#if) => {
            combine_local_declarations(&mut r#if.then_block.lock());
            combine_local_declarations(&mut r#if.else_block.lock());
        }
        Statement::Do(r#do) => combine_local_declarations(&mut r#do.block.lock()),
        Statement::While(r#while) => combine_local_declarations(&mut r#while.block.lock()),
        Statement::Repeat(repeat) => combine_local_declarations(&mut repeat.block.lock()),
        Statement::NumericFor(r#for) => combine_local_declarations(&mut r#for.block.lock()),
        Statement::GenericFor(r#for) => combine_local_declarations(&mut r#for.block.lock()),
        _ => {}
    }
    statement.traverse_rvalues(&mut |rvalue| {
        if let RValue::Closure(closure) = rvalue {
            combine_local_declarations(&mut closure.function.lock().body);
        }
    });
}

/// The declaration a statement is, if it is one this pass can move.
///
/// A declaration list assigns each name exactly one value, so only a
/// one-name, one-value declaration can join one. A multiple-result expression
/// is excluded outright: whether it is truncated depends on its position in
/// the list, which joining changes.
fn simple_declaration(statement: &Statement) -> Option<(&RcLocal, &RValue)> {
    let Statement::Assign(assign) = statement else {
        return None;
    };
    if !assign.prefix || assign.parallel || assign.left.len() != 1 || assign.right.len() != 1 {
        return None;
    }
    let LValue::Local(target) = &assign.left[0] else {
        return None;
    };
    if matches!(assign.right[0], RValue::Select(_)) {
        return None;
    }
    Some((target, &assign.right[0]))
}

/// Every local an expression mentions, including inside the bodies of
/// closures it builds — a captured local is read by the closure wherever the
/// closure is constructed.
fn reads(rvalue: &RValue, out: &mut FxHashSet<RcLocal>) {
    out.extend(rvalue.values_read().into_iter().cloned());
    if let RValue::Closure(closure) = rvalue {
        for statement in closure.function.lock().body.iter() {
            out.extend(statement.values().into_iter().cloned());
            for child in statement.rvalues() {
                reads(child, out);
            }
        }
    }
    for child in rvalue.rvalues() {
        reads(child, out);
    }
}

/// Whether evaluating an expression can run code of the program's own.
///
/// Two such expressions in one declaration list would have to be evaluated in
/// the order the statements ran, which is more than this pass is willing to
/// assume, so a group holds at most one.
fn runs_program_code(rvalue: &RValue) -> bool {
    matches!(
        rvalue,
        RValue::Call(_) | RValue::MethodCall(_) | RValue::Select(_)
    ) || rvalue
        .rvalues()
        .iter()
        .any(|child| runs_program_code(child))
}

/// The end of the run of declarations starting at `start` that can be joined.
///
/// A declaration may only join the ones before it when its value does not
/// read what they bind. A declaration list evaluates every value before it
/// binds any name, so such a read would find whatever the name meant outside
/// the block rather than the value just computed for it.
fn group_end(block: &Block, start: usize) -> usize {
    let Some((first, first_value)) = block.get(start).and_then(simple_declaration) else {
        return start;
    };
    // A value that reads the name it defines needs that name bound before it is
    // evaluated, which is what a standalone `local` does and a list does not:
    // a list evaluates every value first, so a recursive closure would capture
    // whatever the name meant outside instead of itself.
    let mut first_reads = FxHashSet::default();
    reads(first_value, &mut first_reads);
    if first_reads.contains(first) {
        return start;
    }
    let mut declared = vec![first.clone()];
    let mut names = FxHashSet::from_iter([first.to_string()]);
    let mut effectful = usize::from(runs_program_code(first_value));
    let mut end = start + 1;
    while end < block.len() && declared.len() < MAX_GROUP {
        let Some((target, value)) = simple_declaration(&block[end]) else {
            break;
        };
        // Two locals sharing a name in one list would leave the earlier one
        // unnameable, and the same local declared twice is not a list at all.
        if declared.contains(target) || !names.insert(target.to_string()) {
            break;
        }
        effectful += usize::from(runs_program_code(value));
        if effectful > 1 {
            break;
        }
        let mut read = FxHashSet::default();
        reads(value, &mut read);
        if declared.iter().any(|local| read.contains(local)) || read.contains(target) {
            break;
        }
        declared.push(target.clone());
        end += 1;
    }
    end
}

fn merge(block: &mut Block, start: usize, end: usize) {
    let mut left = Vec::with_capacity(end - start);
    let mut right = Vec::with_capacity(end - start);
    for statement in block.0.drain(start..end) {
        let Statement::Assign(assign) = statement else {
            unreachable!("only declarations are grouped")
        };
        left.extend(assign.left);
        right.extend(assign.right);
    }
    let mut joined = Assign::new(left, right);
    joined.prefix = true;
    block.0.insert(start, joined.into());
}

#[cfg(test)]
mod tests {
    use super::combine_local_declarations;
    use crate::{
        Assign, Block, Call, Closure, Function, Global, Literal, Local, RValue, RcLocal, Return,
        Select, Statement,
    };
    use by_address::ByAddress;
    use parking_lot::Mutex;
    use triomphe::Arc;

    fn local(name: &str) -> RcLocal {
        RcLocal::new(Local::new(Some(name.to_owned())))
    }

    fn declare(name: &str, value: RValue) -> Statement {
        let mut declaration = Assign::new(vec![local(name).into()], vec![value]);
        declaration.prefix = true;
        declaration.into()
    }

    fn declare_reading(name: &str, read: &RcLocal) -> Statement {
        let mut declaration =
            Assign::new(vec![local(name).into()], vec![RValue::Local(read.clone())]);
        declaration.prefix = true;
        declaration.into()
    }

    fn call(name: &str) -> RValue {
        Call::new(Global::from(name).into(), Vec::new()).into()
    }

    #[test]
    fn adjacent_declarations_of_plain_values_are_joined() {
        let mut block = Block(vec![
            declare("format", Global::from("string").into()),
            declare("round", Global::from("math").into()),
        ]);

        combine_local_declarations(&mut block);

        assert_eq!(block.to_string(), "local format, round = string, math");
    }

    #[test]
    fn a_self_referencing_declaration_is_never_joined() {
        // A recursive local reads the name it is defining. A list binds its
        // names only after every value is evaluated, so joining one would leave
        // the recursive call resolving to the outer name instead of itself.
        let recursive = local("recursive");
        let mut declaration = Assign::new(
            vec![recursive.clone().into()],
            vec![recursive.clone().into()],
        );
        declaration.prefix = true;
        let mut block = Block(vec![
            declaration.into(),
            declare("other", Global::from("source").into()),
        ]);

        combine_local_declarations(&mut block);

        assert_eq!(
            block.to_string(),
            "local recursive = recursive\nlocal other = source"
        );
    }

    #[test]
    fn a_later_self_referencing_declaration_does_not_join_the_group() {
        let recursive = local("recursive");
        let mut declaration = Assign::new(
            vec![recursive.clone().into()],
            vec![recursive.clone().into()],
        );
        declaration.prefix = true;
        let mut block = Block(vec![
            declare("first", Global::from("source").into()),
            declaration.into(),
        ]);

        combine_local_declarations(&mut block);

        assert_eq!(
            block.to_string(),
            "local first = source\nlocal recursive = recursive"
        );
    }

    #[test]
    fn a_value_reading_an_earlier_name_in_the_group_starts_a_new_one() {
        // `local first = source` then `local second = first` cannot become
        // `local first, second = source, first`: the list evaluates `first`
        // before it binds one, so the second value would read whatever
        // `first` meant outside this block.
        let first = local("first");
        let mut declaration = Assign::new(
            vec![first.clone().into()],
            vec![Global::from("source").into()],
        );
        declaration.prefix = true;
        let mut block = Block(vec![declaration.into(), declare_reading("second", &first)]);

        combine_local_declarations(&mut block);

        assert_eq!(
            block.to_string(),
            "local first = source\nlocal second = first"
        );
    }

    #[test]
    fn a_closure_capturing_an_earlier_name_in_the_group_starts_a_new_one() {
        let first = local("first");
        let mut declaration = Assign::new(
            vec![first.clone().into()],
            vec![Global::from("source").into()],
        );
        declaration.prefix = true;
        let closure = Closure {
            function: ByAddress(Arc::new(Mutex::new(Function {
                body: Block(vec![Return::new(vec![first.clone().into()]).into()]),
                ..Default::default()
            }))),
            upvalues: Vec::new(),
        };
        let mut block = Block(vec![declaration.into(), declare("second", closure.into())]);

        combine_local_declarations(&mut block);

        let formatted = block.to_string();

        assert!(formatted.starts_with("local first = source"), "{formatted}");
        assert!(!formatted.contains("local first, second"), "{formatted}");
    }

    #[test]
    fn two_values_that_run_program_code_are_left_apart() {
        let mut block = Block(vec![
            declare("a", call("first")),
            declare("b", call("second")),
        ]);

        combine_local_declarations(&mut block);

        assert_eq!(block.to_string(), "local a = first()\nlocal b = second()");
    }

    #[test]
    fn one_value_that_runs_program_code_may_still_join() {
        let mut block = Block(vec![
            declare("a", call("first")),
            declare("b", Literal::Number(1.0).into()),
        ]);

        combine_local_declarations(&mut block);

        assert_eq!(block.to_string(), "local a, b = first(), 1");
    }

    #[test]
    fn a_multiple_result_value_is_never_joined() {
        let selected = Select::Call(Call::new(Global::from("produce").into(), Vec::new()));
        let mut block = Block(vec![
            declare("a", Literal::Number(1.0).into()),
            declare("b", selected.into()),
        ]);

        combine_local_declarations(&mut block);

        assert_eq!(block.to_string(), "local a = 1\nlocal b = produce()");
    }

    #[test]
    fn a_run_longer_than_a_group_is_split_into_groups() {
        let mut block = Block(
            (0..5)
                .map(|index| declare(&format!("v{index}"), Literal::Number(index as f64).into()))
                .collect(),
        );

        combine_local_declarations(&mut block);

        assert_eq!(
            block.to_string(),
            "local v0, v1, v2 = 0, 1, 2\nlocal v3, v4 = 3, 4"
        );
    }

    #[test]
    fn a_plain_assignment_is_not_a_declaration_and_breaks_the_run() {
        let mut block = Block(vec![
            declare("a", Literal::Number(1.0).into()),
            Assign::new(
                vec![Global::from("g").into()],
                vec![Literal::Number(2.0).into()],
            )
            .into(),
            declare("b", Literal::Number(3.0).into()),
        ]);

        combine_local_declarations(&mut block);

        assert_eq!(block.to_string(), "local a = 1\ng = 2\nlocal b = 3");
    }

    #[test]
    fn declarations_inside_nested_blocks_are_joined_too() {
        let inner = Block(vec![
            declare("a", Literal::Number(1.0).into()),
            declare("b", Literal::Number(2.0).into()),
        ]);
        let mut block = Block(vec![crate::Do::new(inner).into()]);

        combine_local_declarations(&mut block);

        assert_eq!(block.to_string(), "do\n\tlocal a, b = 1, 2\nend");
    }
}
