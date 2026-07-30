use smallvec::{smallvec};
use crate::{LocalRefs,LocalRefsMut,RValueRefs,RValueRefsMut};
use std::fmt;

use crate::{LocalRw, RValue, RcLocal, SideEffects, Traverse, formatter::Formatter};

#[derive(Debug, Clone, PartialEq)]
pub struct Class {
    pub target: RcLocal,
    pub source_name: String,
    pub properties: Vec<String>,
    pub methods: Vec<(String, RValue)>,
}

impl Class {
    pub fn new(target: RcLocal, source_name: String, properties: Vec<String>) -> Self {
        Self {
            target,
            source_name,
            properties,
            methods: Vec::new(),
        }
    }
}

impl LocalRw for Class {
    fn values_read(&self) -> LocalRefs<'_> {
        self.methods
            .iter()
            .flat_map(|(_, value)| value.values_read())
            .collect()
    }

    fn values_read_mut(&mut self) -> LocalRefsMut<'_> {
        self.methods
            .iter_mut()
            .flat_map(|(_, value)| value.values_read_mut())
            .collect()
    }

    fn values_written(&self) -> LocalRefs<'_> {
        smallvec![&self.target]
    }

    fn values_written_mut(&mut self) -> LocalRefsMut<'_> {
        smallvec![&mut self.target]
    }
}

impl Traverse for Class {
    fn rvalues_mut(&mut self) -> RValueRefsMut<'_> {
        self.methods.iter_mut().map(|(_, value)| value).collect()
    }

    fn rvalues(&self) -> RValueRefs<'_> {
        self.methods.iter().map(|(_, value)| value).collect()
    }
}

impl SideEffects for Class {
    fn has_side_effects(&self) -> bool {
        true
    }
}

impl fmt::Display for Class {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        Formatter {
            indentation_level: 0,
            indentation_mode: Default::default(),
            output: f,
        }
        .format_class(self)
    }
}
