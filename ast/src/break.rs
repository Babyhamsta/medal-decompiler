use std::fmt;

use crate::{LocalRw, Traverse, has_side_effects};

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Break {}

has_side_effects!(Break);

impl LocalRw for Break {}

impl Traverse for Break {}

impl fmt::Display for Break {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "break")
    }
}
