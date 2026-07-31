use parking_lot::Mutex;
use triomphe::Arc;

use crate::{Block, LocalRw, Traverse, formatter::Formatter, has_side_effects};
use std::fmt;

/// A bare `do ... end` block. It introduces a lexical scope without any
/// control flow, so locals declared inside it release their registers at
/// `end`.
#[derive(Debug, Clone)]
pub struct Do {
    pub block: Arc<Mutex<Block>>,
}

impl PartialEq for Do {
    fn eq(&self, _other: &Self) -> bool {
        // TODO: compare block
        false
    }
}

// The body is opaque to the effect analysis that runs over statement lists,
// so a `do` block is conservatively treated the way loops are.
has_side_effects!(Do);

impl Do {
    pub fn new(block: Block) -> Self {
        Self {
            block: Arc::new(block.into()),
        }
    }
}

impl Traverse for Do {}

impl LocalRw for Do {}

impl fmt::Display for Do {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        Formatter {
            indentation_level: 0,
            indentation_mode: Default::default(),
            output: f,
        }
        .format_do(self)
    }
}
