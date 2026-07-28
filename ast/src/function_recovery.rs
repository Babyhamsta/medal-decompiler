use crate::{Block, LValue, Literal, RValue, RcLocal, Statement, Traverse, formatter::Formatter};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FunctionRecoveryStats {
    pub local_functions: usize,
    pub methods: usize,
}

fn receiver_score_rvalue(value: &RValue, receiver: &RcLocal) -> usize {
    let direct_score = match value {
        RValue::MethodCall(method_call) if matches!(method_call.value.as_ref(), RValue::Local(local) if local == receiver) => {
            2
        }
        RValue::Index(index) if matches!(index.left.as_ref(), RValue::Local(local) if local == receiver) => {
            1
        }
        _ => 0,
    };
    direct_score
        + value
            .rvalues()
            .into_iter()
            .map(|child| receiver_score_rvalue(child, receiver))
            .sum::<usize>()
}

fn receiver_score_lvalue(value: &LValue, receiver: &RcLocal) -> usize {
    match value {
        LValue::Index(index) => {
            let direct_score = usize::from(
                matches!(index.left.as_ref(), RValue::Local(local) if local == receiver),
            ) * 2;
            direct_score
                + receiver_score_rvalue(&index.left, receiver)
                + receiver_score_rvalue(&index.right, receiver)
        }
        _ => 0,
    }
}

fn receiver_score_block(block: &Block, receiver: &RcLocal) -> usize {
    block
        .iter()
        .map(|statement| {
            let direct = statement
                .rvalues()
                .into_iter()
                .map(|value| receiver_score_rvalue(value, receiver))
                .sum::<usize>()
                + match statement {
                    Statement::Assign(assign) => assign
                        .left
                        .iter()
                        .map(|value| receiver_score_lvalue(value, receiver))
                        .sum(),
                    _ => 0,
                };
            let nested = match statement {
                Statement::If(r#if) => {
                    receiver_score_block(&r#if.then_block.lock(), receiver)
                        + receiver_score_block(&r#if.else_block.lock(), receiver)
                }
                Statement::While(r#while) => receiver_score_block(&r#while.block.lock(), receiver),
                Statement::Repeat(repeat) => receiver_score_block(&repeat.block.lock(), receiver),
                Statement::NumericFor(numeric_for) => {
                    receiver_score_block(&numeric_for.block.lock(), receiver)
                }
                Statement::GenericFor(generic_for) => {
                    receiver_score_block(&generic_for.block.lock(), receiver)
                }
                _ => 0,
            };
            direct + nested
        })
        .sum()
}

fn is_nil_declaration(statement: &Statement) -> Option<RcLocal> {
    let assign = statement.as_assign()?;
    if !assign.prefix || assign.left.len() != 1 {
        return None;
    }
    let local = assign.left[0].as_local()?.clone();
    if assign.right.is_empty() || matches!(assign.right.as_slice(), [RValue::Literal(Literal::Nil)])
    {
        Some(local)
    } else {
        None
    }
}

fn collapse_local_functions(block: &mut Block) -> usize {
    let mut recovered = 0;
    let mut index = 0;
    while index + 1 < block.len() {
        let Some(local) = is_nil_declaration(&block[index]) else {
            index += 1;
            continue;
        };
        let is_closure_assignment = block[index + 1].as_assign().is_some_and(|assign| {
            !assign.prefix
                && !assign.parallel
                && assign.left.as_slice() == [LValue::Local(local.clone())]
                && matches!(
                    assign.right.as_slice(),
                    [RValue::Closure(closure)] if closure.function.lock().name.is_some()
                )
        });
        if !is_closure_assignment {
            index += 1;
            continue;
        }

        block.remove(index);
        block[index].as_assign_mut().unwrap().prefix = true;
        recovered += 1;
    }
    recovered
}

fn mark_method(assign: &mut crate::Assign) -> bool {
    if assign.prefix || assign.parallel || assign.left.len() != 1 || assign.right.len() != 1 {
        return false;
    }
    let LValue::Index(target) = &assign.left[0] else {
        return false;
    };
    let RValue::Literal(Literal::String(method)) = target.right.as_ref() else {
        return false;
    };
    if !Formatter::<String>::is_valid_name(method) {
        return false;
    }
    let RValue::Closure(closure) = &assign.right[0] else {
        return false;
    };

    let mut function = closure.function.lock();
    if function.name.as_deref().map(str::as_bytes) != Some(method.as_slice()) {
        return false;
    }
    let Some(receiver) = function.parameters.first().cloned() else {
        return false;
    };
    if receiver_score_block(&function.body, &receiver) < 2 {
        return false;
    }

    receiver.0.0.lock().0 = Some("self".to_owned());
    function.is_method = true;
    true
}

fn recover_block(block: &mut Block, stats: &mut FunctionRecoveryStats) {
    stats.local_functions += collapse_local_functions(block);

    for statement in &mut block.0 {
        if let Some(assign) = statement.as_assign_mut() {
            stats.methods += usize::from(mark_method(assign));
        }

        statement.traverse_rvalues(&mut |value| {
            if let RValue::Closure(closure) = value {
                recover_block(&mut closure.function.lock().body, stats);
            }
        });

        match statement {
            Statement::If(r#if) => {
                recover_block(&mut r#if.then_block.lock(), stats);
                recover_block(&mut r#if.else_block.lock(), stats);
            }
            Statement::While(r#while) => recover_block(&mut r#while.block.lock(), stats),
            Statement::Repeat(repeat) => recover_block(&mut repeat.block.lock(), stats),
            Statement::NumericFor(numeric_for) => {
                recover_block(&mut numeric_for.block.lock(), stats)
            }
            Statement::GenericFor(generic_for) => {
                recover_block(&mut generic_for.block.lock(), stats)
            }
            _ => {}
        }
    }
}

pub fn recover_function_syntax(block: &mut Block) -> FunctionRecoveryStats {
    let mut stats = FunctionRecoveryStats::default();
    recover_block(block, &mut stats);
    stats
}

#[cfg(test)]
mod tests {
    use by_address::ByAddress;
    use parking_lot::Mutex;
    use triomphe::Arc;

    use crate::{
        Assign, Block, Closure, Function, Index, LValue, Literal, Local, RValue, RcLocal, Return,
    };

    use super::recover_function_syntax;

    fn local(name: &str) -> RcLocal {
        RcLocal::new(Local::new(Some(name.to_owned())))
    }

    fn closure(name: Option<&str>, parameters: Vec<RcLocal>, body: Block) -> RValue {
        let function = Arc::new(Mutex::new(Function {
            name: name.map(str::to_owned),
            parameters,
            body,
            ..Function::default()
        }));
        Closure {
            function: ByAddress(function),
            upvalues: Vec::new(),
        }
        .into()
    }

    #[test]
    fn recovers_adjacent_recursive_local_function() {
        let callback = local("callback");
        let body = Block(vec![Return::new(vec![callback.clone().into()]).into()]);
        let mut declaration = Assign::new(vec![callback.clone().into()], vec![Literal::Nil.into()]);
        declaration.prefix = true;
        let mut block = Block(vec![
            declaration.into(),
            Assign::new(
                vec![callback.clone().into()],
                vec![closure(Some("callback"), Vec::new(), body)],
            )
            .into(),
        ]);

        let stats = recover_function_syntax(&mut block);

        assert_eq!(stats.local_functions, 1);
        assert_eq!(block.len(), 1);
        assert!(block[0].as_assign().unwrap().prefix);
        assert!(block.to_string().starts_with("local function callback()"));
    }

    #[test]
    fn keeps_anonymous_recursive_assignment_after_declaration() {
        let callback = local("callback");
        let body = Block(vec![Return::new(vec![callback.clone().into()]).into()]);
        let mut declaration = Assign::new(vec![callback.clone().into()], vec![Literal::Nil.into()]);
        declaration.prefix = true;
        let mut block = Block(vec![
            declaration.into(),
            Assign::new(
                vec![callback.clone().into()],
                vec![closure(None, Vec::new(), body)],
            )
            .into(),
        ]);

        let stats = recover_function_syntax(&mut block);

        assert_eq!(stats.local_functions, 0);
        assert_eq!(block.len(), 2);
        assert_eq!(
            block.to_string(),
            "local callback = nil\ncallback = function()\n\treturn callback\nend"
        );
    }

    #[test]
    fn recovers_receiver_backed_method_declaration() {
        let object = local("Controller");
        let receiver = local("receiver");
        let value = local("value");
        let field = RValue::from(Literal::String(b"value".to_vec()));
        let receiver_field = Index::new(receiver.clone().into(), field.clone());
        let body = Block(vec![
            Assign::new(
                vec![LValue::Index(receiver_field.clone())],
                vec![value.clone().into()],
            )
            .into(),
            Return::new(vec![receiver_field.into()]).into(),
        ]);
        let target = Index::new(object.into(), Literal::String(b"update".to_vec()).into());
        let mut block = Block(vec![
            Assign::new(
                vec![LValue::Index(target)],
                vec![closure(Some("update"), vec![receiver.clone(), value], body)],
            )
            .into(),
        ]);

        let stats = recover_function_syntax(&mut block);
        let output = block.to_string();

        assert_eq!(stats.methods, 1);
        assert!(output.starts_with("function Controller:update(value)"));
        assert!(output.contains("self.value = value"));
    }

    #[test]
    fn keeps_static_function_when_first_parameter_is_not_a_receiver() {
        let module = local("Module");
        let value = local("value");
        let body = Block(vec![Return::new(vec![value.clone().into()]).into()]);
        let target = Index::new(module.into(), Literal::String(b"identity".to_vec()).into());
        let mut block = Block(vec![
            Assign::new(
                vec![LValue::Index(target)],
                vec![closure(Some("identity"), vec![value], body)],
            )
            .into(),
        ]);

        let stats = recover_function_syntax(&mut block);
        let output = block.to_string();

        assert_eq!(stats.methods, 0);
        assert!(output.starts_with("function Module.identity(value)"));
    }

    #[test]
    fn keeps_anonymous_callback_field_as_assignment() {
        let handlers = local("handlers");
        let context = local("context");
        let first = Index::new(
            context.clone().into(),
            Literal::String(b"first".to_vec()).into(),
        );
        let second = Index::new(
            context.clone().into(),
            Literal::String(b"second".to_vec()).into(),
        );
        let body = Block(vec![Return::new(vec![first.into(), second.into()]).into()]);
        let target = Index::new(handlers.into(), Literal::String(b"ready".to_vec()).into());
        let mut block = Block(vec![
            Assign::new(
                vec![LValue::Index(target)],
                vec![closure(None, vec![context], body)],
            )
            .into(),
        ]);

        let stats = recover_function_syntax(&mut block);
        let output = block.to_string();

        assert_eq!(stats.methods, 0);
        assert!(output.starts_with("handlers.ready = function(context)"));
    }
}
