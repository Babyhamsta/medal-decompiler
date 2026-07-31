//! Carries parameter names backwards to the arguments that feed them.
//!
//! A local passed as the first argument of `push_stack(stack, value)` is a
//! stack. The callee already states that; this pass moves the statement to
//! where a reader needs it.
//!
//! Names are written into each local's debug-name slot, so `name_locals`
//! picks them up through its existing path and applies its own uniqueness and
//! validity rules. This pass never renames a local that already has a name.

use rustc_hash::FxHashMap;

use crate::{
    Block, Call, Closure, LValue, RValue, RcLocal, Statement, Traverse, is_valid_identifier,
};

/// A name proposed for a local, or a marker that call sites disagreed.
enum Proposal {
    Single(String),
    Conflicting,
}

fn parameter_names(closure: &Closure) -> Vec<Option<String>> {
    closure
        .function
        .lock()
        .parameters
        .iter()
        .map(|parameter| {
            parameter
                .0
                .0
                .lock()
                .0
                .clone()
                .filter(|name| is_valid_identifier(name.as_bytes()))
        })
        .collect()
}

fn collect_callees(block: &Block, callees: &mut FxHashMap<RcLocal, Vec<Option<String>>>) {
    for statement in &block.0 {
        if let Statement::Assign(assign) = statement
            && let ([LValue::Local(target)], [RValue::Closure(closure)]) =
                (assign.left.as_slice(), assign.right.as_slice())
        {
            callees.insert(target.clone(), parameter_names(closure));
        }
        for_each_child_block(statement, &mut |child| collect_callees(child, callees));
    }
}

fn record_call(
    call: &Call,
    callees: &FxHashMap<RcLocal, Vec<Option<String>>>,
    proposals: &mut FxHashMap<RcLocal, Proposal>,
) {
    let RValue::Local(callee) = call.value.as_ref() else {
        return;
    };
    let Some(parameters) = callees.get(callee) else {
        return;
    };
    for (argument, parameter) in call.arguments.iter().zip(parameters) {
        let (RValue::Local(argument), Some(name)) = (argument, parameter) else {
            continue;
        };
        if argument.0.0.lock().0.is_some() {
            continue;
        }
        match proposals.get(argument) {
            None => {
                proposals.insert(argument.clone(), Proposal::Single(name.clone()));
            }
            Some(Proposal::Single(existing)) if existing == name => {}
            Some(Proposal::Single(_)) => {
                proposals.insert(argument.clone(), Proposal::Conflicting);
            }
            Some(Proposal::Conflicting) => {}
        }
    }
}

fn collect_proposals(
    block: &Block,
    callees: &FxHashMap<RcLocal, Vec<Option<String>>>,
    proposals: &mut FxHashMap<RcLocal, Proposal>,
) {
    for statement in &block.0 {
        if let Statement::Call(call) = statement {
            record_call(call, callees, proposals);
        }
        for value in statement.rvalues() {
            record_rvalue(value, callees, proposals);
        }
        for_each_child_block(statement, &mut |child| {
            collect_proposals(child, callees, proposals)
        });
    }
}

fn record_rvalue(
    value: &RValue,
    callees: &FxHashMap<RcLocal, Vec<Option<String>>>,
    proposals: &mut FxHashMap<RcLocal, Proposal>,
) {
    if let RValue::Call(call) | RValue::Select(crate::Select::Call(call)) = value {
        record_call(call, callees, proposals);
    }
    for child in value.rvalues() {
        record_rvalue(child, callees, proposals);
    }
}

/// Visits every block nested inside a statement, including closure bodies.
///
/// Closure bodies matter most: a helper is declared once at the top level
/// and called from inside other functions, so skipping closure bodies would
/// miss nearly every call site worth naming.
fn for_each_child_block(statement: &Statement, visit: &mut impl FnMut(&Block)) {
    match statement {
        Statement::If(r#if) => {
            visit(&r#if.then_block.lock());
            visit(&r#if.else_block.lock());
        }
        Statement::While(r#while) => visit(&r#while.block.lock()),
        Statement::Repeat(repeat) => visit(&repeat.block.lock()),
        Statement::NumericFor(numeric_for) => visit(&numeric_for.block.lock()),
        Statement::GenericFor(generic_for) => visit(&generic_for.block.lock()),
        _ => {}
    }

    for value in statement.rvalues() {
        visit_closure_bodies(value, visit);
    }
}

/// Visits the body of every closure reachable from an expression.
///
/// `Closure` holds `Arc<Mutex<Function>>` and the body is a plain `Block`
/// field inside it, so the lock is taken here and the borrow handed straight
/// to the visitor.
fn visit_closure_bodies(value: &RValue, visit: &mut impl FnMut(&Block)) {
    if let RValue::Closure(closure) = value {
        let function = closure.function.lock();
        visit(&function.body);
    }
    for child in value.rvalues() {
        visit_closure_bodies(child, visit);
    }
}

/// Names unnamed locals after the parameters they are passed to.
///
/// Runs before `name_locals`, which applies uniqueness and scoping to
/// whatever this leaves behind.
pub fn propagate_parameter_names(block: &mut Block) {
    let mut callees = FxHashMap::default();
    collect_callees(block, &mut callees);
    if callees.is_empty() {
        return;
    }

    let mut proposals = FxHashMap::default();
    collect_proposals(block, &callees, &mut proposals);

    for (local, proposal) in proposals {
        if let Proposal::Single(name) = proposal {
            let mut slot = local.0.0.lock();
            if slot.0.is_none() {
                slot.0 = Some(name);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use by_address::ByAddress;
    use parking_lot::Mutex;
    use triomphe::Arc;

    use super::*;
    use crate::{Assign, Closure, Function, Literal, Local, Table};

    fn local(name: Option<&str>) -> RcLocal {
        RcLocal::new(Local::new(name.map(str::to_owned)))
    }

    fn declaration(local: &RcLocal, value: RValue) -> Assign {
        let mut assign = Assign::new(vec![LValue::Local(local.clone())], vec![value]);
        assign.prefix = true;
        assign
    }

    #[test]
    fn argument_takes_the_name_of_the_parameter_it_feeds() {
        // local pushValue = function(stack, value) end
        // local anonymous = {}
        // pushValue(anonymous, 1)
        let stack_parameter = local(Some("stack"));
        let value_parameter = local(Some("value"));
        let callee = local(Some("pushValue"));
        let argument = local(None);

        let closure = Closure {
            function: ByAddress(Arc::new(Mutex::new(Function {
                name: None,
                parameters: vec![stack_parameter, value_parameter],
                is_variadic: false,
                is_method: false,
                body: Block::default(),
            }))),
            upvalues: Vec::new(),
        };

        let mut block = Block(vec![
            declaration(&callee, closure.into()).into(),
            declaration(&argument, Table::default().into()).into(),
            Statement::Call(Call::new(
                callee.clone().into(),
                vec![argument.clone().into(), Literal::Number(1.0).into()],
            )),
        ]);

        propagate_parameter_names(&mut block);

        assert_eq!(argument.0.0.lock().0.as_deref(), Some("stack"));
    }

    #[test]
    fn conflicting_call_sites_leave_the_local_unnamed() {
        let argument = local(None);

        let make = |parameter_name: &str| Closure {
            function: ByAddress(Arc::new(Mutex::new(Function {
                name: None,
                parameters: vec![local(Some(parameter_name))],
                is_variadic: false,
                is_method: false,
                body: Block::default(),
            }))),
            upvalues: Vec::new(),
        };

        let first_callee = local(Some("first"));
        let second_callee = local(Some("second"));

        let mut block = Block(vec![
            declaration(&first_callee, make("stack").into()).into(),
            declaration(&second_callee, make("registry").into()).into(),
            Statement::Call(Call::new(
                first_callee.clone().into(),
                vec![argument.clone().into()],
            )),
            Statement::Call(Call::new(
                second_callee.clone().into(),
                vec![argument.clone().into()],
            )),
        ]);

        propagate_parameter_names(&mut block);

        assert_eq!(argument.0.0.lock().0, None);
    }

    #[test]
    fn a_local_that_already_has_a_name_is_left_alone() {
        let callee = local(Some("pushValue"));
        let argument = local(Some("existing"));

        let closure = Closure {
            function: ByAddress(Arc::new(Mutex::new(Function {
                name: None,
                parameters: vec![local(Some("stack"))],
                is_variadic: false,
                is_method: false,
                body: Block::default(),
            }))),
            upvalues: Vec::new(),
        };

        let mut block = Block(vec![
            declaration(&callee, closure.into()).into(),
            Statement::Call(Call::new(
                callee.clone().into(),
                vec![argument.clone().into()],
            )),
        ]);

        propagate_parameter_names(&mut block);

        assert_eq!(argument.0.0.lock().0.as_deref(), Some("existing"));
    }

    #[test]
    fn a_call_inside_a_closure_body_still_proposes_a_name() {
        let stack_parameter = local(Some("stack"));
        let callee = local(Some("pushValue"));
        let argument = local(None);

        let helper = Closure {
            function: ByAddress(Arc::new(Mutex::new(Function {
                name: None,
                parameters: vec![stack_parameter],
                is_variadic: false,
                is_method: false,
                body: Block::default(),
            }))),
            upvalues: Vec::new(),
        };

        let caller = Closure {
            function: ByAddress(Arc::new(Mutex::new(Function {
                name: None,
                parameters: Vec::new(),
                is_variadic: false,
                is_method: false,
                body: Block(vec![
                    declaration(&argument, Table::default().into()).into(),
                    Statement::Call(Call::new(
                        callee.clone().into(),
                        vec![argument.clone().into()],
                    )),
                ]),
            }))),
            upvalues: Vec::new(),
        };

        let holder = local(Some("caller"));
        let mut block = Block(vec![
            declaration(&callee, helper.into()).into(),
            declaration(&holder, caller.into()).into(),
        ]);

        propagate_parameter_names(&mut block);

        assert_eq!(argument.0.0.lock().0.as_deref(), Some("stack"));
    }
}
