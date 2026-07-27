use crate::{Block, LValue, LocalRw, PreOrPost, RValue, RcLocal, SideEffects, Statement, Traverse};
use itertools::Either;
use rustc_hash::FxHashSet;

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
        if is_structured(statement) {
            return None;
        }
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

fn replace_after_safe_prefix(statement: &mut Statement, alias: &RcLocal, source: &RcLocal) -> bool {
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
                    && !matches!(
                        rvalue,
                        RValue::Global(global)
                            if global.origin() == crate::GlobalOrigin::CompilerImport
                    )
                {
                    crossed_effect = true;
                }
            }
            None
        })
        .unwrap_or(false)
}

fn eliminate_block_once(block: &mut Block, protected: &FxHashSet<RcLocal>) -> usize {
    let mut removed = 0;
    let mut index = 0;
    while index < block.len() {
        let Some((alias, source)) = alias_assignment(&block[index]) else {
            index += 1;
            continue;
        };
        if protected.contains(&alias) {
            index += 1;
            continue;
        }
        let Some(read_at) = single_read_statement(block, index + 1, &alias) else {
            index += 1;
            continue;
        };
        if block[index + 1..=read_at]
            .iter()
            .any(|statement| statement.values_written().contains(&&source))
            || block[index + 1..read_at]
                .iter()
                .any(SideEffects::has_side_effects)
        {
            index += 1;
            continue;
        }

        if !replace_after_safe_prefix(&mut block[read_at], &alias, &source) {
            index += 1;
            continue;
        }
        block.remove(index);
        removed += 1;
    }
    removed
}

pub(crate) fn collect_reference_captures(block: &mut Block, protected: &mut FxHashSet<RcLocal>) {
    for statement in &mut block.0 {
        statement.traverse_rvalues(&mut |rvalue| {
            if let RValue::Closure(closure) = rvalue {
                protected.extend(closure.upvalues.iter().filter_map(|upvalue| match upvalue {
                    crate::Upvalue::Ref(local) => Some(local.clone()),
                    crate::Upvalue::Copy(_) => None,
                }));
            }
        });

        match statement {
            Statement::If(value) => {
                collect_reference_captures(&mut value.then_block.lock(), protected);
                collect_reference_captures(&mut value.else_block.lock(), protected);
            }
            Statement::While(value) => {
                collect_reference_captures(&mut value.block.lock(), protected)
            }
            Statement::Repeat(value) => {
                collect_reference_captures(&mut value.block.lock(), protected)
            }
            Statement::NumericFor(value) => {
                collect_reference_captures(&mut value.block.lock(), protected)
            }
            Statement::GenericFor(value) => {
                collect_reference_captures(&mut value.block.lock(), protected)
            }
            _ => {}
        }
    }
}

fn eliminate_aliases_in_tree(block: &mut Block, protected: &FxHashSet<RcLocal>) -> usize {
    let mut removed = 0;
    loop {
        let changed = eliminate_block_once(block, protected);
        removed += changed;
        if changed == 0 {
            break;
        }
    }

    for statement in &mut block.0 {
        removed += match statement {
            Statement::If(value) => {
                eliminate_aliases_in_tree(&mut value.then_block.lock(), protected)
                    + eliminate_aliases_in_tree(&mut value.else_block.lock(), protected)
            }
            Statement::While(value) => {
                eliminate_aliases_in_tree(&mut value.block.lock(), protected)
            }
            Statement::Repeat(value) => {
                eliminate_aliases_in_tree(&mut value.block.lock(), protected)
            }
            Statement::NumericFor(value) => {
                eliminate_aliases_in_tree(&mut value.block.lock(), protected)
            }
            Statement::GenericFor(value) => {
                eliminate_aliases_in_tree(&mut value.block.lock(), protected)
            }
            _ => 0,
        };
    }
    removed
}

pub fn eliminate_aliases_with_protected(block: &mut Block, protected: &[RcLocal]) -> usize {
    let mut protected = protected.iter().cloned().collect::<FxHashSet<_>>();
    collect_reference_captures(block, &mut protected);
    eliminate_aliases_in_tree(block, &protected)
}

pub fn eliminate_aliases(block: &mut Block) -> usize {
    eliminate_aliases_with_protected(block, &[])
}

#[cfg(test)]
mod tests {
    use crate::{Assign, Block, LValue, Literal, LocalRw, RValue, RcLocal, Return};

    use super::{eliminate_aliases, eliminate_aliases_with_protected};

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

    #[test]
    fn keeps_snapshot_alias_when_call_occurs_before_use_statement() {
        let source = RcLocal::default();
        let alias = RcLocal::default();
        let call = crate::Call::new(crate::Global::from("mutate").into(), Vec::new());
        let mut block = Block(vec![
            Assign::new(vec![alias.clone().into()], vec![source.clone().into()]).into(),
            call.into(),
            Return::new(vec![alias.clone().into()]).into(),
        ]);

        assert_eq!(eliminate_aliases(&mut block), 0);
        assert!(block[0].values_written().contains(&&alias));
    }

    #[test]
    fn keeps_snapshot_alias_when_global_lookup_runs_before_use() {
        let source = RcLocal::default();
        let alias = RcLocal::default();
        let mut block = Block(vec![
            Assign::new(vec![alias.clone().into()], vec![source.clone().into()]).into(),
            Return::new(vec![
                crate::Global::from("possibly_dynamic").into(),
                alias.clone().into(),
            ])
            .into(),
        ]);

        assert_eq!(eliminate_aliases(&mut block), 0);
        assert!(block[0].values_written().contains(&&alias));
    }

    #[test]
    fn eliminates_alias_after_compiler_import_prefix() {
        let source = RcLocal::default();
        let alias = RcLocal::default();
        let call = crate::Call::new(
            crate::Global::compiler_import(b"setmetatable".to_vec()).into(),
            vec![alias.clone().into()],
        );
        let mut block = Block(vec![
            Assign::new(vec![alias.clone().into()], vec![source.clone().into()]).into(),
            Return::new(vec![call.into()]).into(),
        ]);

        assert_eq!(eliminate_aliases(&mut block), 1);
        assert_eq!(block.len(), 1);
        assert!(block[0].values_read().contains(&&source));
    }

    #[test]
    fn keeps_snapshot_alias_when_index_runs_before_use() {
        let source = RcLocal::default();
        let alias = RcLocal::default();
        let table = RcLocal::default();
        let key = RcLocal::default();
        let index = crate::Index::new(table.into(), key.into());
        let mut block = Block(vec![
            Assign::new(vec![alias.clone().into()], vec![source.clone().into()]).into(),
            Return::new(vec![index.into(), alias.clone().into()]).into(),
        ]);

        assert_eq!(eliminate_aliases(&mut block), 0);
        assert!(block[0].values_written().contains(&&alias));
    }

    #[test]
    fn keeps_alias_captured_by_reference() {
        let source = RcLocal::default();
        let alias = RcLocal::default();
        let holder = RcLocal::default();
        let closure = crate::Closure {
            function: by_address::ByAddress(triomphe::Arc::new(parking_lot::Mutex::new(
                crate::Function::default(),
            ))),
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
    fn eliminates_nested_aliases_inside_structured_blocks() {
        let source = RcLocal::default();
        let alias = RcLocal::default();
        let nested = Block(vec![
            Assign::new(vec![alias.clone().into()], vec![source.clone().into()]).into(),
            Return::new(vec![alias.into()]).into(),
        ]);
        let mut block = Block(vec![
            crate::If::new(Literal::Boolean(true).into(), nested, Block::default()).into(),
        ]);

        assert_eq!(eliminate_aliases(&mut block), 1);
        let then_block = block[0].as_if().unwrap().then_block.lock();
        assert_eq!(then_block.len(), 1);
        assert!(then_block[0].values_read().contains(&&source));
    }

    #[test]
    fn keeps_outer_alias_across_if_statement() {
        let source = RcLocal::default();
        let alias = RcLocal::default();
        let then_block = Block(vec![Return::new(vec![alias.clone().into()]).into()]);
        let mut block = Block(vec![
            Assign::new(vec![alias.clone().into()], vec![source.into()]).into(),
            crate::If::new(alias.clone().into(), then_block, Block::default()).into(),
        ]);

        assert_eq!(eliminate_aliases(&mut block), 0);
        assert!(block[0].values_written().contains(&&alias));
    }

    #[test]
    fn keeps_outer_snapshot_alias_across_while_statement() {
        let source = RcLocal::default();
        let alias = RcLocal::default();
        let loop_body = Block(vec![
            Assign::new(
                vec![source.clone().into()],
                vec![Literal::Boolean(false).into()],
            )
            .into(),
        ]);
        let mut block = Block(vec![
            Assign::new(vec![alias.clone().into()], vec![source.into()]).into(),
            crate::While::new(alias.clone().into(), loop_body).into(),
        ]);

        assert_eq!(eliminate_aliases(&mut block), 0);
        assert!(block[0].values_written().contains(&&alias));
    }

    #[test]
    fn keeps_outer_snapshot_alias_across_repeat_statement() {
        let source = RcLocal::default();
        let alias = RcLocal::default();
        let loop_body = Block(vec![
            Assign::new(
                vec![source.clone().into()],
                vec![Literal::Boolean(false).into()],
            )
            .into(),
        ]);
        let mut block = Block(vec![
            Assign::new(vec![alias.clone().into()], vec![source.into()]).into(),
            crate::Repeat::new(alias.clone().into(), loop_body).into(),
        ]);

        assert_eq!(eliminate_aliases(&mut block), 0);
        assert!(block[0].values_written().contains(&&alias));
    }

    #[test]
    fn keeps_alias_captured_by_reference_before_assignment() {
        let source = RcLocal::default();
        let alias = RcLocal::default();
        let holder = RcLocal::default();
        let closure = crate::Closure {
            function: by_address::ByAddress(triomphe::Arc::new(parking_lot::Mutex::new(
                crate::Function::default(),
            ))),
            upvalues: vec![crate::Upvalue::Ref(alias.clone())],
        };
        let capture_block = Block(vec![
            Assign::new(vec![holder.into()], vec![closure.into()]).into(),
        ]);
        let mut block = Block(vec![
            crate::If::new(
                Literal::Boolean(true).into(),
                capture_block,
                Block::default(),
            )
            .into(),
            Assign::new(vec![alias.clone().into()], vec![source.into()]).into(),
            Return::new(vec![alias.clone().into()]).into(),
        ]);

        assert_eq!(eliminate_aliases(&mut block), 0);
        assert!(block[1].values_written().contains(&&alias));
    }

    #[test]
    fn keeps_assignment_to_incoming_upvalue() {
        let source = RcLocal::default();
        let incoming = RcLocal::default();
        let mut block = Block(vec![
            Assign::new(vec![incoming.clone().into()], vec![source.into()]).into(),
            Return::new(vec![incoming.clone().into()]).into(),
        ]);

        assert_eq!(
            eliminate_aliases_with_protected(&mut block, &[incoming.clone()]),
            0
        );
        assert!(block[0].values_written().contains(&&incoming));
    }
}
