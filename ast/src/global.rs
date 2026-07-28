use std::fmt;

use crate::{
    IdentifierContext, LocalRw, SideEffects, Traverse, formatter::Formatter, is_valid_identifier_in,
};

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

    pub fn name(&self) -> &[u8] {
        &self.name
    }

    pub fn into_name(self) -> Vec<u8> {
        self.name
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

impl From<Vec<u8>> for Global {
    fn from(name: Vec<u8>) -> Self {
        Self::new(name)
    }
}

impl fmt::Display for Global {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if is_valid_identifier_in(&self.name, IdentifierContext::GlobalExpression) {
            write!(f, "{}", std::str::from_utf8(&self.name).unwrap())
        } else {
            write!(
                f,
                "getfenv(0)[\"{}\"]",
                Formatter::<fmt::Formatter>::escape_string(&self.name)
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Global, GlobalOrigin};

    #[test]
    fn vector_conversion_preserves_dynamic_name_access() {
        let expected = b"possibly_dynamic".to_vec();
        let global: Global = expected.clone().into();

        assert_eq!(global.origin(), GlobalOrigin::Dynamic);
        assert_eq!(global.name(), expected);
        assert_eq!(global.into_name(), expected);
    }

    #[test]
    fn contextual_global_is_direct_and_unspellable_global_uses_runtime_environment() {
        assert_eq!(Global::from("type").to_string(), "type");
        assert_eq!(
            Global::new(b"bad-name".to_vec()).to_string(),
            "getfenv(0)[\"bad-name\"]"
        );
    }
}
