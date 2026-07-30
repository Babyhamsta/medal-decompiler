use std::{
    any::Any,
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecompilePhase {
    Deserialize,
    Lift,
    Ssa,
    Structure,
    SsaDestruction,
    Restructure,
    AstRecovery,
    Declaration,
    Link,
    Validate,
    Format,
    Unknown,
}

impl DecompilePhase {
    const fn label(self) -> &'static str {
        match self {
            Self::Deserialize => "deserialize",
            Self::Lift => "lift",
            Self::Ssa => "ssa",
            Self::Structure => "structure",
            Self::SsaDestruction => "ssa-destruction",
            Self::Restructure => "restructure",
            Self::AstRecovery => "ast-recovery",
            Self::Declaration => "declaration",
            Self::Link => "link",
            Self::Validate => "validate",
            Self::Format => "format",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct DecompileError {
    pub phase: DecompilePhase,
    pub function_id: Option<usize>,
    pub instruction: Option<usize>,
    pub invariant: &'static str,
    pub detail: String,
}

impl DecompileError {
    pub fn new(
        phase: DecompilePhase,
        function_id: Option<usize>,
        instruction: Option<usize>,
        invariant: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            phase,
            function_id,
            instruction,
            invariant,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for DecompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "[{}]", self.phase.label())?;
        if let Some(function_id) = self.function_id {
            write!(formatter, " function={function_id}")?;
        }
        if let Some(instruction) = self.instruction {
            write!(formatter, " instruction={instruction}")?;
        }
        write!(formatter, " invariant={}: {}", self.invariant, self.detail)
    }
}

impl std::error::Error for DecompileError {}

pub(crate) fn catch_phase<T>(
    phase: DecompilePhase,
    function_id: Option<usize>,
    instruction: Option<usize>,
    operation: impl FnOnce() -> T,
) -> Result<T, DecompileError> {
    catch_unwind(AssertUnwindSafe(operation)).map_err(|payload| {
        DecompileError::new(
            phase,
            function_id,
            instruction,
            "panic-free decompilation",
            panic_message(payload),
        )
    })
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else {
        "decompiler panicked with an unknown error".to_owned()
    }
}
