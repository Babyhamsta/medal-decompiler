use crate::{LocalRefs,LocalRefsMut,RValueRefs,RValueRefsMut};
use std::fmt;

use crate::{LocalRw, RcLocal, Traverse, formatter::Formatter, has_side_effects};

use super::RValue;

#[derive(Debug, PartialEq, Clone, Default)]
pub struct Return {
    pub values: Vec<RValue>,
}

has_side_effects!(Return);

impl Return {
    pub fn new(values: Vec<RValue>) -> Self {
        Self { values }
    }
}

impl Traverse for Return {
    fn rvalues_mut(&mut self) -> RValueRefsMut<'_> {
        self.values.iter_mut().collect()
    }

    fn rvalues(&self) -> RValueRefs<'_> {
        self.values.iter().collect()
    }
}

impl LocalRw for Return {
    fn values_read(&self) -> LocalRefs<'_> {
        self.values.iter().flat_map(|r| r.values_read()).collect()
    }

    fn values_read_mut(&mut self) -> LocalRefsMut<'_> {
        self.values
            .iter_mut()
            .flat_map(|r| r.values_read_mut())
            .collect()
    }
}

impl fmt::Display for Return {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        Formatter {
            indentation_level: 0,
            indentation_mode: Default::default(),
            output: f,
        }
        .format_return(self)
    }
}
