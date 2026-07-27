use std::fmt;

use crate::{LocalRw, SideEffects, Traverse, formatter::Formatter};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GlobalOrigin {
    #[default]
    Dynamic,
    CompilerImport,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Clone)]
pub struct Global {
    name: Vec<u8>,
    origin: GlobalOrigin,
}

impl Global {
    pub fn new(name: Vec<u8>) -> Self {
        Self {
            name,
            origin: GlobalOrigin::Dynamic,
        }
    }

    pub fn compiler_import(name: Vec<u8>) -> Self {
        Self {
            name,
            origin: GlobalOrigin::CompilerImport,
        }
    }

    pub fn origin(&self) -> GlobalOrigin {
        self.origin
    }
}

impl LocalRw for Global {}

impl SideEffects for Global {
    fn has_side_effects(&self) -> bool {
        true
    }
}

impl Traverse for Global {}

impl<'a> From<&'a str> for Global {
    fn from(name: &'a str) -> Self {
        Self::new(name.into())
    }
}

impl fmt::Display for Global {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if Formatter::<fmt::Formatter>::is_valid_name(&self.name) {
            write!(f, "{}", std::str::from_utf8(&self.name).unwrap())
        } else {
            write!(
                f,
                "__FENV[\"{}\"]",
                Formatter::<fmt::Formatter>::escape_string(&self.name)
            )
        }
    }
}
