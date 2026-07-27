use std::fmt;

use crate::{LocalRw, Traverse, has_side_effects};

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Continue {}

has_side_effects!(Continue);

impl LocalRw for Continue {}

impl Traverse for Continue {}

impl fmt::Display for Continue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "continue")
    }
}
