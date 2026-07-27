use crate::{Block, LValue, LocalRw, RcLocal, RValue, Statement};

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
