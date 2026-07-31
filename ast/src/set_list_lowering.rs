use crate::{
    Assign, Block, Empty, Index, LValue, Literal, RValue, SetList, Statement, Table, Traverse,
};

/// Drops constructor slots that no later store filled.
///
/// Lifting keeps a slot for every key in a `DUPTABLE` template so a computed
/// value can be written back into the position the author gave it. A slot left
/// holding nil was never assigned, and a field set to nil is indistinguishable
/// from an absent one, so removing it restores the source table exactly.
pub fn drop_unfilled_table_slots(block: &mut Block) {
    for statement in &mut block.0 {
        statement.traverse_rvalues(&mut |rvalue| {
            if let RValue::Table(Table(fields)) = rvalue {
                fields.retain(|(key, value)| {
                    !(key.is_some() && matches!(value, RValue::Literal(Literal::Nil)))
                });
            }
        });

        match statement {
            Statement::If(r#if) => {
                drop_unfilled_table_slots(&mut r#if.then_block.lock());
                drop_unfilled_table_slots(&mut r#if.else_block.lock());
            }
            Statement::While(r#while) => drop_unfilled_table_slots(&mut r#while.block.lock()),
            Statement::Repeat(repeat) => drop_unfilled_table_slots(&mut repeat.block.lock()),
            Statement::NumericFor(numeric_for) => {
                drop_unfilled_table_slots(&mut numeric_for.block.lock())
            }
            Statement::GenericFor(generic_for) => {
                drop_unfilled_table_slots(&mut generic_for.block.lock())
            }
            _ => {}
        }

        statement.traverse_rvalues(&mut |rvalue| {
            if let RValue::Closure(closure) = rvalue {
                drop_unfilled_table_slots(&mut closure.function.lock().body);
            }
        });
    }
}

/// Rewrites a `SetList` that table-constructor folding could not absorb into
/// the indexed assignments it stands for.
///
/// Folding needs the constructor and its batch to be close enough that neither
/// has to cross the other; register pressure separates them once a constructor
/// grows past a couple of batches. A `SetList` that survives has no Luau
/// spelling, so leaving it in place fails the whole file. Writing the elements
/// out one index at a time says the same thing in valid source, costing one
/// table its shape instead of everything around it.
///
/// A batch with an open tail is left alone: its element count is only known at
/// run time, so no fixed sequence of assignments reproduces it.
pub fn lower_residual_set_lists(block: &mut Block) -> usize {
    let mut lowered = 0;
    let mut index = 0;
    while index < block.len() {
        lower_nested(&mut block.0[index], &mut lowered);

        let Statement::SetList(set_list) = &block.0[index] else {
            index += 1;
            continue;
        };
        if set_list.tail.is_some() {
            index += 1;
            continue;
        }

        let Statement::SetList(SetList {
            object_local,
            index: first_index,
            values,
            ..
        }) = std::mem::replace(&mut block.0[index], Empty {}.into())
        else {
            unreachable!("statement was matched as a set-list above");
        };

        let assignments = values.into_iter().enumerate().map(|(offset, value)| {
            let key = Literal::Number((first_index + offset) as f64);
            let target = Index::new(object_local.clone().into(), key.into());
            Statement::from(Assign::new(vec![LValue::Index(target)], vec![value]))
        });
        let count = block.0.splice(index..index + 1, assignments).count();
        // `count` is the replaced statement itself, so the new statements start
        // where it stood; an empty batch leaves nothing to advance past.
        debug_assert_eq!(count, 1);
        lowered += 1;
    }
    lowered
}

fn lower_nested(statement: &mut Statement, lowered: &mut usize) {
    match statement {
        Statement::If(r#if) => {
            *lowered += lower_residual_set_lists(&mut r#if.then_block.lock());
            *lowered += lower_residual_set_lists(&mut r#if.else_block.lock());
        }
        Statement::While(r#while) => {
            *lowered += lower_residual_set_lists(&mut r#while.block.lock());
        }
        Statement::Repeat(repeat) => {
            *lowered += lower_residual_set_lists(&mut repeat.block.lock());
        }
        Statement::NumericFor(numeric_for) => {
            *lowered += lower_residual_set_lists(&mut numeric_for.block.lock());
        }
        Statement::GenericFor(generic_for) => {
            *lowered += lower_residual_set_lists(&mut generic_for.block.lock());
        }
        _ => {}
    }

    let mut nested = 0;
    statement.traverse_rvalues(&mut |rvalue| {
        if let RValue::Closure(closure) = rvalue {
            nested += lower_residual_set_lists(&mut closure.function.lock().body);
        }
    });
    *lowered += nested;
}
