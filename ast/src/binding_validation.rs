use std::{error::Error, fmt};

use rustc_hash::FxHashSet;

use crate::{Block, LocalRw, RValue, RcLocal, Statement};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindingAccess {
    Read,
    Write,
    Declaration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindingResolutionError {
    pub local: String,
    pub access: BindingAccess,
    pub statement: usize,
}

impl fmt::Display for BindingResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:?} of local {} does not resolve in lexical scope at statement {}",
            self.access, self.local, self.statement
        )
    }
}

impl Error for BindingResolutionError {}

struct BindingValidator;

impl BindingValidator {
    fn error(local: &RcLocal, access: BindingAccess, statement: usize) -> BindingResolutionError {
        BindingResolutionError {
            local: local.to_string(),
            access,
            statement,
        }
    }

    fn require_visible<'a>(
        values: impl IntoIterator<Item = &'a RcLocal>,
        visible: &FxHashSet<RcLocal>,
        access: BindingAccess,
        statement: usize,
    ) -> Result<(), BindingResolutionError> {
        for local in values {
            if !visible.contains(local) {
                return Err(Self::error(local, access, statement));
            }
        }
        Ok(())
    }

    fn validate_assign(
        assign: &crate::Assign,
        visible: &mut FxHashSet<RcLocal>,
        statement: usize,
    ) -> Result<(), BindingResolutionError> {
        if !assign.prefix {
            Self::require_visible(
                assign.values_read(),
                visible,
                BindingAccess::Read,
                statement,
            )?;
            return Self::require_visible(
                assign.values_written(),
                visible,
                BindingAccess::Write,
                statement,
            );
        }

        let declarations = assign
            .left
            .iter()
            .filter_map(|value| value.as_local())
            .cloned()
            .collect::<FxHashSet<_>>();

        for value in &assign.left {
            Self::require_visible(value.values_read(), visible, BindingAccess::Read, statement)?;
        }
        for value in &assign.right {
            for local in value.values_read() {
                let recursive_local_function =
                    matches!(value, RValue::Closure(_)) && declarations.contains(local);
                if !visible.contains(local) && !recursive_local_function {
                    return Err(Self::error(local, BindingAccess::Read, statement));
                }
            }
        }

        for local in declarations {
            if !visible.insert(local.clone()) {
                return Err(Self::error(&local, BindingAccess::Declaration, statement));
            }
        }
        Ok(())
    }

    fn validate_block(
        block: &Block,
        visible: &mut FxHashSet<RcLocal>,
    ) -> Result<(), BindingResolutionError> {
        for (statement_index, statement) in block.iter().enumerate() {
            match statement {
                Statement::Assign(assign) => {
                    Self::validate_assign(assign, visible, statement_index)?;
                }
                Statement::Class(class) => {
                    if !visible.insert(class.target.clone()) {
                        return Err(Self::error(
                            &class.target,
                            BindingAccess::Declaration,
                            statement_index,
                        ));
                    }
                    Self::require_visible(
                        class.values_read(),
                        visible,
                        BindingAccess::Read,
                        statement_index,
                    )?;
                }
                Statement::If(if_) => {
                    Self::require_visible(
                        if_.condition.values_read(),
                        visible,
                        BindingAccess::Read,
                        statement_index,
                    )?;
                    Self::validate_block(&if_.then_block.lock(), &mut visible.clone())?;
                    Self::validate_block(&if_.else_block.lock(), &mut visible.clone())?;
                }
                Statement::Do(do_) => {
                    Self::validate_block(&do_.block.lock(), &mut visible.clone())?;
                }
                Statement::While(while_) => {
                    Self::require_visible(
                        while_.condition.values_read(),
                        visible,
                        BindingAccess::Read,
                        statement_index,
                    )?;
                    Self::validate_block(&while_.block.lock(), &mut visible.clone())?;
                }
                Statement::Repeat(repeat) => {
                    let mut body_visible = visible.clone();
                    Self::validate_block(&repeat.block.lock(), &mut body_visible)?;
                    Self::require_visible(
                        repeat.condition.values_read(),
                        &body_visible,
                        BindingAccess::Read,
                        statement_index,
                    )?;
                }
                Statement::NumericFor(for_) => {
                    Self::require_visible(
                        for_.values_read(),
                        visible,
                        BindingAccess::Read,
                        statement_index,
                    )?;
                    let mut body_visible = visible.clone();
                    if !body_visible.insert(for_.counter.clone()) {
                        return Err(Self::error(
                            &for_.counter,
                            BindingAccess::Declaration,
                            statement_index,
                        ));
                    }
                    Self::validate_block(&for_.block.lock(), &mut body_visible)?;
                }
                Statement::GenericFor(for_) => {
                    Self::require_visible(
                        for_.values_read(),
                        visible,
                        BindingAccess::Read,
                        statement_index,
                    )?;
                    let mut body_visible = visible.clone();
                    for local in &for_.res_locals {
                        if !body_visible.insert(local.clone()) {
                            return Err(Self::error(
                                local,
                                BindingAccess::Declaration,
                                statement_index,
                            ));
                        }
                    }
                    Self::validate_block(&for_.block.lock(), &mut body_visible)?;
                }
                _ => {
                    Self::require_visible(
                        statement.values_read(),
                        visible,
                        BindingAccess::Read,
                        statement_index,
                    )?;
                    Self::require_visible(
                        statement.values_written(),
                        visible,
                        BindingAccess::Write,
                        statement_index,
                    )?;
                }
            }
        }
        Ok(())
    }
}

pub fn validate_bindings(
    block: &Block,
    initially_visible: &FxHashSet<RcLocal>,
) -> Result<(), BindingResolutionError> {
    BindingValidator::validate_block(block, &mut initially_visible.clone())
}

#[cfg(test)]
mod tests {
    use rustc_hash::FxHashSet;

    use super::{BindingAccess, validate_bindings};
    use crate::{Assign, Block, Local, RValue, RcLocal, Repeat, Return};

    fn local(name: &str) -> RcLocal {
        RcLocal::new(Local::new(Some(name.to_owned())))
    }

    #[test]
    fn repeat_body_declaration_is_visible_to_condition_only() {
        let local = local("inside");
        let repeat = Repeat::new(
            RValue::Local(local.clone()),
            Block(vec![{
                let mut declaration = Assign::new(
                    vec![local.clone().into()],
                    vec![crate::Literal::Boolean(true).into()],
                );
                declaration.prefix = true;
                declaration.into()
            }]),
        );
        let valid = Block(vec![repeat.clone().into()]);
        assert_eq!(validate_bindings(&valid, &FxHashSet::default()), Ok(()));

        let invalid = Block(vec![
            repeat.into(),
            Return::new(vec![local.clone().into()]).into(),
        ]);
        let error = validate_bindings(&invalid, &FxHashSet::default()).unwrap_err();
        assert_eq!(error.access, BindingAccess::Read);
        assert_eq!(error.local, "inside");
    }

    #[test]
    fn undeclared_capture_is_rejected_but_recursive_local_function_is_allowed() {
        let captured = local("captured");
        let closure_function = crate::Function::default();
        let closure = crate::Closure {
            function: by_address::ByAddress(triomphe::Arc::new(parking_lot::Mutex::new(
                closure_function,
            ))),
            upvalues: vec![crate::Upvalue::Ref(captured.clone())],
        };
        let invalid = Block(vec![Return::new(vec![closure.clone().into()]).into()]);
        assert!(validate_bindings(&invalid, &FxHashSet::default()).is_err());

        let mut declaration = Assign::new(vec![captured.into()], vec![RValue::Closure(closure)]);
        declaration.prefix = true;
        assert_eq!(
            validate_bindings(&Block(vec![declaration.into()]), &FxHashSet::default()),
            Ok(())
        );
    }
}
