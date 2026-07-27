use derive_more::From;
use enum_as_inner::EnumAsInner;
use std::fmt;

use crate::{
    LocalRw, Reduce, SideEffects, Traverse, Type, TypeSystem, formatter::Formatter,
    type_system::Infer,
};

#[derive(Debug, From, Clone, PartialEq, PartialOrd, EnumAsInner)]
pub enum Literal {
    Nil,
    Boolean(bool),
    Number(f64),
    Integer(i64),
    String(Vec<u8>),
    Vector(f32, f32, f32),
    VectorD(f64, f64, f64),
}

impl Reduce for Literal {
    fn reduce(self) -> crate::RValue {
        self.into()
    }

    fn reduce_condition(self) -> crate::RValue {
        Literal::Boolean(match self {
            Literal::Boolean(false) | Literal::Nil => false,
            Literal::Boolean(true)
            | Literal::Number(_)
            | Literal::Integer(_)
            | Literal::String(_)
            | Literal::Vector(..)
            | Literal::VectorD(..) => true,
        })
        .into()
    }
}

impl Infer for Literal {
    fn infer<'a: 'b, 'b>(&'a mut self, _: &mut TypeSystem<'b>) -> Type {
        match self {
            Literal::Nil => Type::Nil,
            Literal::Boolean(_) => Type::Boolean,
            Literal::Number(_) => Type::Number,
            Literal::Integer(_) => Type::Integer,
            Literal::String(_) => Type::String,
            Literal::Vector(..) | Literal::VectorD(..) => Type::Vector,
        }
    }
}

impl From<&str> for Literal {
    fn from(value: &str) -> Self {
        Self::String(value.into())
    }
}

impl LocalRw for Literal {}

impl SideEffects for Literal {}

impl Traverse for Literal {}

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Literal::Nil => write!(f, "nil"),
            Literal::Boolean(value) => write!(f, "{}", value),
            &Literal::Number(value) => {
                // TODO: this is a bit messy, just use `buffer.format` here and format_finite
                // in formatter.rs
                debug_assert!(value.is_finite());
                // TODO: fork ryu to remove ".0"
                let mut buffer = ryu::Buffer::new();
                let printed = buffer.format_finite(value);
                write!(f, "{}", printed.strip_suffix(".0").unwrap_or(printed))
            }
            Literal::Integer(value) if *value == i64::MIN => {
                write!(f, "(-9223372036854775807i - 1i)")
            }
            Literal::Integer(value) => write!(f, "{value}i"),
            Literal::String(value) => {
                write!(
                    f,
                    "\"{}\"",
                    Formatter::<fmt::Formatter>::escape_string(value)
                )
            }
            Literal::Vector(x, y, z) => write!(f, "Vector3.new({}, {}, {})", x, y, z),
            Literal::VectorD(x, y, z) => write!(f, "Vector3.new({}, {}, {})", x, y, z),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Literal;

    #[test]
    fn formats_exact_integer_literals() {
        assert_eq!(
            Literal::Integer(9_007_199_254_740_993).to_string(),
            "9007199254740993i"
        );
        assert_eq!(
            Literal::Integer(i64::MIN).to_string(),
            "(-9223372036854775807i - 1i)"
        );
    }

    #[test]
    fn formats_double_vector_literals() {
        assert_eq!(
            Literal::VectorD(1.25, 2.5, 3.75).to_string(),
            "Vector3.new(1.25, 2.5, 3.75)"
        );
    }
}
