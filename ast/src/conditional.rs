use smallvec::{smallvec};
use crate::{LocalRefs,LocalRefsMut,RValueRefs,RValueRefsMut};
use std::fmt;

use crate::{LocalRw, RValue, RcLocal, SideEffects, Traverse, formatter::Formatter};

#[derive(Debug, Clone, PartialEq)]
pub struct Conditional {
    pub condition: Box<RValue>,
    pub then_value: Box<RValue>,
    pub else_value: Box<RValue>,
}

impl Conditional {
    pub fn new(condition: RValue, then_value: RValue, else_value: RValue) -> Self {
        Self {
            condition: Box::new(condition),
            then_value: Box::new(then_value),
            else_value: Box::new(else_value),
        }
    }
}

impl Traverse for Conditional {
    fn rvalues_mut(&mut self) -> RValueRefsMut<'_> {
        smallvec![
            &mut *self.condition,
            &mut *self.then_value,
            &mut *self.else_value,
        ]
    }

    fn rvalues(&self) -> RValueRefs<'_> {
        smallvec![&*self.condition, &*self.then_value, &*self.else_value]
    }
}

impl SideEffects for Conditional {
    fn has_side_effects(&self) -> bool {
        self.condition.has_side_effects()
            || self.then_value.has_side_effects()
            || self.else_value.has_side_effects()
    }
}

impl LocalRw for Conditional {
    fn values_read(&self) -> LocalRefs<'_> {
        self.condition
            .values_read()
            .into_iter()
            .chain(self.then_value.values_read())
            .chain(self.else_value.values_read())
            .collect()
    }

    fn values_read_mut(&mut self) -> LocalRefsMut<'_> {
        self.condition
            .values_read_mut()
            .into_iter()
            .chain(self.then_value.values_read_mut())
            .chain(self.else_value.values_read_mut())
            .collect()
    }
}

impl fmt::Display for Conditional {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        Formatter {
            indentation_level: 0,
            indentation_mode: Default::default(),
            output: f,
        }
        .format_conditional(self)
    }
}

#[cfg(test)]
mod tests {
    use crate::{Call, Conditional, Global, Literal, Local, LocalRw, RValue, RcLocal, SideEffects};

    fn local(name: &str) -> RcLocal {
        RcLocal::new(Local::new(Some(name.to_owned())))
    }

    #[test]
    fn formats_conditional_expression() {
        let condition = local("condition");
        let expression = Conditional::new(
            condition.into(),
            Literal::String(b"yes".to_vec()).into(),
            Literal::String(b"no".to_vec()).into(),
        );

        assert_eq!(
            expression.to_string(),
            "if condition then \"yes\" else \"no\""
        );
    }

    #[test]
    fn formats_nested_else_as_elseif() {
        let inner = Conditional::new(
            local("second").into(),
            Literal::String(b"two".to_vec()).into(),
            Literal::String(b"other".to_vec()).into(),
        );
        let expression = Conditional::new(
            local("first").into(),
            Literal::String(b"one".to_vec()).into(),
            inner.into(),
        );

        assert_eq!(
            expression.to_string(),
            "if first then \"one\" elseif second then \"two\" else \"other\""
        );
    }

    #[test]
    fn exposes_all_reads_and_branch_effects() {
        let condition = local("condition");
        let then_value = local("then_value");
        let fallback = Call::new(Global::from("fallback").into(), Vec::new());
        let expression = Conditional::new(
            RValue::Local(condition.clone()),
            RValue::Local(then_value.clone()),
            fallback.into(),
        );

        assert_eq!(
            expression.values_read().as_slice(),
            [&condition, &then_value]
        );
        assert!(expression.has_side_effects());
    }
}
