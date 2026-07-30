use smallvec::{smallvec};
use crate::{LocalRefs,LocalRefsMut,RValueRefs,RValueRefsMut};
use parking_lot::Mutex;
use triomphe::Arc;

use crate::{Block, LocalRw, RValue, RcLocal, Traverse, formatter::Formatter, has_side_effects};
use std::fmt;

#[derive(Debug, Clone)]
pub struct While {
    pub condition: RValue,
    pub block: Arc<Mutex<Block>>,
}

impl PartialEq for While {
    fn eq(&self, _other: &Self) -> bool {
        // TODO: compare block
        false
    }
}

has_side_effects!(While);

impl While {
    pub fn new(condition: RValue, block: Block) -> Self {
        Self {
            condition,
            block: Arc::new(block.into()),
        }
    }
}

impl Traverse for While {
    fn rvalues_mut(&mut self) -> RValueRefsMut<'_> {
        smallvec![&mut self.condition]
    }

    fn rvalues(&self) -> RValueRefs<'_> {
        smallvec![&self.condition]
    }
}

impl LocalRw for While {
    fn values_read(&self) -> LocalRefs<'_> {
        self.condition.values_read()
    }

    fn values_read_mut(&mut self) -> LocalRefsMut<'_> {
        self.condition.values_read_mut()
    }
}

impl fmt::Display for While {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        Formatter {
            indentation_level: 0,
            indentation_mode: Default::default(),
            output: f,
        }
        .format_while(self)
    }
}
