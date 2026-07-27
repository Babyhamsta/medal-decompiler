use crate::{Block, LValue, LocalRw, PreOrPost, RValue, RcLocal, SideEffects, Statement, Traverse};
use itertools::Either;

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
                if rvalue.has_side_effects() {
                    crossed_effect = true;
                }
            }
            None
        })
        .unwrap_or(false)
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
}
