use crate::{
    Assign, Binary, BinaryOperation, Block, Call, Closure, Empty, Function, Global, Index, LValue,
    Literal, NumericFor, RValue, RcLocal, Return, SetList, Statement, Table, Traverse, Upvalue,
    VarArg,
};
use by_address::ByAddress;
use parking_lot::Mutex;
use triomphe::Arc;

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
            Statement::Do(r#do) => drop_unfilled_table_slots(&mut r#do.block.lock()),
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
pub fn lower_residual_set_lists(block: &mut Block) -> usize {
    let mut lowered = 0;
    let mut index = 0;
    while index < block.len() {
        lower_nested(&mut block.0[index], &mut lowered);

        let Statement::SetList(_) = &block.0[index] else {
            index += 1;
            continue;
        };
        let Statement::SetList(SetList {
            object_local,
            index: first_index,
            values,
            tail,
        }) = std::mem::replace(&mut block.0[index], Empty {}.into())
        else {
            unreachable!("statement was matched as a set-list above");
        };

        let replacement = if let Some(tail) = tail {
            lower_open_set_list(object_local, first_index, values, tail)
        } else {
            values
                .into_iter()
                .enumerate()
                .map(|(offset, value)| {
                    let key = Literal::Number((first_index + offset) as f64);
                    let target = Index::new(object_local.clone().into(), key.into());
                    Statement::from(Assign::new(vec![LValue::Index(target)], vec![value]))
                })
                .collect()
        };
        let count = block.0.splice(index..index + 1, replacement).count();
        // `count` is the replaced statement itself, so the new statements start
        // where it stood; an empty batch leaves nothing to advance past.
        debug_assert_eq!(count, 1);
        lowered += 1;
    }
    lowered
}

/// Captures an open result list once, including nil holes, then copies the
/// exact runtime arity into the destination table. `select` is part of Lua 5.1.
fn lower_open_set_list(
    object_local: RcLocal,
    first_index: usize,
    mut values: Vec<RValue>,
    tail: RValue,
) -> Vec<Statement> {
    values.push(tail);

    let packed = RcLocal::default();
    let counter = RcLocal::default();
    let count = Call::new(
        Global::from("select").into(),
        vec![Literal::String(b"#".to_vec()).into(), VarArg.into()],
    );
    let table = Table(vec![
        (Some(Literal::String(b"n".to_vec()).into()), count.into()),
        (None, VarArg.into()),
    ]);
    let pack = Closure {
        function: ByAddress(Arc::new(Mutex::new(Function {
            is_variadic: true,
            body: Block(vec![Return::new(vec![table.into()]).into()]),
            ..Function::default()
        }))),
        upvalues: Vec::<Upvalue>::new(),
    };
    let mut capture = Assign::new(
        vec![LValue::Local(packed.clone())],
        vec![Call::new(pack.into(), values).into()],
    );
    capture.prefix = true;

    let offset = first_index.saturating_sub(1);
    let target_index = if offset == 0 {
        counter.clone().into()
    } else {
        Binary::new(
            counter.clone().into(),
            Literal::Number(offset as f64).into(),
            BinaryOperation::Add,
        )
        .into()
    };
    let target = Index::new(object_local.into(), target_index);
    let value = Index::new(packed.clone().into(), counter.clone().into());
    let copy = Assign::new(vec![LValue::Index(target)], vec![value.into()]);
    let packed_count = Index::new(packed.into(), Literal::String(b"n".to_vec()).into());
    let copy_all = NumericFor::new(
        Literal::Number(1.0).into(),
        packed_count.into(),
        Literal::Number(1.0).into(),
        counter,
        Block(vec![copy.into()]),
    );

    vec![capture.into(), copy_all.into()]
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
        Statement::Do(r#do) => {
            *lowered += lower_residual_set_lists(&mut r#do.block.lock());
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

#[cfg(test)]
mod tests {
    use by_address::ByAddress;
    use parking_lot::Mutex;
    use triomphe::Arc;

    use super::lower_residual_set_lists;
    use crate::name_locals::name_locals;
    use crate::{
        Assign, Block, Call, Closure, Do, Function, Global, LValue, Literal, Local, RcLocal,
        SetList, Statement, format_lua51,
    };

    #[test]
    fn open_set_list_preserves_runtime_arity_without_a_pseudo_node() {
        let table = RcLocal::default();
        let tail = Call::new(Global::from("produce").into(), Vec::new()).into();
        let mut block = Block(vec![
            SetList::new(
                table,
                3,
                vec![Literal::String(b"first".to_vec()).into()],
                Some(tail),
            )
            .into(),
        ]);

        assert_eq!(lower_residual_set_lists(&mut block), 1);
        assert_eq!(block.len(), 2);
        assert!(block[0].as_assign().is_some_and(|assign| assign.prefix));
        assert!(block[1].as_numeric_for().is_some());
        assert!(
            block
                .iter()
                .all(|statement| !matches!(statement, Statement::SetList(_)))
        );
    }

    #[test]
    fn lowers_set_lists_inside_do_blocks() {
        let table = RcLocal::default();
        let mut block = Block(vec![
            Do::new(Block(vec![
                SetList::new(table, 1, vec![Literal::Number(1.0).into()], None).into(),
            ]))
            .into(),
        ]);

        assert_eq!(lower_residual_set_lists(&mut block), 1);
        let body = &block[0].as_do().expect("do block").block.lock();
        assert!(matches!(body[0], Statement::Assign(_)));
    }

    #[test]
    fn open_set_list_builtin_is_not_shadowed_by_a_debug_name() {
        let shadow = RcLocal::new(Local::new(Some("select".to_owned())));
        let table = RcLocal::default();
        let mut declaration = Assign::new(
            vec![LValue::Local(shadow.clone())],
            vec![Literal::Nil.into()],
        );
        declaration.prefix = true;
        let mut block = Block(vec![
            declaration.into(),
            SetList::new(
                table,
                1,
                Vec::new(),
                Some(Call::new(Global::from("produce").into(), Vec::new()).into()),
            )
            .into(),
        ]);

        lower_residual_set_lists(&mut block);
        name_locals(&mut block, false);
        let rendered = format_lua51(&block);

        assert_ne!(shadow.0.0.lock().0.as_deref(), Some("select"));
        assert!(rendered.contains("select(\"#\", ...)"));
    }

    #[test]
    fn open_set_list_does_not_rename_out_of_scope_debug_names() {
        let later = RcLocal::new(Local::new(Some("select".to_owned())));
        let mut later_declaration = Assign::new(
            vec![LValue::Local(later.clone())],
            vec![Literal::Nil.into()],
        );
        later_declaration.prefix = true;
        let mut block = Block(vec![
            SetList::new(
                RcLocal::default(),
                1,
                Vec::new(),
                Some(Call::new(Global::from("produce").into(), Vec::new()).into()),
            )
            .into(),
            later_declaration.into(),
        ]);

        lower_residual_set_lists(&mut block);
        name_locals(&mut block, false);

        assert_eq!(later.0.0.lock().0.as_deref(), Some("select"));
    }

    #[test]
    fn open_set_list_does_not_rename_a_sibling_debug_name() {
        let sibling = RcLocal::new(Local::new(Some("select".to_owned())));
        let mut sibling_declaration = Assign::new(
            vec![LValue::Local(sibling.clone())],
            vec![Literal::Nil.into()],
        );
        sibling_declaration.prefix = true;
        let mut block = Block(vec![
            Do::new(Block(vec![sibling_declaration.into()])).into(),
            Do::new(Block(vec![
                SetList::new(
                    RcLocal::default(),
                    1,
                    Vec::new(),
                    Some(Call::new(Global::from("produce").into(), Vec::new()).into()),
                )
                .into(),
            ]))
            .into(),
        ]);

        lower_residual_set_lists(&mut block);
        name_locals(&mut block, false);

        assert_eq!(sibling.0.0.lock().0.as_deref(), Some("select"));
    }

    #[test]
    fn open_set_list_builtin_is_not_captured_by_its_local_function() {
        let function_local = RcLocal::new(Local::new(Some("select".to_owned())));
        let closure = Closure {
            function: ByAddress(Arc::new(Mutex::new(Function {
                name: Some("select".to_owned()),
                body: Block(vec![
                    SetList::new(
                        RcLocal::default(),
                        1,
                        Vec::new(),
                        Some(Call::new(Global::from("produce").into(), Vec::new()).into()),
                    )
                    .into(),
                ]),
                ..Function::default()
            }))),
            upvalues: Vec::new(),
        };
        let mut declaration = Assign::new(
            vec![LValue::Local(function_local.clone())],
            vec![closure.into()],
        );
        declaration.prefix = true;
        let mut block = Block(vec![declaration.into()]);

        lower_residual_set_lists(&mut block);
        name_locals(&mut block, false);
        let rendered = format_lua51(&block);

        assert_ne!(function_local.0.0.lock().0.as_deref(), Some("select"));
        assert!(rendered.contains("select(\"#\", ...)"));
    }
}
