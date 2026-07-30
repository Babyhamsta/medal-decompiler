use smallvec::{smallvec};
use crate::{LocalRefs,LocalRefsMut,RValueRefs,RValueRefsMut};
use parking_lot::Mutex;
use triomphe::Arc;

use crate::{LocalRw, RcLocal, SideEffects, Traverse, formatter::Formatter};

use super::{Block, RValue};

use std::fmt;

#[derive(Debug, Clone)]
pub struct If {
    pub condition: RValue,
    pub then_block: Arc<Mutex<Block>>,
    pub else_block: Arc<Mutex<Block>>,
}

impl PartialEq for If {
    fn eq(&self, _other: &Self) -> bool {
        // TODO: compare block
        false
    }
}

impl If {
    pub fn new(condition: RValue, then_block: Block, else_block: Block) -> Self {
        Self {
            condition,
            then_block: Arc::new(then_block.into()),
            else_block: Arc::new(else_block.into()),
        }
    }
}

impl Traverse for If {
    fn rvalues_mut(&mut self) -> RValueRefsMut<'_> {
        smallvec![&mut self.condition]
    }

    fn rvalues(&self) -> RValueRefs<'_> {
        smallvec![&self.condition]
    }
}

impl SideEffects for If {
    // TODO: side effects for blocks
    fn has_side_effects(&self) -> bool {
        true
    }
}

impl LocalRw for If {
    fn values_read(&self) -> LocalRefs<'_> {
        self.condition.values_read()
    }

    fn values_read_mut(&mut self) -> LocalRefsMut<'_> {
        self.condition.values_read_mut()
    }
}

impl fmt::Display for If {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        Formatter {
            indentation_level: 0,
            indentation_mode: Default::default(),
            output: f,
        }
        .format_if(self)
    }
}
