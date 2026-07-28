use crate::{RValue, Select};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResultDemand {
    Exact(usize),
    Open,
}

impl ResultDemand {
    pub const fn has_values(self) -> bool {
        !matches!(self, Self::Exact(0))
    }
}

impl Select {
    pub fn into_rvalue(self, demand: ResultDemand) -> RValue {
        assert!(
            demand.has_values(),
            "a discarded result producer must be emitted as a statement"
        );
        match demand {
            ResultDemand::Exact(0) => unreachable!(),
            ResultDemand::Exact(1) => RValue::Select(self),
            ResultDemand::Exact(_) | ResultDemand::Open => match self {
                Select::VarArg(vararg) => RValue::VarArg(vararg),
                Select::Call(call) => RValue::Call(call),
                Select::MethodCall(method_call) => RValue::MethodCall(method_call),
            },
        }
    }
}
