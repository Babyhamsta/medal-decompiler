use crate::{
    Block, Continue, Literal, RValue, SideEffects, Statement, Traverse, Unary, UnaryOperation,
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ControlFlowCleanupStats {
    pub inverted_empty_then: usize,
    pub inverted_terminal_else: usize,
    pub flattened_guards: usize,
    pub loop_guards: usize,
    pub removed_empty: usize,
}

fn invert_condition(condition: RValue) -> RValue {
    match condition {
        RValue::Unary(Unary {
            value,
            operation: UnaryOperation::Not,
        }) => *value,
        condition => Unary {
            value: Box::new(condition),
            operation: UnaryOperation::Not,
        }
        .into(),
    }
}

fn invert_condition_in_place(condition: &mut RValue) {
    let original = std::mem::replace(condition, Literal::Nil.into());
    *condition = invert_condition(original);
}

fn statement_terminates(statement: &Statement) -> bool {
    match statement {
        Statement::Return(_)
        | Statement::Break(_)
        | Statement::Continue(_)
        | Statement::Goto(_) => true,
        Statement::If(r#if) => {
            let then_block = r#if.then_block.lock();
            let else_block = r#if.else_block.lock();
            !else_block.is_empty() && block_terminates(&then_block) && block_terminates(&else_block)
        }
        _ => false,
    }
}

fn block_terminates(block: &Block) -> bool {
    block
        .iter()
        .rev()
        .find(|statement| !matches!(statement, Statement::Comment(_) | Statement::Empty(_)))
        .is_some_and(statement_terminates)
}

fn clean_statement(statement: &mut Statement, stats: &mut ControlFlowCleanupStats) {
    statement.traverse_rvalues(&mut |value| {
        if let RValue::Closure(closure) = value {
            clean_block(&mut closure.function.lock().body, false, stats);
        }
    });

    match statement {
        Statement::If(r#if) => {
            clean_block(&mut r#if.then_block.lock(), false, stats);
            clean_block(&mut r#if.else_block.lock(), false, stats);
        }
        Statement::While(r#while) => clean_block(&mut r#while.block.lock(), true, stats),
        Statement::Repeat(repeat) => clean_block(&mut repeat.block.lock(), true, stats),
        Statement::NumericFor(numeric_for) => {
            clean_block(&mut numeric_for.block.lock(), true, stats)
        }
        Statement::GenericFor(generic_for) => {
            clean_block(&mut generic_for.block.lock(), true, stats)
        }
        _ => {}
    }
}

fn normalize_empty_branches(block: &mut Block, stats: &mut ControlFlowCleanupStats) {
    let mut index = 0;
    while index < block.len() {
        let Some(r#if) = block[index].as_if_mut() else {
            index += 1;
            continue;
        };
        let then_empty = r#if.then_block.lock().is_empty();
        let else_empty = r#if.else_block.lock().is_empty();

        if then_empty && else_empty && !r#if.condition.has_side_effects() {
            block.remove(index);
            stats.removed_empty += 1;
            continue;
        }
        if then_empty && !else_empty {
            invert_condition_in_place(&mut r#if.condition);
            std::mem::swap(&mut r#if.then_block, &mut r#if.else_block);
            stats.inverted_empty_then += 1;
        }
        index += 1;
    }
}

fn flatten_terminal_branches(block: &mut Block, stats: &mut ControlFlowCleanupStats) {
    let mut index = 0;
    while index < block.len() {
        let moved = {
            let Some(r#if) = block[index].as_if_mut() else {
                index += 1;
                continue;
            };
            let then_terminates = block_terminates(&r#if.then_block.lock());
            let else_terminates = block_terminates(&r#if.else_block.lock());
            let else_empty = r#if.else_block.lock().is_empty();

            if else_empty {
                None
            } else if then_terminates {
                Some(std::mem::take(&mut *r#if.else_block.lock()).0)
            } else if else_terminates {
                invert_condition_in_place(&mut r#if.condition);
                std::mem::swap(&mut r#if.then_block, &mut r#if.else_block);
                stats.inverted_terminal_else += 1;
                Some(std::mem::take(&mut *r#if.else_block.lock()).0)
            } else {
                None
            }
        };

        if let Some(statements) = moved {
            block.0.splice(index + 1..index + 1, statements);
            stats.flattened_guards += 1;
        }
        index += 1;
    }
}

fn recover_loop_tail_guard(block: &mut Block, stats: &mut ControlFlowCleanupStats) {
    let Some(last) = block.last_mut() else {
        return;
    };
    let Some(r#if) = last.as_if_mut() else {
        return;
    };
    if !r#if.else_block.lock().is_empty()
        || r#if.then_block.lock().is_empty()
        || block_terminates(&r#if.then_block.lock())
    {
        return;
    }

    invert_condition_in_place(&mut r#if.condition);
    let body = std::mem::take(&mut *r#if.then_block.lock()).0;
    r#if.then_block.lock().push(Continue {}.into());
    block.0.extend(body);
    stats.loop_guards += 1;
}

fn clean_block(block: &mut Block, loop_body: bool, stats: &mut ControlFlowCleanupStats) {
    for statement in &mut block.0 {
        clean_statement(statement, stats);
    }
    normalize_empty_branches(block, stats);
    flatten_terminal_branches(block, stats);
    if loop_body {
        recover_loop_tail_guard(block, stats);
    }
}

pub fn cleanup_control_flow(block: &mut Block) -> ControlFlowCleanupStats {
    let mut stats = ControlFlowCleanupStats::default();
    clean_block(block, false, &mut stats);
    stats
}

#[cfg(test)]
mod tests {
    use crate::{
        Assign, Block, Call, Global, If, LValue, Literal, Local, RValue, RcLocal, Return,
        Statement, UnaryOperation, While,
    };

    use super::cleanup_control_flow;

    fn local(name: &str) -> RcLocal {
        RcLocal::new(Local::new(Some(name.to_owned())))
    }

    fn assign(name: &RcLocal, value: f64) -> Statement {
        Assign::new(
            vec![LValue::Local(name.clone())],
            vec![Literal::Number(value).into()],
        )
        .into()
    }

    #[test]
    fn inverts_empty_then_branch() {
        let condition = local("condition");
        let value = local("value");
        let mut block = Block(vec![
            If::new(
                condition.clone().into(),
                Block::default(),
                Block(vec![assign(&value, 1.0)]),
            )
            .into(),
        ]);

        let stats = cleanup_control_flow(&mut block);
        let r#if = block[0].as_if().unwrap();

        assert_eq!(stats.inverted_empty_then, 1);
        assert!(r#if.else_block.lock().is_empty());
        assert_eq!(r#if.then_block.lock().len(), 1);
        assert!(matches!(&r#if.condition, RValue::Unary(unary)
                if unary.operation == UnaryOperation::Not
                    && matches!(unary.value.as_ref(), RValue::Local(local) if local == &condition)));
    }

    #[test]
    fn flattens_terminal_then_branch_into_guard_clause() {
        let condition = local("condition");
        let value = local("value");
        let mut block = Block(vec![
            If::new(
                condition.into(),
                Block(vec![
                    Return::new(vec![Literal::Boolean(false).into()]).into(),
                ]),
                Block(vec![assign(&value, 1.0)]),
            )
            .into(),
        ]);

        let stats = cleanup_control_flow(&mut block);

        assert_eq!(stats.flattened_guards, 1);
        assert_eq!(stats.inverted_terminal_else, 0);
        assert_eq!(block.len(), 2);
        assert!(block[0].as_if().unwrap().else_block.lock().is_empty());
        assert!(block[1].as_assign().is_some());
    }

    #[test]
    fn inverts_terminal_else_before_flattening_guard() {
        let condition = local("condition");
        let value = local("value");
        let mut block = Block(vec![
            If::new(
                condition.clone().into(),
                Block(vec![assign(&value, 1.0)]),
                Block(vec![Return::new(Vec::new()).into()]),
            )
            .into(),
        ]);

        let stats = cleanup_control_flow(&mut block);
        let r#if = block[0].as_if().unwrap();

        assert_eq!(stats.flattened_guards, 1);
        assert_eq!(stats.inverted_terminal_else, 1);
        assert_eq!(block.len(), 2);
        assert!(r#if.then_block.lock()[0].as_return().is_some());
        assert!(matches!(&r#if.condition, RValue::Unary(unary)
                if unary.operation == UnaryOperation::Not
                    && matches!(unary.value.as_ref(), RValue::Local(local) if local == &condition)));
        assert!(block[1].as_assign().is_some());
    }

    #[test]
    fn removes_only_pure_empty_conditionals() {
        let condition = local("condition");
        let effectful = Call::new(Global::from("observe").into(), Vec::new());
        let mut block = Block(vec![
            If::new(condition.into(), Block::default(), Block::default()).into(),
            If::new(effectful.into(), Block::default(), Block::default()).into(),
        ]);

        let stats = cleanup_control_flow(&mut block);

        assert_eq!(stats.removed_empty, 1);
        assert_eq!(block.len(), 1);
        assert!(block[0].as_if().is_some());
    }

    #[test]
    fn converts_last_loop_if_into_continue_guard() {
        let enabled = local("enabled");
        let value = local("value");
        let loop_body = Block(vec![
            If::new(
                enabled.clone().into(),
                Block(vec![assign(&value, 1.0)]),
                Block::default(),
            )
            .into(),
        ]);
        let mut block = Block(vec![
            While::new(Literal::Boolean(true).into(), loop_body).into(),
        ]);

        let stats = cleanup_control_flow(&mut block);
        let loop_body = block[0].as_while().unwrap().block.lock();

        assert_eq!(stats.loop_guards, 1);
        assert_eq!(loop_body.len(), 2);
        let guard = loop_body[0].as_if().unwrap();
        assert!(guard.then_block.lock()[0].as_continue().is_some());
        assert!(matches!(&guard.condition, RValue::Unary(unary)
                if unary.operation == UnaryOperation::Not
                    && matches!(unary.value.as_ref(), RValue::Local(local) if local == &enabled)));
        assert!(loop_body[1].as_assign().is_some());
    }

    #[test]
    fn loop_guard_strips_existing_not_without_rewriting_comparison() {
        let disabled = local("disabled");
        let value = local("value");
        let condition = crate::Unary {
            value: Box::new(disabled.clone().into()),
            operation: UnaryOperation::Not,
        };
        let loop_body = Block(vec![
            If::new(
                condition.into(),
                Block(vec![assign(&value, 1.0)]),
                Block::default(),
            )
            .into(),
        ]);
        let mut block = Block(vec![
            While::new(Literal::Boolean(true).into(), loop_body).into(),
        ]);

        cleanup_control_flow(&mut block);
        let loop_body = block[0].as_while().unwrap().block.lock();
        let guard = loop_body[0].as_if().unwrap();

        assert!(matches!(&guard.condition, RValue::Local(local) if local == &disabled));
    }

    #[test]
    fn does_not_expand_terminal_loop_tail_into_redundant_guard() {
        let condition = local("condition");
        let loop_body = Block(vec![
            If::new(
                condition.into(),
                Block(vec![Return::new(Vec::new()).into()]),
                Block::default(),
            )
            .into(),
        ]);
        let mut block = Block(vec![
            While::new(Literal::Boolean(true).into(), loop_body).into(),
        ]);

        let stats = cleanup_control_flow(&mut block);
        let loop_body = block[0].as_while().unwrap().block.lock();

        assert_eq!(stats.loop_guards, 0);
        assert_eq!(loop_body.len(), 1);
        assert!(loop_body[0].as_if().is_some());
    }
}
