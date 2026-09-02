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

struct BindingValidator {
    visible: FxHashSet<RcLocal>,
    scoped_declarations: Vec<RcLocal>,
}

impl BindingValidator {
    fn error(local: &RcLocal, access: BindingAccess, statement: usize) -> BindingResolutionError {
        BindingResolutionError {
            local: local.to_string(),
            access,
            statement,
        }
    }

    fn require_visible<'a>(
        &self,
        values: impl IntoIterator<Item = &'a RcLocal>,
        access: BindingAccess,
        statement: usize,
    ) -> Result<(), BindingResolutionError> {
        for local in values {
            if !self.visible.contains(local) {
                return Err(Self::error(local, access, statement));
            }
        }
        Ok(())
    }

    fn declare(&mut self, local: RcLocal, statement: usize) -> Result<(), BindingResolutionError> {
        if !self.visible.insert(local.clone()) {
            return Err(Self::error(&local, BindingAccess::Declaration, statement));
        }
        self.scoped_declarations.push(local);
        Ok(())
    }

    fn restore_scope(&mut self, marker: usize) {
        for local in self.scoped_declarations.drain(marker..) {
            self.visible.remove(&local);
        }
    }

    fn validate_child_block(&mut self, block: &Block) -> Result<(), BindingResolutionError> {
        let marker = self.scoped_declarations.len();
        let result = self.validate_block(block);
        self.restore_scope(marker);
        result
    }

    fn validate_assign(
        &mut self,
        assign: &crate::Assign,
        statement: usize,
    ) -> Result<(), BindingResolutionError> {
        if !assign.prefix {
            self.require_visible(assign.values_read(), BindingAccess::Read, statement)?;
            return self.require_visible(assign.values_written(), BindingAccess::Write, statement);
        }

        let declarations = assign
            .left
            .iter()
            .filter_map(|value| value.as_local())
            .cloned()
            .collect::<FxHashSet<_>>();

        for value in &assign.left {
            self.require_visible(value.values_read(), BindingAccess::Read, statement)?;
        }
        for value in &assign.right {
            for local in value.values_read() {
                let recursive_local_function =
                    matches!(value, RValue::Closure(_)) && declarations.contains(local);
                if !self.visible.contains(local) && !recursive_local_function {
                    return Err(Self::error(local, BindingAccess::Read, statement));
                }
            }
        }

        for local in declarations {
            self.declare(local, statement)?;
        }
        Ok(())
    }

    fn validate_block(&mut self, block: &Block) -> Result<(), BindingResolutionError> {
        for (statement_index, statement) in block.iter().enumerate() {
            match statement {
                Statement::Assign(assign) => {
                    self.validate_assign(assign, statement_index)?;
                }
                Statement::Class(class) => {
                    self.declare(class.target.clone(), statement_index)?;
                    self.require_visible(
                        class.values_read(),
                        BindingAccess::Read,
                        statement_index,
                    )?;
                }
                Statement::If(if_) => {
                    self.require_visible(
                        if_.condition.values_read(),
                        BindingAccess::Read,
                        statement_index,
                    )?;
                    self.validate_child_block(&if_.then_block.lock())?;
                    self.validate_child_block(&if_.else_block.lock())?;
                }
                Statement::Do(do_) => {
                    self.validate_child_block(&do_.block.lock())?;
                }
                Statement::While(while_) => {
                    self.require_visible(
                        while_.condition.values_read(),
                        BindingAccess::Read,
                        statement_index,
                    )?;
                    self.validate_child_block(&while_.block.lock())?;
                }
                Statement::Repeat(repeat) => {
                    let marker = self.scoped_declarations.len();
                    let result = self.validate_block(&repeat.block.lock()).and_then(|()| {
                        self.require_visible(
                            repeat.condition.values_read(),
                            BindingAccess::Read,
                            statement_index,
                        )
                    });
                    self.restore_scope(marker);
                    result?;
                }
                Statement::NumericFor(for_) => {
                    self.require_visible(for_.values_read(), BindingAccess::Read, statement_index)?;
                    let marker = self.scoped_declarations.len();
                    let result = self
                        .declare(for_.counter.clone(), statement_index)
                        .and_then(|()| self.validate_block(&for_.block.lock()));
                    self.restore_scope(marker);
                    result?;
                }
                Statement::GenericFor(for_) => {
                    self.require_visible(for_.values_read(), BindingAccess::Read, statement_index)?;
                    let marker = self.scoped_declarations.len();
                    let result = (|| {
                        for local in &for_.res_locals {
                            self.declare(local.clone(), statement_index)?;
                        }
                        self.validate_block(&for_.block.lock())
                    })();
                    self.restore_scope(marker);
                    result?;
                }
                _ => {
                    self.require_visible(
                        statement.values_read(),
                        BindingAccess::Read,
                        statement_index,
                    )?;
                    self.require_visible(
                        statement.values_written(),
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
    BindingValidator {
        visible: initially_visible.clone(),
        scoped_declarations: Vec::new(),
    }
    .validate_block(block)
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
