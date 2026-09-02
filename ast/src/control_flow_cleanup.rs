use crate::{
    BinaryOperation, Block, Continue, Literal, LocalRw, RValue, RcLocal, Reduce, Return,
    SideEffects, Statement, Traverse, Unary, UnaryOperation,
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
        // Reducing folds the negation into the operator wherever that is
        // exact, so an inverted equality reads as `~=` rather than `not (…)`.
        // Ordering comparisons have no such form and stay negated.
        condition => Unary {
            value: Box::new(condition),
            operation: UnaryOperation::Not,
        }
        .reduce_condition(),
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

fn clean_statement(
    statement: &mut Statement,
    allow_continue: bool,
    stats: &mut ControlFlowCleanupStats,
) {
    statement.traverse_rvalues(&mut |value| {
        if let RValue::Closure(closure) = value {
            clean_block(
                &mut closure.function.lock().body,
                false,
                allow_continue,
                stats,
            );
        }
    });

    match statement {
        Statement::If(r#if) => {
            clean_block(&mut r#if.then_block.lock(), false, allow_continue, stats);
            clean_block(&mut r#if.else_block.lock(), false, allow_continue, stats);
        }
        Statement::While(r#while) => {
            clean_block(&mut r#while.block.lock(), true, allow_continue, stats)
        }
        Statement::Repeat(repeat) => {
            clean_block(&mut repeat.block.lock(), true, allow_continue, stats)
        }
        Statement::NumericFor(numeric_for) => {
            clean_block(&mut numeric_for.block.lock(), true, allow_continue, stats)
        }
        Statement::GenericFor(generic_for) => {
            clean_block(&mut generic_for.block.lock(), true, allow_continue, stats)
        }
        _ => {}
    }
}

/// Resolves a conditional whose condition is already a constant.
///
/// Structuring can leave a branch guarded by a literal, such as the
/// `if not true then continue end` a refined virtual edge produces. The
/// condition decides nothing, so the branch it selects belongs in the
/// surrounding block and the other is unreachable. Only a side-effect-free
/// condition qualifies, since dropping it must not drop work with it.
fn resolve_constant_conditions(block: &mut Block, stats: &mut ControlFlowCleanupStats) {
    let mut index = 0;
    while index < block.len() {
        let Some(r#if) = block[index].as_if() else {
            index += 1;
            continue;
        };
        let taken = match r#if.condition.clone().reduce_condition() {
            RValue::Literal(Literal::Boolean(taken)) => taken,
            _ => {
                index += 1;
                continue;
            }
        };
        if r#if.condition.has_side_effects() {
            index += 1;
            continue;
        }

        let selected = if taken {
            std::mem::take(&mut *r#if.then_block.lock()).0
        } else {
            std::mem::take(&mut *r#if.else_block.lock()).0
        };
        let taken_count = selected.len();
        block.0.splice(index..index + 1, selected);
        stats.removed_empty += 1;
        // Re-examine from here: the spliced statements have not been seen yet,
        // and an empty branch leaves the following statement at this index.
        index += taken_count;
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

fn recover_loop_tail_guard(block: &mut Block, stats: &mut ControlFlowCleanupStats) -> bool {
    let Some(last) = block.last_mut() else {
        return false;
    };
    let Some(r#if) = last.as_if_mut() else {
        return false;
    };

    let then_block = r#if.then_block.lock();
    let complex_body = then_block.len() >= 2
        || then_block.iter().any(|statement| {
            matches!(
                statement,
                Statement::If(_)
                    | Statement::While(_)
                    | Statement::Repeat(_)
                    | Statement::NumericFor(_)
                    | Statement::GenericFor(_)
            )
        });
    let should_preserve = !r#if.else_block.lock().is_empty()
        || then_block.is_empty()
        || block_terminates(&then_block)
        || !complex_body;
    drop(then_block);
    if should_preserve {
        return false;
    }

    invert_condition_in_place(&mut r#if.condition);
    let body = std::mem::take(&mut *r#if.then_block.lock()).0;
    r#if.then_block.lock().push(Continue {}.into());
    block.0.extend(body);
    stats.loop_guards += 1;
    true
}

/// Rewrites a trailing negated comparison back into an early return.
///
/// Branch inversion can leave a function ending in `if not (a < b) then body
/// end`. Authors spell that as a guard, and it is equivalent here: taking the
/// early return and falling off the end both leave the function with no
/// results. Only the last statement of a function body qualifies, since a
/// `return` inserted anywhere else would leave more than its own block.
///
/// The condition must be a negated ordering comparison. Those come from
/// inversion rather than from source, so undoing them cannot reshape an `if`
/// the author actually wrote.
fn recover_trailing_comparison_guard(
    block: &mut Block,
    stats: &mut ControlFlowCleanupStats,
) -> bool {
    let Some(r#if) = block.last_mut().and_then(Statement::as_if_mut) else {
        return false;
    };
    if !r#if.else_block.lock().is_empty() {
        return false;
    }
    let RValue::Unary(unary) = &r#if.condition else {
        return false;
    };
    if unary.operation != UnaryOperation::Not
        || !matches!(
            unary.value.as_ref(),
            RValue::Binary(binary) if matches!(
                binary.operation,
                BinaryOperation::LessThan
                    | BinaryOperation::LessThanOrEqual
                    | BinaryOperation::GreaterThan
                    | BinaryOperation::GreaterThanOrEqual
            )
        )
    {
        return false;
    }
    let then_block = r#if.then_block.lock();
    if then_block.is_empty() || block_terminates(&then_block) {
        return false;
    }
    drop(then_block);

    invert_condition_in_place(&mut r#if.condition);
    let body = std::mem::take(&mut *r#if.then_block.lock()).0;
    r#if.then_block.lock().push(Return::new(Vec::new()).into());
    block.0.extend(body);
    stats.flattened_guards += 1;
    true
}

fn clean_block(
    block: &mut Block,
    loop_body: bool,
    allow_continue: bool,
    stats: &mut ControlFlowCleanupStats,
) {
    loop {
        for statement in &mut block.0 {
            clean_statement(statement, allow_continue, stats);
        }
        resolve_constant_conditions(block, stats);
        normalize_empty_branches(block, stats);
        flatten_terminal_branches(block, stats);
        if !loop_body || !allow_continue || !recover_loop_tail_guard(block, stats) {
            break;
        }
    }
}

pub fn cleanup_control_flow(block: &mut Block) -> ControlFlowCleanupStats {
    let mut stats = ControlFlowCleanupStats::default();
    clean_block(block, false, true, &mut stats);
    while recover_trailing_comparison_guard(block, &mut stats) {}
    stats
}

pub fn cleanup_control_flow_lua51(block: &mut Block) -> ControlFlowCleanupStats {
    let mut stats = ControlFlowCleanupStats::default();
    clean_block(block, false, false, &mut stats);
    while recover_trailing_comparison_guard(block, &mut stats) {}
    stats
}

fn current_loop_controls(block: &Block) -> (bool, bool) {
    let mut has_continue = false;
    let mut has_break = false;
    for statement in &block.0 {
        let (nested_continue, nested_break) = match statement {
            Statement::Continue(_) => (true, false),
            Statement::Break(_) => (false, true),
            Statement::If(value) => {
                let then_controls = current_loop_controls(&value.then_block.lock());
                let else_controls = current_loop_controls(&value.else_block.lock());
                (
                    then_controls.0 || else_controls.0,
                    then_controls.1 || else_controls.1,
                )
            }
            Statement::Do(value) => current_loop_controls(&value.block.lock()),
            _ => (false, false),
        };
        has_continue |= nested_continue;
        has_break |= nested_break;
    }
    (has_continue, has_break)
}

fn rewrite_current_loop_controls(block: &mut Block, break_flag: Option<&RcLocal>) {
    let mut rewritten = Vec::with_capacity(block.len());
    for mut statement in std::mem::take(&mut block.0) {
        match &mut statement {
            Statement::Continue(_) => rewritten.push(crate::Break {}.into()),
            Statement::Break(_) => {
                if let Some(flag) = break_flag {
                    rewritten.push(
                        crate::Assign::new(
                            vec![flag.clone().into()],
                            vec![Literal::Boolean(true).into()],
                        )
                        .into(),
                    );
                }
                rewritten.push(statement);
            }
            Statement::If(value) => {
                rewrite_current_loop_controls(&mut value.then_block.lock(), break_flag);
                rewrite_current_loop_controls(&mut value.else_block.lock(), break_flag);
                rewritten.push(statement);
            }
            Statement::Do(value) => {
                rewrite_current_loop_controls(&mut value.block.lock(), break_flag);
                rewritten.push(statement);
            }
            _ => rewritten.push(statement),
        }
    }
    block.0 = rewritten;
}

fn repeat_condition_declaration_boundary(block: &Block, condition: &RValue) -> Option<usize> {
    let condition_locals = condition
        .values_read()
        .into_iter()
        .cloned()
        .collect::<rustc_hash::FxHashSet<_>>();

    block
        .iter()
        .enumerate()
        .filter_map(|(index, statement)| {
            statement
                .as_assign()
                .filter(|assign| {
                    assign.prefix
                        && assign
                            .left
                            .iter()
                            .filter_map(|value| value.as_local())
                            .any(|local| condition_locals.contains(local))
                })
                .map(|_| index)
        })
        .last()
}

fn hoist_repeat_condition_declarations(block: &mut Block, condition: &RValue) -> Vec<Statement> {
    let condition_locals = condition
        .values_read()
        .into_iter()
        .cloned()
        .collect::<rustc_hash::FxHashSet<_>>();
    let mut declarations = Vec::new();
    let mut remove = Vec::new();
    for (index, statement) in block.0.iter_mut().enumerate() {
        let Some(assign) = statement.as_assign_mut() else {
            continue;
        };
        if !assign.prefix
            || !assign
                .left
                .iter()
                .filter_map(|value| value.as_local())
                .any(|local| condition_locals.contains(local))
        {
            continue;
        }

        let declared = assign
            .left
            .iter()
            .filter_map(|value| value.as_local().cloned())
            .collect::<Vec<_>>();
        if declared.is_empty() {
            continue;
        }
        for local in &declared {
            local.0.0.lock().0 = Some(format!("__lua51_repeat_local_{}", local.id()));
        }
        let mut declaration = crate::Assign::new(
            declared.into_iter().map(crate::LValue::Local).collect(),
            Vec::new(),
        );
        declaration.prefix = true;
        declarations.push(declaration.into());
        if assign.right.is_empty() {
            remove.push(index);
        } else {
            assign.prefix = false;
        }
    }
    for index in remove.into_iter().rev() {
        block.remove(index);
    }
    declarations
}

fn lower_lua51_current_loop_body(block: &mut Block) {
    let (has_continue, has_break) = current_loop_controls(block);
    if !has_continue {
        return;
    }

    let break_flag = has_break.then(RcLocal::default);
    rewrite_current_loop_controls(block, break_flag.as_ref());
    let wrapped = crate::Repeat::new(Literal::Boolean(true).into(), std::mem::take(block));
    if let Some(flag) = break_flag {
        let mut declaration = crate::Assign::new(
            vec![flag.clone().into()],
            vec![Literal::Boolean(false).into()],
        );
        declaration.prefix = true;
        block.0.push(declaration.into());
        block.0.push(wrapped.into());
        block.0.push(
            crate::If::new(
                flag.into(),
                Block(vec![crate::Break {}.into()]),
                Block::default(),
            )
            .into(),
        );
    } else {
        block.0.push(wrapped.into());
    }
}

fn lower_lua51_loop_body(block: &mut Block, repeat_condition: Option<&RValue>) {
    lower_lua51_continues_nested(block);
    if !current_loop_controls(block).0 {
        return;
    }

    if let Some(boundary) = repeat_condition
        .and_then(|condition| repeat_condition_declaration_boundary(block, condition))
    {
        let prefix = Block(block.0[..=boundary].to_vec());
        if current_loop_controls(&prefix).0 {
            let mut declarations = hoist_repeat_condition_declarations(
                block,
                repeat_condition.expect("repeat declaration boundary requires a condition"),
            );
            lower_lua51_current_loop_body(block);
            declarations.append(&mut block.0);
            block.0 = declarations;
            return;
        }

        let mut suffix = Block(block.0.split_off(boundary + 1));
        lower_lua51_current_loop_body(&mut suffix);
        block.0.extend(suffix.0);
    } else {
        lower_lua51_current_loop_body(block);
    }
}

fn lower_lua51_continues_nested(block: &mut Block) {
    for statement in &mut block.0 {
        statement.traverse_rvalues(&mut |value| {
            if let RValue::Closure(closure) = value {
                lower_lua51_continues(&mut closure.function.lock().body);
            }
        });
        match statement {
            Statement::If(value) => {
                lower_lua51_continues_nested(&mut value.then_block.lock());
                lower_lua51_continues_nested(&mut value.else_block.lock());
            }
            Statement::Do(value) => lower_lua51_continues_nested(&mut value.block.lock()),
            Statement::While(value) => lower_lua51_loop_body(&mut value.block.lock(), None),
            Statement::Repeat(value) => {
                lower_lua51_loop_body(&mut value.block.lock(), Some(&value.condition))
            }
            Statement::NumericFor(value) => lower_lua51_loop_body(&mut value.block.lock(), None),
            Statement::GenericFor(value) => lower_lua51_loop_body(&mut value.block.lock(), None),
            _ => {}
        }
    }
}

pub fn lower_lua51_continues(block: &mut Block) {
    lower_lua51_continues_nested(block);
}

#[cfg(test)]
mod tests {
    use crate::{
        Assign, Binary, BinaryOperation, Block, Call, Global, If, Index, LValue, Literal, Local,
        Break, Continue, RValue, RcLocal, Repeat, Return, Statement, Unary, UnaryOperation, While,
    };

    use super::{cleanup_control_flow, cleanup_control_flow_lua51};

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
    fn trailing_negated_comparison_becomes_early_return() {
        let count = local("count");
        let value = local("value");
        let mut block = Block(vec![
            If::new(
                Unary::new(
                    Binary::new(
                        count.clone().into(),
                        Literal::Number(3.0).into(),
                        BinaryOperation::LessThan,
                    )
                    .into(),
                    UnaryOperation::Not,
                )
                .into(),
                Block(vec![assign(&value, 1.0)]),
                Block::default(),
            )
            .into(),
        ]);

        cleanup_control_flow(&mut block);

        assert_eq!(block.len(), 2);
        let guard = block[0].as_if().unwrap();
        assert!(matches!(
            &guard.condition,
            RValue::Binary(binary) if binary.operation == BinaryOperation::LessThan
        ));
        assert!(guard.then_block.lock()[0].as_return().is_some());
        assert!(block[1].as_assign().is_some());
    }

    #[test]
    fn trailing_negated_equality_keeps_its_shape() {
        // `not (a == b)` reduces to `a ~= b` on its own, so a surviving
        // negated equality came from the source and must not be reshaped.
        let left = local("left");
        let value = local("value");
        let mut block = Block(vec![
            If::new(
                Unary::new(
                    Binary::new(
                        left.clone().into(),
                        Literal::Number(3.0).into(),
                        BinaryOperation::Equal,
                    )
                    .into(),
                    UnaryOperation::Not,
                )
                .into(),
                Block(vec![assign(&value, 1.0)]),
                Block::default(),
            )
            .into(),
        ]);

        cleanup_control_flow(&mut block);

        assert_eq!(block.len(), 1);
        assert!(block[0].as_if().is_some());
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
        let indexed = Index::new(
            Global::from("object").into(),
            Literal::String(b"enabled".to_vec()).into(),
        );
        let compared = Binary::new(
            Global::from("left").into(),
            Global::from("right").into(),
            BinaryOperation::Equal,
        );
        let mut block = Block(vec![
            If::new(condition.into(), Block::default(), Block::default()).into(),
            If::new(effectful.into(), Block::default(), Block::default()).into(),
            If::new(indexed.into(), Block::default(), Block::default()).into(),
            If::new(compared.into(), Block::default(), Block::default()).into(),
        ]);

        let stats = cleanup_control_flow(&mut block);

        assert_eq!(stats.removed_empty, 1);
        assert_eq!(block.len(), 3);
        assert!(block.iter().all(|statement| statement.as_if().is_some()));
    }

    #[test]
    fn converts_last_loop_if_into_continue_guard() {
        let enabled = local("enabled");
        let value = local("value");
        let loop_body = Block(vec![
            If::new(
                enabled.clone().into(),
                Block(vec![assign(&value, 1.0), assign(&value, 2.0)]),
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
        assert_eq!(loop_body.len(), 3);
        let guard = loop_body[0].as_if().unwrap();
        assert!(guard.then_block.lock()[0].as_continue().is_some());
        assert!(matches!(&guard.condition, RValue::Unary(unary)
                if unary.operation == UnaryOperation::Not
                    && matches!(unary.value.as_ref(), RValue::Local(local) if local == &enabled)));
        assert!(loop_body[1].as_assign().is_some());
        assert!(loop_body[2].as_assign().is_some());
    }

    #[test]
    fn lua51_cleanup_does_not_create_continue() {
        let enabled = local("enabled");
        let value = local("value");
        let loop_body = Block(vec![
            If::new(
                enabled.into(),
                Block(vec![assign(&value, 1.0), assign(&value, 2.0)]),
                Block::default(),
            )
            .into(),
        ]);
        let mut block = Block(vec![
            While::new(Literal::Boolean(true).into(), loop_body).into(),
        ]);

        let stats = cleanup_control_flow_lua51(&mut block);
        let loop_body = block[0].as_while().unwrap().block.lock();

        assert_eq!(stats.loop_guards, 0);
        assert!(loop_body[0].as_if().is_some());
    }

    #[test]
    fn lua51_lowering_preserves_continue_and_break_meanings() {
        let skip = local("skip");
        let stop = local("stop");
        let loop_body = Block(vec![
            If::new(
                skip.into(),
                Block(vec![Continue {}.into()]),
                Block::default(),
            )
            .into(),
            If::new(
                stop.into(),
                Block(vec![Break {}.into()]),
                Block::default(),
            )
            .into(),
        ]);
        let mut block = Block(vec![
            While::new(Literal::Boolean(true).into(), loop_body).into(),
        ]);

        super::lower_lua51_continues(&mut block);
        let source = crate::format_lua51(&block);

        assert!(!source.contains("continue"), "{source}");
        assert!(source.contains("repeat"), "{source}");
        assert!(source.contains("until true"), "{source}");
    }

    #[test]
    fn lua51_repeat_continue_keeps_body_local_visible_to_condition() {
        let done = local("done");
        let declaration = Assign::new(
            vec![done.clone().into()],
            vec![Literal::Boolean(false).into()],
        );
        let block = Block(vec![
            Repeat::new(
                done.clone().into(),
                Block(vec![declaration.into(), Continue {}.into()]),
            )
            .into(),
        ]);
        let block = triomphe::Arc::new(parking_lot::Mutex::new(block));
        crate::local_declarations::LocalDeclarer::default()
            .declare_locals(triomphe::Arc::clone(&block), &Default::default());
        let mut block = triomphe::Arc::try_unwrap(block).unwrap().into_inner();

        super::lower_lua51_continues(&mut block);

        assert!(crate::validate_bindings(&block, &Default::default()).is_ok());
        let source = crate::format_lua51(&block);
        assert!(!source.contains("continue"), "{source}");
        assert!(source.contains("local done"), "{source}");
        assert!(source.contains("until done"), "{source}");
    }

    #[test]
    fn lua51_repeat_continue_preserves_declaration_initializer_scope() {
        let done = local("done");
        let declaration = Assign::new(
            vec![done.clone().into()],
            vec![Global::from("done").into()],
        );
        let block = Block(vec![
            Repeat::new(
                done.clone().into(),
                Block(vec![declaration.into(), Continue {}.into()]),
            )
            .into(),
        ]);
        let block = triomphe::Arc::new(parking_lot::Mutex::new(block));
        crate::local_declarations::LocalDeclarer::default()
            .declare_locals(triomphe::Arc::clone(&block), &Default::default());
        let mut block = triomphe::Arc::try_unwrap(block).unwrap().into_inner();

        super::lower_lua51_continues(&mut block);

        assert!(crate::validate_bindings(&block, &Default::default()).is_ok());
        let repeat = block[0].as_repeat().unwrap();
        let body = repeat.block.lock();
        let declaration = body[0].as_assign().expect("condition local declaration");
        assert!(declaration.prefix);
        assert!(matches!(declaration.right.as_slice(), [RValue::Global(_)]));
        drop(body);
        let source = crate::format_lua51(&block);
        assert!(!source.contains("continue"), "{source}");
        assert!(source.contains("local done = done"), "{source}");
    }

    #[test]
    fn lua51_repeat_continue_before_condition_local_is_lowered() {
        let skip = local("skip");
        let done = local("done");
        let declaration = Assign::new(
            vec![done.clone().into()],
            vec![Global::from("done").into()],
        );
        let block = Block(vec![
            Repeat::new(
                done.clone().into(),
                Block(vec![
                    If::new(
                        skip.into(),
                        Block(vec![Continue {}.into()]),
                        Block::default(),
                    )
                    .into(),
                    declaration.into(),
                ]),
            )
            .into(),
        ]);
        let block = triomphe::Arc::new(parking_lot::Mutex::new(block));
        crate::local_declarations::LocalDeclarer::default()
            .declare_locals(triomphe::Arc::clone(&block), &Default::default());
        let mut block = triomphe::Arc::try_unwrap(block).unwrap().into_inner();

        super::lower_lua51_continues(&mut block);

        assert!(crate::validate_bindings(&block, &Default::default()).is_ok());
        let source = crate::format_lua51(&block);
        assert!(!source.contains("continue"), "{source}");
        assert!(source.contains("= done"), "{source}");
        assert!(source.contains("until __lua51_repeat_local_"), "{source}");
    }

    #[test]
    fn lua51_repeat_continue_before_bare_condition_local_formats() {
        let skip = local("skip");
        let done = local("done");
        let declaration = Assign::new(vec![done.clone().into()], Vec::new());
        let block = Block(vec![
            Repeat::new(
                done.clone().into(),
                Block(vec![
                    If::new(
                        skip.into(),
                        Block(vec![Continue {}.into()]),
                        Block::default(),
                    )
                    .into(),
                    declaration.into(),
                ]),
            )
            .into(),
        ]);
        let block = triomphe::Arc::new(parking_lot::Mutex::new(block));
        crate::local_declarations::LocalDeclarer::default()
            .declare_locals(triomphe::Arc::clone(&block), &Default::default());
        let mut block = triomphe::Arc::try_unwrap(block).unwrap().into_inner();

        super::lower_lua51_continues(&mut block);

        assert!(crate::validate_bindings(&block, &Default::default()).is_ok());
        let source = crate::format_lua51(&block);
        assert!(!source.contains("continue"), "{source}");
        assert!(source.contains("local __lua51_repeat_local_"), "{source}");
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
                Block(vec![assign(&value, 1.0), assign(&value, 2.0)]),
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

    #[test]
    fn keeps_simple_loop_tail_conditional() {
        let condition = local("condition");
        let value = local("value");
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

        let stats = cleanup_control_flow(&mut block);
        let loop_body = block[0].as_while().unwrap().block.lock();

        assert_eq!(stats.loop_guards, 0);
        assert_eq!(loop_body.len(), 1);
        assert!(loop_body[0].as_if().is_some());
    }

    #[test]
    fn cleans_nested_loop_tails_to_a_fixed_point() {
        let outer = local("outer");
        let inner = local("inner");
        let value = local("value");
        let nested = If::new(
            inner.into(),
            Block(vec![assign(&value, 2.0), assign(&value, 3.0)]),
            Block::default(),
        );
        let loop_body = Block(vec![
            If::new(
                outer.into(),
                Block(vec![assign(&value, 1.0), nested.into()]),
                Block::default(),
            )
            .into(),
        ]);
        let mut block = Block(vec![
            While::new(Literal::Boolean(true).into(), loop_body).into(),
        ]);

        let stats = cleanup_control_flow(&mut block);
        let loop_body = block[0].as_while().unwrap().block.lock();

        assert_eq!(stats.loop_guards, 2);
        assert_eq!(loop_body.len(), 5);
        assert!(
            loop_body[0].as_if().unwrap().then_block.lock()[0]
                .as_continue()
                .is_some()
        );
        assert!(
            loop_body[2].as_if().unwrap().then_block.lock()[0]
                .as_continue()
                .is_some()
        );
    }
}
