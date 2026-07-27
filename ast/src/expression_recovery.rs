use rustc_hash::FxHashSet;

use crate::{
    Assign, Binary, BinaryOperation, Block, Conditional, LValue, LocalRw, RValue, RcLocal,
    SideEffects, Statement, Traverse, UnaryOperation,
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

fn inline_candidate(statement: &Statement) -> Option<(RcLocal, RValue)> {
    let assign = statement.as_assign()?;
    if assign.prefix || assign.parallel || assign.left.len() != 1 || assign.right.len() != 1 {
        return None;
    }
    let LValue::Local(target) = &assign.left[0] else {
        return None;
    };
    let value = &assign.right[0];
    if matches!(
        value,
        RValue::Local(_)
            | RValue::Call(_)
            | RValue::MethodCall(_)
            | RValue::Closure(_)
            | RValue::Table(_)
            | RValue::VarArg(_)
            | RValue::Select(crate::Select::VarArg(_))
    ) || value.values_read().contains(&target)
    {
        return None;
    }
    Some((target.clone(), value.clone()))
}

fn single_read_statement(block: &Block, start: usize, target: &RcLocal) -> Option<usize> {
    let mut read_at = None;
    for (index, statement) in block.iter().enumerate().skip(start) {
        if is_structured(statement) || statement.values_written().contains(&target) {
            return None;
        }
        for read in statement.values_read() {
            if read == target {
                if read_at.is_some() {
                    return None;
                }
                read_at = Some(index);
            }
        }
    }
    read_at
}

fn has_observable_effect(value: &RValue) -> bool {
    value.has_side_effects()
        && !matches!(
            value,
            RValue::Global(global)
                if global.origin() == crate::GlobalOrigin::CompilerImport
        )
}

fn replace_local_before_effect(
    value: &mut RValue,
    target: &RcLocal,
    replacement: &RValue,
    crossed_effect: &mut bool,
) -> Option<bool> {
    if matches!(value, RValue::Local(local) if local == target) {
        if *crossed_effect {
            return Some(false);
        }
        *value = replacement.clone();
        return Some(true);
    }

    match value {
        RValue::Binary(binary)
            if matches!(binary.operation, BinaryOperation::And | BinaryOperation::Or) =>
        {
            if let Some(replaced) =
                replace_local_before_effect(&mut binary.left, target, replacement, crossed_effect)
            {
                return Some(replaced);
            }
            if binary.right.values_read().contains(&target) {
                return Some(false);
            }
            if binary.has_side_effects()
                || replacement.has_side_effects() && !binary.values_read().is_empty()
            {
                *crossed_effect = true;
            }
            return None;
        }
        RValue::Conditional(conditional) => {
            if let Some(replaced) = replace_local_before_effect(
                &mut conditional.condition,
                target,
                replacement,
                crossed_effect,
            ) {
                return Some(replaced);
            }
            if conditional.then_value.values_read().contains(&target)
                || conditional.else_value.values_read().contains(&target)
            {
                return Some(false);
            }
            if conditional.has_side_effects()
                || replacement.has_side_effects() && !conditional.values_read().is_empty()
            {
                *crossed_effect = true;
            }
            return None;
        }
        RValue::MethodCall(method_call)
        | RValue::Select(crate::Select::MethodCall(method_call)) => {
            return replace_in_method_call(method_call, target, replacement, crossed_effect);
        }
        _ => {}
    }

    for child in value.rvalues_mut() {
        if let Some(replaced) =
            replace_local_before_effect(child, target, replacement, crossed_effect)
        {
            return Some(replaced);
        }
    }
    if has_observable_effect(value)
        || replacement.has_side_effects() && !value.values_read().is_empty()
    {
        *crossed_effect = true;
    }
    None
}

fn replace_in_method_call(
    method_call: &mut crate::MethodCall,
    target: &RcLocal,
    replacement: &RValue,
    crossed_effect: &mut bool,
) -> Option<bool> {
    if let Some(replaced) =
        replace_local_before_effect(&mut method_call.value, target, replacement, crossed_effect)
    {
        return Some(replaced);
    }

    *crossed_effect = true;
    for argument in &mut method_call.arguments {
        if let Some(replaced) =
            replace_local_before_effect(argument, target, replacement, crossed_effect)
        {
            return Some(replaced);
        }
    }
    None
}

fn replace_in_consumer(statement: &mut Statement, target: &RcLocal, replacement: &RValue) -> bool {
    if !matches!(
        statement,
        Statement::Assign(_) | Statement::Call(_) | Statement::MethodCall(_) | Statement::Return(_)
    ) {
        return false;
    }

    let mut crossed_effect = false;
    if let Statement::Assign(assign) = statement {
        for lvalue in &assign.left {
            if lvalue.has_side_effects()
                || replacement.has_side_effects() && !lvalue.values_read().is_empty()
            {
                crossed_effect = true;
            }
        }
        for value in &mut assign.right {
            if let Some(replaced) =
                replace_local_before_effect(value, target, replacement, &mut crossed_effect)
            {
                return replaced;
            }
        }
        return false;
    }
    if let Statement::MethodCall(method_call) = statement {
        return replace_in_method_call(method_call, target, replacement, &mut crossed_effect)
            .unwrap_or(false);
    }

    for value in statement.rvalues_mut() {
        if let Some(replaced) =
            replace_local_before_effect(value, target, replacement, &mut crossed_effect)
        {
            return replaced;
        }
    }
    false
}

fn inline_block_once(block: &mut Block, protected: &FxHashSet<RcLocal>) -> usize {
    let mut removed = 0;
    let mut index = 0;
    while index < block.len() {
        let Some((target, value)) = inline_candidate(&block[index]) else {
            index += 1;
            continue;
        };
        if protected.contains(&target) {
            index += 1;
            continue;
        }
        let Some(read_at) = single_read_statement(block, index + 1, &target) else {
            index += 1;
            continue;
        };
        let source_reads = value
            .values_read()
            .into_iter()
            .cloned()
            .collect::<FxHashSet<_>>();
        if block[index + 1..read_at]
            .iter()
            .any(SideEffects::has_side_effects)
            || block[index + 1..=read_at].iter().any(|statement| {
                statement
                    .values_written()
                    .into_iter()
                    .any(|written| source_reads.contains(written))
            })
        {
            index += 1;
            continue;
        }

        if !replace_in_consumer(&mut block[read_at], &target, &value) {
            index += 1;
            continue;
        }
        block.remove(index);
        removed += 1;
    }
    removed
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

    loop {
        let inlined = inline_block_once(block, protected);
        stats.inlined_temporaries += inlined;
        if inlined == 0 {
            break;
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
        Assign, Binary, BinaryOperation, Block, Call, Global, If, LValue, Literal, Local,
        MethodCall, RValue, RcLocal, Return, Select, Unary, UnaryOperation,
        recover_expressions_with_protected,
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
    fn keeps_conditional_when_a_branch_does_not_assign() {
        let condition = local("condition");
        let result = local("result");
        let mut block = Block(vec![
            If::new(
                condition.into(),
                Block(vec![assign(&result, Literal::Boolean(true).into())]),
                Block::default(),
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
    fn keeps_short_circuit_with_nonempty_else_branch() {
        let first = local("first");
        let next = local("next");
        let fallback = local("fallback");
        let result = local("result");
        let mut block = Block(vec![
            assign(&result, first.into()),
            If::new(
                result.clone().into(),
                Block(vec![assign(&result, next.into())]),
                Block(vec![assign(&result, fallback.into())]),
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

    #[test]
    fn inlines_single_use_expression_into_return() {
        let input = local("input");
        let temporary = local("temporary");
        let arithmetic = Binary::new(
            input.into(),
            Literal::Number(1.0).into(),
            BinaryOperation::Add,
        );
        let mut block = Block(vec![
            assign(&temporary, arithmetic.into()),
            Return::new(vec![temporary.into()]).into(),
        ]);

        let stats = recover_expressions_with_protected(&mut block, &[]);

        assert_eq!(stats.inlined_temporaries, 1);
        assert_eq!(block.to_string(), "return input + 1");
    }

    #[test]
    fn inlines_single_selected_call_without_opening_return_arity() {
        let temporary = local("temporary");
        let selected = Select::Call(Call::new(Global::from("produce").into(), Vec::new()));
        let mut block = Block(vec![
            assign(&temporary, selected.into()),
            Return::new(vec![temporary.into()]).into(),
        ]);

        let stats = recover_expressions_with_protected(&mut block, &[]);

        assert_eq!(stats.inlined_temporaries, 1);
        assert_eq!(block.to_string(), "return (produce())");
    }

    #[test]
    fn keeps_open_call_variant_at_return_boundary() {
        let temporary = local("temporary");
        let call = Call::new(Global::from("produce").into(), Vec::new());
        let mut block = Block(vec![
            assign(&temporary, call.into()),
            Return::new(vec![temporary.into()]).into(),
        ]);

        let stats = recover_expressions_with_protected(&mut block, &[]);

        assert_eq!(stats.inlined_temporaries, 0);
        assert_eq!(block.len(), 2);
    }

    #[test]
    fn keeps_call_result_when_consumer_has_an_earlier_call() {
        let temporary = local("temporary");
        let selected = Select::Call(Call::new(Global::from("produce").into(), Vec::new()));
        let other = Call::new(Global::from("other").into(), Vec::new());
        let mut block = Block(vec![
            assign(&temporary, selected.into()),
            Return::new(vec![other.into(), temporary.into()]).into(),
        ]);

        let stats = recover_expressions_with_protected(&mut block, &[]);

        assert_eq!(stats.inlined_temporaries, 0);
        assert_eq!(block.len(), 2);
    }

    #[test]
    fn keeps_call_result_before_method_lookup() {
        let object = local("object");
        let temporary = local("temporary");
        let selected = Select::Call(Call::new(Global::from("produce").into(), Vec::new()));
        let consume = MethodCall::new(
            object.into(),
            "consume".to_owned(),
            vec![temporary.clone().into()],
        );
        let mut block = Block(vec![assign(&temporary, selected.into()), consume.into()]);

        let stats = recover_expressions_with_protected(&mut block, &[]);

        assert_eq!(stats.inlined_temporaries, 0);
        assert_eq!(block.len(), 2);
    }

    #[test]
    fn keeps_effectful_value_out_of_short_circuit_branch() {
        let condition = local("condition");
        let temporary = local("temporary");
        let selected = Select::Call(Call::new(Global::from("produce").into(), Vec::new()));
        let consumer = Binary::new(
            condition.into(),
            temporary.clone().into(),
            BinaryOperation::And,
        );
        let mut block = Block(vec![
            assign(&temporary, selected.into()),
            Return::new(vec![consumer.into()]).into(),
        ]);

        let stats = recover_expressions_with_protected(&mut block, &[]);

        assert_eq!(stats.inlined_temporaries, 0);
        assert_eq!(block.len(), 2);
    }

    #[test]
    fn keeps_effectful_value_before_earlier_local_read() {
        let observed = local("observed");
        let temporary = local("temporary");
        let selected = Select::Call(Call::new(Global::from("mutate").into(), Vec::new()));
        let mut block = Block(vec![
            assign(&temporary, selected.into()),
            Return::new(vec![observed.into(), temporary.clone().into()]).into(),
        ]);

        let stats = recover_expressions_with_protected(&mut block, &[]);

        assert_eq!(stats.inlined_temporaries, 0);
        assert_eq!(block.len(), 2);
    }

    #[test]
    fn keeps_value_before_effectful_assignment_target() {
        let key = local("key");
        let temporary = local("temporary");
        let selected = Select::Call(Call::new(Global::from("produce").into(), Vec::new()));
        let target = crate::Index::new(
            Call::new(Global::from("get_table").into(), Vec::new()).into(),
            key.into(),
        );
        let consumer = Assign::new(vec![LValue::Index(target)], vec![temporary.clone().into()]);
        let mut block = Block(vec![assign(&temporary, selected.into()), consumer.into()]);

        let stats = recover_expressions_with_protected(&mut block, &[]);

        assert_eq!(stats.inlined_temporaries, 0);
        assert_eq!(block.len(), 2);
    }

    #[test]
    fn keeps_expression_across_overwrite_and_structured_boundary() {
        let input = local("input");
        let condition = local("condition");
        let temporary = local("temporary");
        let arithmetic = Binary::new(
            input.into(),
            Literal::Number(1.0).into(),
            BinaryOperation::Add,
        );
        let mut overwritten = Block(vec![
            assign(&temporary, arithmetic.clone().into()),
            assign(&temporary, Literal::Number(2.0).into()),
            Return::new(vec![temporary.clone().into()]).into(),
        ]);
        let mut structured = Block(vec![
            assign(&temporary, arithmetic.into()),
            If::new(condition.into(), Block::default(), Block::default()).into(),
            Return::new(vec![temporary.into()]).into(),
        ]);

        let overwritten_stats = recover_expressions_with_protected(&mut overwritten, &[]);
        let structured_stats = recover_expressions_with_protected(&mut structured, &[]);

        assert_eq!(overwritten_stats.inlined_temporaries, 1);
        assert!(matches!(
            overwritten[0].as_assign().unwrap().right[0],
            RValue::Binary(_)
        ));
        assert_eq!(structured_stats.inlined_temporaries, 0);
    }

    #[test]
    fn keeps_expression_across_source_write() {
        let source = local("source");
        let temporary = local("temporary");
        let arithmetic = Binary::new(
            source.clone().into(),
            Literal::Number(1.0).into(),
            BinaryOperation::Add,
        );
        let mut block = Block(vec![
            assign(&temporary, arithmetic.into()),
            assign(&source, Literal::Integer(2).into()),
            Return::new(vec![temporary.into()]).into(),
        ]);

        let stats = recover_expressions_with_protected(&mut block, &[]);

        assert_eq!(stats.inlined_temporaries, 0);
        assert_eq!(block.len(), 3);
    }

    #[test]
    fn keeps_expression_across_observable_work() {
        let input = local("input");
        let object = local("object");
        let temporary = local("temporary");
        let observed = local("observed");
        let arithmetic = Binary::new(
            input.into(),
            Literal::Number(1.0).into(),
            BinaryOperation::Add,
        );
        let index = crate::Index::new(object.into(), Literal::String(b"value".to_vec()).into());
        let mut block = Block(vec![
            assign(&temporary, arithmetic.into()),
            assign(&observed, index.into()),
            Return::new(vec![temporary.into()]).into(),
        ]);

        let stats = recover_expressions_with_protected(&mut block, &[]);

        assert_eq!(stats.inlined_temporaries, 0);
        assert_eq!(block.len(), 3);
    }

    #[test]
    fn keeps_expression_for_protected_target() {
        let input = local("input");
        let temporary = local("temporary");
        let arithmetic = Binary::new(
            input.into(),
            Literal::Number(1.0).into(),
            BinaryOperation::Add,
        );
        let mut block = Block(vec![
            assign(&temporary, arithmetic.into()),
            Return::new(vec![temporary.clone().into()]).into(),
        ]);

        let stats =
            recover_expressions_with_protected(&mut block, std::slice::from_ref(&temporary));

        assert_eq!(stats.inlined_temporaries, 0);
        assert_eq!(block.len(), 2);
    }
}
