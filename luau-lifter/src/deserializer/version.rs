#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct BytecodeVersion(u8);

impl BytecodeVersion {
    pub fn new(value: u8) -> Result<Self, String> {
        if (4..=12).contains(&value) {
            Ok(Self(value))
        } else {
            Err(format!("unsupported bytecode version: {value}"))
        }
    }

    pub const fn value(self) -> u8 {
        self.0
    }

    pub const fn has_feedback(self) -> bool {
        self.0 >= 11
    }

    pub const fn has_sized_prototypes(self) -> bool {
        self.0 >= 12
    }

    pub const fn has_cost(self) -> bool {
        self.0 >= 12
    }
}
