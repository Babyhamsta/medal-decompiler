use rustc_hash::FxHashSet;

use crate::{
    Assign, Binary, BinaryOperation, Block, Conditional, LValue, LocalRw, RValue, RcLocal,
    Statement, UnaryOperation,
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExpressionRecoveryStats {
    pub conditionals: usize,
    pub short_circuits: usize,
    pub inlined_temporaries: usize,
}

fn single_local_assignment(block: &Block) -> Option<(RcLocal, RValue)> {
    if block.len() != 1 {
        return None;
    }
    let assign = block[0].as_assign()?;
    if assign.prefix || assign.parallel || assign.left.len() != 1 || assign.right.len() != 1 {
        return None;
    }
    let LValue::Local(target) = &assign.left[0] else {
        return None;
    };
    Some((target.clone(), assign.right[0].clone()))
}

fn recover_nested(
    statement: &mut Statement,
    protected: &FxHashSet<RcLocal>,
) -> ExpressionRecoveryStats {
    match statement {
        Statement::If(value) => {
            let then_stats = recover_block(&mut value.then_block.lock(), protected);
            let else_stats = recover_block(&mut value.else_block.lock(), protected);
            ExpressionRecoveryStats {
                conditionals: then_stats.conditionals + else_stats.conditionals,
                short_circuits: then_stats.short_circuits + else_stats.short_circuits,
                inlined_temporaries: then_stats.inlined_temporaries
                    + else_stats.inlined_temporaries,
            }
        }
        Statement::While(value) => recover_block(&mut value.block.lock(), protected),
        Statement::Repeat(value) => recover_block(&mut value.block.lock(), protected),
        Statement::NumericFor(value) => recover_block(&mut value.block.lock(), protected),
        Statement::GenericFor(value) => recover_block(&mut value.block.lock(), protected),
        _ => ExpressionRecoveryStats::default(),
    }
}

fn try_recover_conditional(statement: &mut Statement) -> bool {
    let Statement::If(value) = statement else {
        return false;
    };
    let Some((then_target, then_value)) = single_local_assignment(&value.then_block.lock()) else {
        return false;
    };
    let Some((else_target, else_value)) = single_local_assignment(&value.else_block.lock()) else {
        return false;
    };
    if then_target != else_target
        || value.condition.values_read().contains(&&then_target)
        || then_value.values_read().contains(&&then_target)
        || else_value.values_read().contains(&&then_target)
    {
        return false;
    }

    let condition = std::mem::replace(&mut value.condition, crate::Literal::Nil.into());
    *statement = Assign::new(
        vec![then_target.into()],
        vec![Conditional::new(condition, then_value, else_value).into()],
    )
    .into();
    true
}

fn short_circuit_operator(condition: &RValue, target: &RcLocal) -> Option<BinaryOperation> {
    match condition {
        RValue::Local(local) if local == target => Some(BinaryOperation::And),
        RValue::Unary(unary)
            if unary.operation == UnaryOperation::Not
                && matches!(unary.value.as_ref(), RValue::Local(local) if local == target) =>
        {
            Some(BinaryOperation::Or)
        }
        _ => None,
    }
}

fn try_extend_short_circuit(
    first: &mut Statement,
    second: &Statement,
    protected: &FxHashSet<RcLocal>,
) -> bool {
    let Some(assign) = first.as_assign_mut() else {
        return false;
    };
    if assign.prefix || assign.parallel || assign.left.len() != 1 || assign.right.len() != 1 {
        return false;
    }
    let LValue::Local(target) = &assign.left[0] else {
        return false;
    };
    if protected.contains(target) || assign.right[0].values_read().contains(&target) {
        return false;
    }

    let Some(branch) = second.as_if() else {
        return false;
    };
    let Some(operation) = short_circuit_operator(&branch.condition, target) else {
        return false;
    };
    if !branch.else_block.lock().is_empty() {
        return false;
    }
    let Some((branch_target, branch_value)) = single_local_assignment(&branch.then_block.lock())
    else {
        return false;
    };
    if &branch_target != target || branch_value.values_read().contains(&target) {
        return false;
    }

    let prefix = assign.right.pop().unwrap();
    assign
        .right
        .push(Binary::new(prefix, branch_value, operation).into());
    true
}

fn recover_block(block: &mut Block, protected: &FxHashSet<RcLocal>) -> ExpressionRecoveryStats {
    let mut stats = ExpressionRecoveryStats::default();
    for statement in &mut block.0 {
        let nested = recover_nested(statement, protected);
        stats.conditionals += nested.conditionals;
        stats.short_circuits += nested.short_circuits;
        stats.inlined_temporaries += nested.inlined_temporaries;
        if try_recover_conditional(statement) {
            stats.conditionals += 1;
        }
    }

    let mut index = 0;
    while index + 1 < block.len() {
        let (before, after) = block.0.split_at_mut(index + 1);
        if try_extend_short_circuit(&mut before[index], &after[0], protected) {
            block.remove(index + 1);
            stats.short_circuits += 1;
        } else {
            index += 1;
        }
    }
    stats
}

pub fn recover_expressions_with_protected(
    block: &mut Block,
    protected: &[RcLocal],
) -> ExpressionRecoveryStats {
    let mut protected = protected.iter().cloned().collect::<FxHashSet<_>>();
    crate::alias_elimination::collect_reference_captures(block, &mut protected);
    recover_block(block, &protected)
}

#[cfg(test)]
mod tests {
    use crate::{
        Assign, Binary, BinaryOperation, Block, Call, Global, If, LValue, Literal, Local, RValue,
        RcLocal, Unary, UnaryOperation, recover_expressions_with_protected,
    };

    fn local(name: &str) -> RcLocal {
        RcLocal::new(Local::new(Some(name.to_owned())))
    }

    fn assign(target: &RcLocal, value: RValue) -> crate::Statement {
        Assign::new(vec![LValue::Local(target.clone())], vec![value]).into()
    }

    #[test]
    fn folds_falsy_conditional_assignment() {
        let condition = local("condition");
        let result = local("result");
        let fallback = Call::new(Global::from("fallback").into(), Vec::new());
        let mut block = Block(vec![
            If::new(
                condition.into(),
                Block(vec![assign(&result, Literal::Boolean(false).into())]),
                Block(vec![assign(&result, fallback.into())]),
            )
            .into(),
        ]);

        let stats = recover_expressions_with_protected(&mut block, &[]);

        assert_eq!(stats.conditionals, 1);
        assert_eq!(
            block.to_string(),
            "result = if condition then false else fallback()"
        );
    }

    #[test]
    fn keeps_conditional_with_extra_branch_statement() {
        let condition = local("condition");
        let result = local("result");
        let observe = Call::new(Global::from("observe").into(), Vec::new());
        let mut block = Block(vec![
            If::new(
                condition.into(),
                Block(vec![
                    observe.into(),
                    assign(&result, Literal::Boolean(true).into()),
                ]),
                Block(vec![assign(&result, Literal::Boolean(false).into())]),
            )
            .into(),
        ]);

        let stats = recover_expressions_with_protected(&mut block, &[]);

        assert_eq!(stats.conditionals, 0);
        assert!(block[0].as_if().is_some());
    }

    #[test]
    fn keeps_conditional_with_different_targets() {
        let condition = local("condition");
        let left = local("left");
        let right = local("right");
        let mut block = Block(vec![
            If::new(
                condition.into(),
                Block(vec![assign(&left, Literal::Boolean(true).into())]),
                Block(vec![assign(&right, Literal::Boolean(false).into())]),
            )
            .into(),
        ]);

        let stats = recover_expressions_with_protected(&mut block, &[]);

        assert_eq!(stats.conditionals, 0);
        assert!(block[0].as_if().is_some());
    }

    #[test]
    fn keeps_declaration_sensitive_conditional_self_read() {
        let condition = local("condition");
        let result = local("result");
        let mut block = Block(vec![
            If::new(
                condition.into(),
                Block(vec![assign(&result, result.clone().into())]),
                Block(vec![assign(&result, Literal::Nil.into())]),
            )
            .into(),
        ]);

        let stats = recover_expressions_with_protected(&mut block, &[]);

        assert_eq!(stats.conditionals, 0);
        assert!(block[0].as_if().is_some());
    }

    #[test]
    fn extends_adjacent_short_circuit_chain() {
        let first = local("first");
        let second = local("second");
        let third = local("third");
        let result = local("result");
        let prefix = Binary::new(first.into(), second.into(), BinaryOperation::And);
        let mut block = Block(vec![
            assign(&result, prefix.into()),
            If::new(
                result.clone().into(),
                Block(vec![assign(&result, third.into())]),
                Block::default(),
            )
            .into(),
        ]);

        let stats = recover_expressions_with_protected(&mut block, &[]);

        assert_eq!(stats.short_circuits, 1);
        assert_eq!(block.to_string(), "result = first and second and third");
    }

    #[test]
    fn extends_adjacent_or_chain() {
        let first = local("first");
        let fallback = local("fallback");
        let result = local("result");
        let mut block = Block(vec![
            assign(&result, first.into()),
            If::new(
                Unary::new(result.clone().into(), UnaryOperation::Not).into(),
                Block(vec![assign(&result, fallback.into())]),
                Block::default(),
            )
            .into(),
        ]);

        let stats = recover_expressions_with_protected(&mut block, &[]);

        assert_eq!(stats.short_circuits, 1);
        assert_eq!(block.to_string(), "result = first or fallback");
    }

    #[test]
    fn keeps_short_circuit_when_appended_value_reads_target() {
        let first = local("first");
        let result = local("result");
        let next = Binary::new(
            result.clone().into(),
            Literal::Number(1.0).into(),
            BinaryOperation::Add,
        );
        let mut block = Block(vec![
            assign(&result, first.into()),
            If::new(
                result.clone().into(),
                Block(vec![assign(&result, next.into())]),
                Block::default(),
            )
            .into(),
        ]);

        let stats = recover_expressions_with_protected(&mut block, &[]);

        assert_eq!(stats.short_circuits, 0);
        assert_eq!(block.len(), 2);
    }

    #[test]
    fn keeps_short_circuit_for_protected_target() {
        let first = local("first");
        let third = local("third");
        let result = local("result");
        let mut block = Block(vec![
            assign(&result, first.into()),
            If::new(
                result.clone().into(),
                Block(vec![assign(&result, third.into())]),
                Block::default(),
            )
            .into(),
        ]);

        let stats = recover_expressions_with_protected(&mut block, std::slice::from_ref(&result));

        assert_eq!(stats.short_circuits, 0);
        assert_eq!(block.len(), 2);
    }
}
